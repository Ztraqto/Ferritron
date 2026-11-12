//! A minimal VST 2 host: loads the plugin through `VSTPluginMain` and drives its dispatcher
//! (run with `--features vst2-test` and `VST2_SDK_DIR` set).

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

type Dispatcher = unsafe extern "C" fn(*mut Effect, i32, i32, isize, *mut c_void, f32) -> isize;
type Process = unsafe extern "C" fn(*mut Effect, *mut *mut f32, *mut *mut f32, i32);

#[repr(C)]
struct Effect {
    magic: i32,
    dispatcher: Dispatcher,
    process: Process,
    set_parameter: unsafe extern "C" fn(*mut Effect, i32, f32),
    get_parameter: unsafe extern "C" fn(*mut Effect, i32) -> f32,
    num_programs: i32,
    num_params: i32,
    num_inputs: i32,
    num_outputs: i32,
    flags: i32,
    resvd1: isize,
    resvd2: isize,
    initial_delay: i32,
    real_qualities: i32,
    off_qualities: i32,
    io_ratio: f32,
    object: *mut c_void,
    user: *mut c_void,
    unique_id: i32,
    version: i32,
    process_replacing: Process,
}

#[repr(C)]
struct TimeInfo {
    sample_pos: f64,
    sample_rate: f64,
    nanos: f64,
    ppq_pos: f64,
    tempo: f64,
    bar_start: f64,
    cycle_start: f64,
    cycle_end: f64,
    sig_num: i32,
    sig_den: i32,
    smpte_offset: i32,
    smpte_rate: i32,
    samples_to_clock: i32,
    flags: i32,
}

#[repr(C)]
struct MidiEvent {
    kind: i32,
    byte_size: i32,
    delta_frames: i32,
    flags: i32,
    note_length: i32,
    note_offset: i32,
    midi_data: [u8; 4],
    detune: i8,
    off_velocity: i8,
    reserved: [i8; 2],
}

#[repr(C)]
struct Events {
    num: i32,
    reserved: isize,
    events: [*mut MidiEvent; 2],
}

unsafe extern "C" {
    fn VSTPluginMain(master: *mut c_void) -> *mut Effect;
}

static CALLS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static TIME: Mutex<usize> = Mutex::new(0);

unsafe extern "C" fn master(_: *mut Effect, opcode: i32, _: i32, _: isize, _: *mut c_void, _: f32) -> isize {
    CALLS.lock().unwrap().push(opcode);
    match opcode {
        1 => 2400,
        7 => *TIME.lock().unwrap() as isize,
        _ => 0,
    }
}

fn text_of(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap().to_string()
}

