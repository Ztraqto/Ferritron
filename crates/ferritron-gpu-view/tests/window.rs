//! The Windows view for real: a child window of a top-level window, a wgpu surface, and the
//! view's timer. Counts the frames an editor is asked to draw, so pacing is measured, not assumed.
//! Needs a desktop session and a GPU; the tests take turns (one window at a time).
#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ferritron_core::__private::{Localized, PluginMeta};
use ferritron_core::abi::ViewHost;
use ferritron_core::isolation::{Isolation, ProcessOptions};
use ferritron_core::{AudioConfig, Context, ParamCell, ParamInfo, ParamSet, PluginInfo, PluginKind, Process};
use ferritron_gpu_view::wgpu;
use ferritron_gpu_view::{Frame, Gpu, GpuEditor, Input, Modifiers, Ui, view_api};

// ---- a plugin without parameters, written by hand (no macro) --------------------------------

#[derive(Default)]
struct NoParams;

impl ParamSet for NoParams {
    fn infos() -> &'static [ParamInfo] {
        &[]
    }
    fn cell(&self, _index: usize) -> &ParamCell {
        panic!("no parameters")
    }
    fn index_of(_id: u32) -> Option<usize> {
        None
    }
}

#[derive(Default)]
struct Plug;

impl Process for Plug {
    type Params = NoParams;
    fn process(&mut self, _ctx: &mut Context<'_, NoParams>) {}
}

static INFO: PluginInfo = PluginInfo {
    id: "test.pacing",
    name: "Pacing",
    vendor: "Ztraqto",
    version: "1.0.0",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [7; 16],
    isolation: Isolation::Guarded,
    process: ProcessOptions { per_instance: false, pipeline: false },
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig::DEFAULT,
};

impl PluginMeta for Plug {
    const INFO: &'static PluginInfo = &INFO;
    fn localized() -> &'static Localized {
        static L: OnceLock<Localized> = OnceLock::new();
        L.get_or_init(|| Localized::new("test.pacing", &[], &[("en", "")]))
    }
}

// ---- an editor that records when it is drawn -------------------------------------------------

static FRAMES: Mutex<Vec<(Instant, Duration, u64)>> = Mutex::new(Vec::new());
static WANT: Mutex<Option<Duration>> = Mutex::new(None);
static INPUTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Lock the pointer on a left press, unlock on release.
static LOCKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Ask for a redraw after every input.
static REDRAW_ON_INPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DRAWS: AtomicU64 = AtomicU64::new(0);

struct Recorder;

impl GpuEditor for Recorder {
    fn new(_gpu: &Gpu, _ui: &Ui) -> Self {
        Recorder
    }

    fn render(&mut self, gpu: &Gpu, frame: &Frame, _ui: &Ui) {
        FRAMES.lock().unwrap().push((Instant::now(), frame.delta, frame.index));
        DRAWS.fetch_add(1, Ordering::SeqCst);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &frame.target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.1, g: 0.1, b: 0.12, a: 1.0 }), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        }));
        gpu.queue.submit([encoder.finish()]);
    }

    fn next_frame(&self) -> Option<Duration> {
        *WANT.lock().unwrap()
    }

    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        INPUTS.lock().unwrap().push(format!("{event:?}"));
        if LOCKING.load(Ordering::SeqCst) {
            match event {
                Input::Press { .. } => ui.lock_pointer(true),
                Input::Release { .. } => ui.lock_pointer(false),
                _ => {}
            }
        }
        REDRAW_ON_INPUT.load(Ordering::SeqCst)
    }
}

// ---- Win32 plumbing ----------------------------------------------------------------------------

type Hwnd = *mut c_void;

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w: usize,
    l: isize,
    time: u32,
    pt: [i32; 2],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: *mut c_void, inst: *mut c_void, param: *mut c_void) -> Hwnd;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> i32;
    fn SendMessageW(hwnd: Hwnd, msg: u32, w: usize, l: isize) -> isize;
    fn ReleaseCapture() -> i32;
    fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
    fn GetCapture() -> Hwnd;
    fn GetCursorPos(p: *mut [i32; 2]) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Runs the message loop of this thread for `d`.
fn pump(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        let mut msg: Msg = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, 1) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

unsafe extern "C" fn get(_ctx: *mut c_void, _id: u32) -> f64 {
    0.0
}
unsafe extern "C" fn gesture(_ctx: *mut c_void, _id: u32) {}
unsafe extern "C" fn set(_ctx: *mut c_void, _id: u32, _v: f64) {}

