//! The child process: serves the instances its host-side group sends it,
//! each on its own thread.

use super::os::{Memory, Process, Signal};
use super::shm::*;
use crate::abi::{EXT_LATENCY, LatencyApi, ParamInfoC, PluginApi, RenderApi};
use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

/// Serves instances until the host asks to quit or the host process ends.
///
/// # Safety
/// `command_line` must be null or a null-terminated string.
pub unsafe fn run(api: &'static PluginApi, command_line: *const c_char) {
    if command_line.is_null() {
        return;
    }
    // A crashing plugin must end this process at once: no error dialog, no
    // Windows Error Reporting delay (the host restarts us).
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
        }
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
    }
    // "group <group id> <host pid>"
    let line = unsafe { CStr::from_ptr(command_line) }.to_string_lossy().into_owned();
    let mut args = line.split_whitespace();
    let (Some("group"), Some(group), Some(host_pid)) = (args.next(), args.next(), args.next().and_then(|p| p.parse::<u32>().ok())) else {
        return;
    };
    // Development bundles reload the plugin's code here, in the child.
    let api = crate::reload::dev(api);
    let (memory, command, ready) = group_names(group);
    let (Some(memory), Some(command), Some(ready), Some(host)) =
        (Memory::open(&memory, size_of::<Control>()), Signal::open(&command), Signal::open(&ready), Process::open(host_pid))
    else {
        return;
    };
    let host = Arc::new(host);
    let control = unsafe { &*(memory.ptr() as *const Control) };
    control.ready.store(1, SeqCst);
    ready.set();
    loop {
        if !command.wait(None, Some(&host)) || control.quit.load(SeqCst) != 0 {
            break;
        }
        let head = control.head.load(SeqCst);
        let mut tail = control.tail.load(SeqCst);
        while tail != head {
            let bytes = unsafe { *control.commands[tail as usize % MAX_COMMANDS].get() };
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(COMMAND_LEN);
            let id = String::from_utf8_lossy(&bytes[..end]).into_owned();
            let host = host.clone();
            let _ = std::thread::Builder::new().name(format!("ferritron-instance-{id}")).spawn(move || serve_instance(api, &id, &host));
            tail = tail.wrapping_add(1);
        }
        control.tail.store(tail, SeqCst);
    }
}

/// Passes the host's offline flag to the plugin when it has changed (`applied` is what the plugin
/// has). Called before each block, so a block never sees the flag change in the middle.
///
/// # Safety
/// `core` must be a live instance of the plugin `render` belongs to.
unsafe fn sync_offline(shared: &Shared, render: Option<&RenderApi>, core: *mut c_void, applied: &mut bool) {
    let offline = shared.offline.load(SeqCst) != 0;
    if offline != *applied {
        *applied = offline;
        if let Some(render) = render {
            unsafe { (render.set_offline)(core, offline) };
        }
    }
}

fn serve_instance(api: &'static PluginApi, id: &str, host: &Process) {
    // This thread processes the instance's audio for as long as it lives; the host's audio thread
    // waits for it. Only this thread (not the `poller`) gets the audio priority.
    crate::parallel::raise_audio_priority();
    let (memory, request, done, ready) = names(id);
    if let (Some(memory), Some(request), Some(done), Some(ready)) =
        (Memory::open(&memory, size_of::<Shared>()), Signal::open(&request), Signal::open(&done), Signal::open(&ready))
    {
        unsafe { serve(api, id, &mut *(memory.ptr() as *mut Shared), &request, &done, &ready, host) };
    }
}