#[test]
fn the_vst2_shim_runs_a_block_with_sidechain_midi_and_transport() {
    // flags: 1 << 1 playing, 1 << 10 tempo valid.
    let time = Box::leak(Box::new(TimeInfo {
        sample_pos: 0.0,
        sample_rate: 48_000.0,
        nanos: 0.0,
        ppq_pos: 0.0,
        tempo: 240.0,
        bar_start: 0.0,
        cycle_start: 0.0,
        cycle_end: 0.0,
        sig_num: 4,
        sig_den: 4,
        smpte_offset: 0,
        smpte_rate: 0,
        samples_to_clock: 0,
        flags: (1 << 1) | (1 << 10),
    }));
    *TIME.lock().unwrap() = time as *mut TimeInfo as usize;
    let ep = unsafe { VSTPluginMain(master as *mut c_void) };
    assert!(!ep.is_null());
    let e = unsafe { &mut *ep };
    assert_eq!(e.magic, 0x5673_7450, "'VstP'");
    assert_eq!(e.unique_id, i32::from_be_bytes(*b"MbTs"));
    let call = |opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32| unsafe {
        ((*ep).dispatcher)(ep, opcode, index, value, ptr, opt)
    };
    call(0, 0, 0, std::ptr::null_mut(), 0.0);
    let mut name = [0 as c_char; 64];
    call(45, 0, 0, name.as_mut_ptr().cast(), 0.0);
    assert_eq!(text_of(&name), "Ferritron Multiband Test");
    // A stereo main bus plus a stereo sidechain.
    assert_eq!((e.num_inputs, e.num_outputs, e.num_params), (4, 2, 7));
    call(10, 0, 0, std::ptr::null_mut(), 48_000.0);
    call(11, 0, 64, std::ptr::null_mut(), 0.0);
    call(12, 0, 1, std::ptr::null_mut(), 0.0);

    // output_gain (index 2) at the middle of -36..36 is 0 dB; the text goes through the plugin's display.
    unsafe { (e.set_parameter)(ep, 2, 0.5) };
    let mut text = [0 as c_char; 64];
    call(7, 2, 0, text.as_mut_ptr().cast(), 0.0);
    assert_eq!(text_of(&text), "0.00");
    // `level` (index 4) writes its own text.
    unsafe { (e.set_parameter)(ep, 4, 0.0) };
    call(7, 4, 0, text.as_mut_ptr().cast(), 0.0);
    assert_eq!(text_of(&text), "-INF dB");
    unsafe { (e.set_parameter)(ep, 4, 1.0) };

    // CC 7 = 64 at the start of the block.
    let mut cc = MidiEvent {
        kind: 1,
        byte_size: 32,
        delta_frames: 0,
        flags: 0,
        note_length: 0,
        note_offset: 0,
        midi_data: [0xB0, 7, 64, 0],
        detune: 0,
        off_velocity: 0,
        reserved: [0; 2],
    };
    let mut events = Events { num: 1, reserved: 0, events: [&mut cc, std::ptr::null_mut()] };
    call(25, 0, 0, (&raw mut events).cast(), 0.0);
    let frames = 8usize;
    let (mut in_l, mut in_r) = (vec![1.0f32; frames], vec![1.0f32; frames]);
    let (mut sc_l, mut sc_r) = (vec![0.0f32; frames], vec![0.25f32; frames]);
    let (mut out_l, mut out_r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let mut inputs = [in_l.as_mut_ptr(), in_r.as_mut_ptr(), sc_l.as_mut_ptr(), sc_r.as_mut_ptr()];
    let mut outputs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
    unsafe { (e.process_replacing)(ep, inputs.as_mut_ptr(), outputs.as_mut_ptr(), frames as i32) };
    // The same numbers as the LV2 test: tempo 240 (gain x2), CC 7 = 64, sidechain channel 1 added to channel 0.
    let volume = 64.0 / 127.0;
    assert!((out_l[0] - (2.0 * volume + 0.25)).abs() < 1e-5, "{out_l:?}");
    assert!((out_r[0] - 2.0 * volume).abs() < 1e-5, "{out_r:?}");

    // State round trip through chunks.
    let mut chunk: *mut c_void = std::ptr::null_mut();
    let size = call(23, 0, 0, (&raw mut chunk).cast(), 0.0);
    assert!(size > 0 && !chunk.is_null());
    assert_eq!(call(24, 0, size, chunk, 0.0), 1);

    // A latency change: linear phase (mode, index 5) -> a request, an idle call, ioChanged and the new delay.
    CALLS.lock().unwrap().clear();
    unsafe { (e.set_parameter)(ep, 5, 1.0) };
    unsafe { (e.process_replacing)(ep, inputs.as_mut_ptr(), outputs.as_mut_ptr(), frames as i32) };
    assert!(CALLS.lock().unwrap().contains(&14), "the plugin asked for an idle call (audioMasterNeedIdle)");
    call(19, 0, 0, std::ptr::null_mut(), 0.0);
    assert!(CALLS.lock().unwrap().contains(&13), "ioChanged");
    assert_eq!(e.initial_delay, 5120);
    call(1, 0, 0, std::ptr::null_mut(), 0.0);
}