/// An open editor in a real window.
struct Opened {
    parent: Hwnd,
    view: *mut c_void,
    api: ferritron_core::abi::ViewApi,
}

impl Opened {
    fn child(&self) -> Hwnd {
        unsafe { GetWindow(self.parent, 5) } // GW_CHILD
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.view);
            DestroyWindow(self.parent);
        }
        pump(Duration::from_millis(20));
    }
}

/// Opens the recorder editor with `want` as its `next_frame`; one test at a time.
fn open(want: Option<Duration>) -> Opened {
    FRAMES.lock().unwrap().clear();
    INPUTS.lock().unwrap().clear();
    *WANT.lock().unwrap() = want;
    let class = wide("STATIC");
    let parent = unsafe { CreateWindowExW(0x0000_0088, class.as_ptr(), std::ptr::null(), 0x9000_0000, 40, 40, 240, 140, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert!(!parent.is_null(), "the parent window");
    let api = view_api::<Plug, Recorder>((240, 140), (100, 60), (800, 600));
    let host = ViewHost { ctx: std::ptr::null_mut(), get_param: get, begin_edit: gesture, perform_edit: set, end_edit: gesture, request_resize: None };
    let view = unsafe { (api.open)(parent, &host, 240, 140, 1.0) };
    assert!(!view.is_null(), "the view opens (needs a GPU adapter that can present)");
    Opened { parent, view, api }
}

static TURN: Mutex<()> = Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn count() -> usize {
    FRAMES.lock().unwrap().len()
}

#[test]
fn a_still_editor_is_not_drawn_again() {
    let _t = turn();
    let o = open(None);
    pump(Duration::from_millis(400));
    let settled = count();
    assert!(settled >= 1, "the first frame is drawn when the window opens");
    assert!(settled <= 4, "opening draws a few frames at most: {settled}");
    pump(Duration::from_millis(2500));
    assert_eq!(count(), settled, "no frame in 2.5 s while nothing changed and nothing is asked for");
    drop(o);
}

#[test]
fn thirty_hertz_is_paced_in_a_real_window() {
    let _t = turn();
    let o = open(Some(Duration::from_secs_f64(1.0 / 30.0)));
    pump(Duration::from_millis(300)); // settle
    let start_index = count();
    let t0 = Instant::now();
    pump(Duration::from_secs(3));
    let elapsed = t0.elapsed().as_secs_f64();
    let frames: Vec<(Instant, Duration, u64)> = FRAMES.lock().unwrap()[start_index..].to_vec();
    let rate = frames.len() as f64 / elapsed;
    let gaps: Vec<f64> = frames.windows(2).map(|w| (w[1].0 - w[0].0).as_secs_f64() * 1000.0).collect();
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    let max = gaps.iter().cloned().fold(0.0, f64::max);
    let min = gaps.iter().cloned().fold(f64::MAX, f64::min);
    eprintln!("30 Hz: {rate:.2} frames/s, gaps mean {mean:.2} ms, min {min:.2}, max {max:.2}");
    assert!((rate - 30.0).abs() < 1.5, "{rate} frames per second");
    assert!(max < 45.0 && min > 25.0, "gaps between {min} and {max} ms");
    // Frame::delta is what the view measured between frames
    let (_, delta, index) = frames[frames.len() / 2];
    assert!(delta > Duration::from_millis(25) && delta < Duration::from_millis(45), "{delta:?}");
    assert!(index > 10);
    drop(o);
}

#[test]
fn zero_draws_once_per_display_refresh() {
    let _t = turn();
    let hz = ferritron_gpu_view::display_refresh_hz();
    eprintln!("the compositor reports {hz:?} Hz");
    let hz = hz.expect("DwmGetCompositionTimingInfo works on this machine");
    assert!((24.0..=500.0).contains(&hz), "{hz}");
    let o = open(Some(Duration::ZERO));
    pump(Duration::from_millis(300));
    let start_index = count();
    let t0 = Instant::now();
    pump(Duration::from_secs(2));
    let rate = (count() - start_index) as f64 / t0.elapsed().as_secs_f64();
    eprintln!("ZERO: {rate:.1} frames/s at {hz} Hz");
    // Never faster than the display; as fast as the system timer lets the view check
    assert!(rate <= hz * 1.08, "{rate} frames/s on a {hz} Hz display");
    assert!(rate >= hz.min(60.0) * 0.8, "{rate} frames/s on a {hz} Hz display");
    drop(o);
}

#[test]
fn input_asks_for_a_frame_only_when_the_editor_says_so_and_the_pointer_reaches_the_editor() {
    let _t = turn();
    let o = open(None);
    pump(Duration::from_millis(300));
    let child = o.child();
    assert!(!child.is_null());
    // a click at (10, 10), then a move, as Windows would deliver them
    let pos = |x: i32, y: i32| ((y << 16) | x) as isize;
    unsafe {
        PostMessageW(child, 0x0201, 1, pos(10, 10)); // WM_LBUTTONDOWN, MK_LBUTTON
        PostMessageW(child, 0x0200, 1, pos(20, 12)); // WM_MOUSEMOVE
        PostMessageW(child, 0x0202, 0, pos(20, 12)); // WM_LBUTTONUP
    }
    pump(Duration::from_millis(200));
    let log = INPUTS.lock().unwrap().join("\n");
    assert!(log.contains("Press") && log.contains("Move") && log.contains("Release"), "{log}");
    let _ = Modifiers::default();
    drop(o);
}

#[test]
#[ignore = "moves the real mouse pointer: run alone with `cargo test -p ferritron-gpu-view -- --ignored`"]
fn the_pointer_lock_reports_relative_moves_and_gives_the_pointer_back() {
    let _t = turn();
    LOCKING.store(true, Ordering::SeqCst);
    let o = open(None);
    pump(Duration::from_millis(300));
    let child = o.child();
    let mut before = [0i32; 2];
    unsafe { GetCursorPos(&mut before) };
    let pos = |x: i32, y: i32| ((y << 16) | x) as isize;
    // the editor is 240 x 140 px: the lock point is its centre (120, 70)
    unsafe { PostMessageW(child, 0x0201, 1, pos(30, 30)) }; // press: the editor locks the pointer
    pump(Duration::from_millis(100));
    unsafe {
        PostMessageW(child, 0x0200, 1, pos(125, 73)); // 5 right, 3 down of the lock point
        PostMessageW(child, 0x0200, 1, pos(118, 70)); // 2 left
    }
    pump(Duration::from_millis(100));
    unsafe { PostMessageW(child, 0x0202, 0, pos(118, 70)) }; // release: unlock
    pump(Duration::from_millis(100));
    unsafe { PostMessageW(child, 0x0200, 0, pos(50, 50)) }; // ordinary moves again
    pump(Duration::from_millis(100));
    LOCKING.store(false, Ordering::SeqCst);
    let log = INPUTS.lock().unwrap().clone();
    let text = log.join("\n");
    assert!(text.contains("PointerDelta { dx: 5.0, dy: 3.0"), "{text}");
    assert!(text.contains("PointerDelta { dx: -2.0, dy: 0.0"), "{text}");
    // Other programs (a test host clicking on this machine) may inject real mouse events while
    // this runs; judge only the moves this test posted.
    assert!(!log.iter().any(|l| l.starts_with("Move") && (l.contains("x: 125.0") || l.contains("x: 118.0"))), "a move made while locked was reported as a Move: {text}");
    assert!(log.iter().any(|l| l.starts_with("Move") && l.contains("x: 50.0")), "ordinary moves are back after the unlock: {text}");
    let foreign = log.iter().any(|l| l.contains("Right") || l.contains("x: 949"));
    let mut after = [0i32; 2];
    unsafe { GetCursorPos(&mut after) };
    if !foreign {
        assert_eq!(before, after, "the pointer is back where it was");
    }
    drop(o);
    unsafe { SetCursorPos(before[0], before[1]) };
}

#[test]
fn a_drag_keeps_the_mouse_captured_until_every_button_is_released() {
    let _t = turn();
    let o = open(None);
    pump(Duration::from_millis(300));
    let child = o.child();
    let pos = |x: i32, y: i32| ((y << 16) | x) as isize;
    unsafe { PostMessageW(child, 0x0201, 1, pos(10, 10)) }; // left down
    pump(Duration::from_millis(60));
    assert_eq!(unsafe { GetCapture() }, child, "the editor owns the mouse while a button is held (drags go on outside the window)");
    unsafe { PostMessageW(child, 0x0204, 3, pos(10, 10)) }; // right down too
    pump(Duration::from_millis(60));
    unsafe { PostMessageW(child, 0x0202, 2, pos(10, 10)) }; // left up: right still held
    pump(Duration::from_millis(60));
    assert_eq!(unsafe { GetCapture() }, child, "still captured with one button left");
    unsafe { PostMessageW(child, 0x0205, 0, pos(10, 10)) }; // right up
    pump(Duration::from_millis(60));
    assert!(unsafe { GetCapture() }.is_null(), "released with the last button");
    drop(o);
}

#[test]
fn input_that_asks_for_a_frame_is_drawn_by_the_next_check_not_inside_the_message() {
    let _t = turn();
    REDRAW_ON_INPUT.store(true, Ordering::SeqCst);
    let o = open(None);
    pump(Duration::from_millis(300));
    let child = o.child();
    let base = count();
    let pos = |x: i32, y: i32| ((y << 16) | x) as isize;
    // 50 moves, each handled before the next is sent: no frame may be drawn inside them
    for i in 0..50 {
        unsafe { SendMessageW(child, 0x0200, 0, pos(10 + i, 10)) };
    }
    assert_eq!(count(), base, "a frame was drawn inside the input messages");
    // the ticker wakes the view at once; the pacer draws within a frame or two
    pump(Duration::from_millis(100));
    let drawn = count() - base;
    REDRAW_ON_INPUT.store(false, Ordering::SeqCst);
    drop(o);
    assert!(drawn >= 1, "the input's frame was never drawn");
    assert!(drawn <= 8, "{drawn} frames in 100 ms (at most the display rate)");
}

#[test]
fn losing_the_capture_while_a_button_is_held_sends_a_release_and_a_leave() {
    let _t = turn();
    let o = open(None);
    pump(Duration::from_millis(300));
    let child = o.child();
    let pos = |x: i32, y: i32| ((y << 16) | x) as isize;
    unsafe { SendMessageW(child, 0x0201, 1, pos(10, 10)) }; // WM_LBUTTONDOWN: the view takes the capture
    assert_eq!(unsafe { GetCapture() }, child, "the press captures the mouse");
    INPUTS.lock().unwrap().clear();
    unsafe { ReleaseCapture() }; // something else takes the mouse; the view loses the capture
    pump(Duration::from_millis(20));
    let log = INPUTS.lock().unwrap().join("\n");
    assert!(log.contains("Release") && log.contains("Leave"), "{log}");
    drop(o);
}

#[test]
fn a_test_ui_has_a_clock_the_test_moves() {
    // E9: time-dependent editor logic is tested without waiting.
    let ui = Ui::for_tests::<Plug>(100.0, 50.0);
    let clock = ui.test_clock().expect("a test Ui has a manual clock").clone();
    assert_eq!(ui.now(), Duration::ZERO);
    clock.advance(Duration::from_millis(1500));
    assert_eq!(ui.now(), Duration::from_millis(1500));
    clock.set(Duration::from_secs(60));
    assert_eq!(ui.now(), Duration::from_secs(60));
    assert_eq!(ui.scale_factor(), 1.0);
    assert!(!ui.is_pointer_locked());
    ui.lock_pointer(true);
    assert!(ui.is_pointer_locked(), "a requested lock counts at once");
}

#[test]
fn the_default_next_frame_follows_animating() {
    struct Old(bool);
    impl GpuEditor for Old {
        fn new(_: &Gpu, _: &Ui) -> Self {
            Old(false)
        }
        fn render(&mut self, _: &Gpu, _: &Frame, _: &Ui) {}
        fn animating(&self) -> bool {
            self.0
        }
    }
    assert_eq!(Old(false).next_frame(), None, "an editor that never animated is never asked for frames");
    assert_eq!(Old(true).next_frame(), Some(Duration::ZERO), "animating means once per display refresh");
}

#[test]
fn the_cursor_position_api_is_available_for_the_pointer_lock_tests() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
    }
    const ERROR_ACCESS_DENIED: u32 = 5;
    let mut p = [0; 2];
    if unsafe { GetCursorPos(&mut p) } == 0 {
        // A locked screen or a service session has no input desktop: nothing to check there.
        let error = unsafe { GetLastError() };
        assert_eq!(error, ERROR_ACCESS_DENIED, "GetCursorPos failed with error {error}");
        eprintln!("skipped: no interactive desktop (the screen may be locked)");
    }
}