unsafe fn serve(api: &'static PluginApi, id: &str, shared: &mut Shared, request: &Signal, done: &Signal, ready: &Signal, host: &Process) {
    let core = unsafe { (api.create)() };
    if core.is_null() {
        return;
    }
    let latency = unsafe { ((api.get_extension)(EXT_LATENCY.as_ptr()) as *const LatencyApi).as_ref() };
    let ext = |name: &std::ffi::CStr| unsafe { (api.get_extension)(name.as_ptr()) };
    let transport_api = unsafe { (ext(crate::abi::EXT_TRANSPORT) as *const crate::abi::TransportApi).as_ref() };
    let sidechain_api = unsafe { (ext(crate::abi::EXT_SIDECHAIN) as *const crate::abi::SidechainApi).as_ref() };
    let layout_api = unsafe { (ext(crate::abi::EXT_LAYOUT) as *const crate::abi::LayoutApi).as_ref() };
    let main_api = unsafe { (ext(crate::abi::EXT_MAIN) as *const crate::abi::MainApi).as_ref() };
    let render_api = unsafe { (ext(crate::abi::EXT_RENDER) as *const RenderApi).as_ref() };
    let mut offline_applied = false;
    let count = (unsafe { (api.param_count)() } as usize).min(MAX_PARAMS);
    let ids: Vec<u32> = (0..count as u32)
        .map(|i| {
            let mut info = ParamInfoC::empty();
            unsafe { (api.param_info)(i, &mut info) };
            info.id
        })
        .collect();
    let mut applied = vec![f64::NAN; count];
    let mut generation = 0;
    let mut audio: Option<AudioRegion> = None;
    let mut audio_generation = 0;
    // The child's "main thread" for this instance: takes the plugin's requests (and runs its
    // `on_notify` handler, which must not run on the processing thread) and passes the requests
    // to the host side.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = main_api.map(|main| {
        let (stop, core, shared) = (stop.clone(), core as usize, shared as *const Shared as usize);
        let latency = latency.map(|l| l.get);
        let take = main.take_requests;
        std::thread::spawn(move || {
            // SAFETY: `core` and the shared memory outlive this thread (joined before destroy).
            let shared = unsafe { &*(shared as *const Shared) };
            while !stop.load(SeqCst) {
                let bits = unsafe { take(core as *mut c_void) };
                if bits & crate::abi::REQUEST_LATENCY != 0 {
                    shared.latency.store(latency.map_or(0, |get| unsafe { get(core as *mut c_void) }), SeqCst);
                }
                if bits != 0 {
                    shared.requests.fetch_or(bits, SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        })
    });
    shared.ready.store(1, SeqCst);
    ready.set();

    loop {
        // Woken for a block, or the host process is gone.
        if !request.wait(None, Some(host)) || shared.quit.load(SeqCst) != 0 {
            break;
        }
        let g = shared.setup_generation.load(SeqCst);
        if g != generation {
            generation = g;
            // The client made a new audio region for this activation.
            let ag = shared.audio_generation.load(SeqCst);
            if ag != audio_generation {
                audio_generation = ag;
                let (frames, channels) = (shared.audio_frames.load(SeqCst) as usize, shared.audio_channels.load(SeqCst) as usize);
                audio = Memory::open(&audio_name(id, ag), audio_len(frames, channels)).map(|memory| AudioRegion { memory, frames, channels });
            }
            let rate = f64::from_bits(shared.sample_rate.load(SeqCst));
            if let Some(layout) = layout_api {
                let n = (shared.layout_len as usize).min(MAX_CHANNELS);
                unsafe { (layout.set)(core, shared.layout.as_ptr(), n as u32) };
            }
            unsafe { (api.activate)(core, rate, shared.max_block.load(SeqCst)) };
            shared.latency.store(latency.map_or(0, |l| unsafe { (l.get)(core) }), SeqCst);
        }
        unsafe { sync_offline(shared, render_api, core, &mut offline_applied) };
        for i in 0..count {
            let value = f64::from_bits(shared.params[i].load(SeqCst));
            if value != applied[i] {
                unsafe { (api.set_param)(core, ids[i], value) };
                applied[i] = value;
            }
        }
        let room = audio.as_ref().map_or((0, 0), |a| (a.frames, a.channels));
        let frames = (shared.frames as usize).min(room.0);
        if let Some(audio) = audio.as_ref().filter(|_| frames > 0) {
            let channels = (shared.channels as usize).min(room.1);
            let events = (shared.num_events as usize).min(MAX_EVENTS);
            let mut ptrs = [std::ptr::null_mut::<f32>(); MAX_CHANNELS];
            for (ch, ptr) in ptrs.iter_mut().enumerate().take(channels) {
                *ptr = audio.main(ch);
            }
            if let Some(t) = transport_api {
                unsafe { (t.set)(core, &shared.transport) };
            }
            if let Some(sc) = sidechain_api {
                let n = (shared.sc_channels as usize).min(room.1);
                let mut sc_ptrs = [std::ptr::null::<f32>(); MAX_CHANNELS];
                for (ch, ptr) in sc_ptrs.iter_mut().enumerate().take(n) {
                    *ptr = audio.sidechain(ch);
                }
                unsafe { (sc.set)(core, if n == 0 { std::ptr::null() } else { sc_ptrs.as_ptr() }, n as u32) };
            }
            // The plugin's output events go back to the client in the shared region (see `Shared::out`).
            let ok = unsafe {
                {
                    let mut d = crate::abi::ProcessData::new(ptrs.as_ptr(), channels as u32, frames as u32, shared.events.as_ptr(), events as u32, std::ptr::null());
                    d.out_events = shared.out.as_mut_ptr();
                    d.out_capacity = MAX_OUT as u32;
                    d.out_count = &mut shared.out_count;
                    (api.process)(core, &d)
                }
            };
            // Events changed parameters inside the block; remember the results.
            for i in 0..count {
                applied[i] = unsafe { (api.get_param)(core, ids[i]) };
            }
            shared.ok = ok as u32;
        }
        done.set();
    }
    stop.store(true, SeqCst);
    if let Some(poller) = poller {
        let _ = poller.join();
    }
    unsafe { (api.destroy)(core) };
    done.set(); // the host waits for the instance to be gone
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn record(_core: *mut c_void, offline: bool) {
        CALLS.fetch_add(1, SeqCst);
        LAST.store(offline as u32, SeqCst);
    }

    /// The child tells the plugin about the host's offline flag once per change, before a block.
    #[test]
    fn the_child_passes_the_offline_flag_to_the_plugin_on_change_only() {
        let shared: Box<Shared> = Box::new(unsafe { std::mem::zeroed() });
        let render = RenderApi { set_offline: record };
        let core = std::ptr::null_mut();
        let mut applied = false;
        let calls = || CALLS.load(SeqCst);

        unsafe { sync_offline(&shared, Some(&render), core, &mut applied) };
        assert_eq!(calls(), 0, "online and unchanged: nothing is passed");

        shared.offline.store(1, SeqCst);
        unsafe { sync_offline(&shared, Some(&render), core, &mut applied) };
        assert_eq!((calls(), LAST.load(SeqCst)), (1, 1), "the plugin now renders offline");

        unsafe { sync_offline(&shared, Some(&render), core, &mut applied) };
        assert_eq!(calls(), 1, "still offline: nothing is passed again");

        shared.offline.store(0, SeqCst);
        unsafe { sync_offline(&shared, Some(&render), core, &mut applied) };
        assert_eq!((calls(), LAST.load(SeqCst)), (2, 0), "back online");
    }
}
