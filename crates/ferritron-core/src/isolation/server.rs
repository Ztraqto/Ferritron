//! The child process: serves the instances its host-side group sends it,
//! each on its own thread.

use super::os::{Memory, Process, Signal};
use super::shm::*;
use crate::abi::{EXT_LATENCY, LatencyApi, ParamInfoC, PluginApi};
use std::ffi::{CStr, c_char};
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

fn serve_instance(api: &'static PluginApi, id: &str, host: &Process) {
    let (memory, request, done, ready) = names(id);
    if let (Some(memory), Some(request), Some(done), Some(ready)) =
        (Memory::open(&memory, size_of::<Shared>()), Signal::open(&request), Signal::open(&done), Signal::open(&ready))
    {
        unsafe { serve(api, &mut *(memory.ptr() as *mut Shared), &request, &done, &ready, host) };
    }
}

unsafe fn serve(api: &'static PluginApi, shared: &mut Shared, request: &Signal, done: &Signal, ready: &Signal, host: &Process) {
    let core = unsafe { (api.create)() };
    if core.is_null() {
        return;
    }
    let latency = unsafe { ((api.get_extension)(EXT_LATENCY.as_ptr()) as *const LatencyApi).as_ref() };
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
            let rate = f64::from_bits(shared.sample_rate.load(SeqCst));
            unsafe { (api.activate)(core, rate, shared.max_block.load(SeqCst)) };
            shared.latency.store(latency.map_or(0, |l| unsafe { (l.get)(core) }), SeqCst);
        }
        for i in 0..count {
            let value = f64::from_bits(shared.params[i].load(SeqCst));
            if value != applied[i] {
                unsafe { (api.set_param)(core, ids[i], value) };
                applied[i] = value;
            }
        }
        let frames = (shared.frames as usize).min(MAX_FRAMES);
        if frames > 0 {
            let channels = (shared.channels as usize).min(MAX_CHANNELS);
            let events = (shared.num_events as usize).min(MAX_EVENTS);
            let mut ptrs = [std::ptr::null_mut::<f32>(); MAX_CHANNELS];
            for (ch, ptr) in ptrs.iter_mut().enumerate().take(channels) {
                *ptr = shared.audio[ch].as_mut_ptr();
            }
            let ok = unsafe {
                (api.process)(core, ptrs.as_ptr(), channels as u32, frames as u32, shared.events.as_ptr(), events as u32, std::ptr::null())
            };
            // Events changed parameters inside the block; remember the results.
            for i in 0..count {
                applied[i] = unsafe { (api.get_param)(core, ids[i]) };
            }
            shared.ok = ok as u32;
        }
        done.set();
    }
    unsafe { (api.destroy)(core) };
    done.set(); // the host waits for the instance to be gone
}
