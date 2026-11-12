//! GPU view backend: the plugin draws its editor itself with wgpu, in a child
//! window of the host's window (`[view] backend = "gpu"`): a child HWND on
//! Windows (Direct3D 12 or Vulkan), an NSView on macOS (Metal).
//!
//! ```ignore
//! pub struct Editor { /* pipelines, buffers, ... */ }
//!
//! impl ferritron::gpu_view::GpuEditor for Editor {
//!     fn new(gpu: &Gpu, ui: &Ui) -> Self { /* create pipelines for gpu.format */ }
//!     fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
//!         let gain = ui.normalized("gain");
//!         /* encode a render pass into frame.target and submit it to gpu.queue */
//!     }
//!     fn input(&mut self, event: &Input, ui: &Ui) -> bool {
//!         if let Input::Press { x, .. } = *event { ui.edit_normalized("gain", x / ui.width()); }
//!         true // redraw
//!     }
//! }
//! ```
//!
//! The project file names the type: `[view] backend = "gpu"`, `editor = "Editor"`.
//! The editor is redrawn when a parameter changes (UI, automation or host),
//! after input that asks for it, on resize, and at the pace of
//! [`GpuEditor::next_frame`] (meters, animations: 60 / 30 / 20 Hz or once per display
//! refresh). An editor that asks for nothing and has nothing that changed is not drawn at
//! all; frames never come faster than the display refreshes (read from the compositor on
//! Windows). Sizes are logical pixels
//! (points on macOS); `frame` carries the physical size and the scale factor.
//! On macOS, Command counts as `ctrl` in [`Modifiers`], and typed text arrives
//! after the keyboard layout (input methods such as Japanese composition are
//! not supported there yet).
//!
//! All views of a process share one wgpu device. Edits go to the host as
//! gestures (`begin` / `set` / `end`), so DAWs record them as automation.
//!
//! Without a usable GPU (no device can present to the window, or the device
//! was lost and could not be made again three times in a row) an editor that
//! implements [`GpuEditor::new_cpu`] and [`GpuEditor::render_cpu`] (such as
//! [`FngAdapter`] on Windows) is drawn on the CPU into the window instead. It
//! goes back to the GPU when a device can be made again; the check is made every
//! five seconds. `FERRITRON_GPU_VIEW=cpu` forces the CPU mode (for tests and
//! for users whose GPU driver misbehaves). An editor that needs the GPU does
//! not open without one.
//!
//! [`Ui::instance`] is the editor's plugin instance (identity, track, store,
//! the other instances of the plugin); the editor is also redrawn when it
//! changes, when instances come or go, and when another thread asks through
//! a [`RedrawHandle`]. [`Ui::for_tests`] makes a `Ui` without a host or GPU,
//! for unit tests of an editor's logic.

// Linux has no GPU view yet: the shared code is unused there.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code, clippy::extra_unused_type_parameters))]

use ferritron_core::__private::PluginMeta;
use ferritron_core::abi::{ViewApi, ViewHost};
use ferritron_core::{InstanceHandle, InstanceId, ParamInfo, ParamSet, Process, TrackInfo};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use wgpu;

mod pacing;
pub use pacing::{Clock, ManualClock, SystemClock};

/// The refresh rate of the display the editors are shown on, in Hz, as the system's
/// compositor reports it (Windows: `DwmGetCompositionTimingInfo`); `None` where it cannot say
/// (editors are then paced as for 60 Hz).
pub fn display_refresh_hz() -> Option<f64> {
    #[cfg(windows)]
    {
        imp::dwm_refresh_hz()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(feature = "fng")]
mod native;
/// Ferritron Native Graphics, for editors that draw with it (feature `fng`).
#[cfg(feature = "fng")]
pub use ::fng;
#[cfg(feature = "fng")]
pub use native::{FngAdapter, FngEditor};

/// A plugin editor drawn with wgpu. Every method runs on the host's UI thread.
pub trait GpuEditor: 'static {
    /// Creates the editor when the host opens it: pipelines, buffers, textures
    /// for `gpu.format`.
    fn new(gpu: &Gpu, ui: &Ui) -> Self
    where
        Self: Sized;

    /// Draws one frame into `frame.target` (every pixel: the previous frame
    /// is not kept) and submits the work to `gpu.queue`.
    fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui);

    /// Mouse input, in logical pixels. Returns whether to redraw.
    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        let _ = (event, ui);
        false
    }

    /// Whether the editor uses this key press. A key the editor does not want
    /// goes to the host's window instead, so the DAW keeps its shortcuts (space
    /// for transport, ...) while the editor has the keyboard focus. Called for
    /// each key press before [`input`](GpuEditor::input) gets it as
    /// [`Input::Key`]; characters ([`Input::Char`]) are only delivered while
    /// the editor takes keys.
    fn wants_key(&self, key: Key, modifiers: Modifiers) -> bool {
        let _ = (key, modifiers);
        false
    }

    /// Whether to redraw continuously, e.g. for meters: once per display refresh. The
    /// default of [`next_frame`](GpuEditor::next_frame) is made from this.
    fn animating(&self) -> bool {
        false
    }

    /// When the editor wants its next frame, asked after every frame and at every check:
    ///
    /// * `None`: no frame is needed (the editor is drawn again only when something changes:
    ///   parameters, input, [`Ui::request_redraw`], new stream values, resize).
    /// * `Some(Duration::ZERO)`: once per display refresh.
    /// * `Some(interval)`: a frame every `interval` (for 30 Hz: `Duration::from_secs_f64(1.0 /
    ///   30.0)`). Frames are put on a fixed grid, so the rate is exact on average; an interval
    ///   shorter than the display refresh gives one frame per refresh.
    ///
    /// The default is `Some(ZERO)` while [`animating`](GpuEditor::animating) is true and `None`
    /// otherwise, so existing editors behave as before.
    fn next_frame(&self) -> Option<std::time::Duration> {
        self.animating().then_some(std::time::Duration::ZERO)
    }

    /// Makes the editor for a window that has no usable GPU (the CPU mode, Windows only: the
    /// view then draws with [`render_cpu`](GpuEditor::render_cpu)). The default is `None`: an
    /// editor that draws with its own GPU code does not open without a GPU, and the log says so.
    fn new_cpu(ui: &Ui) -> Option<Self>
    where
        Self: Sized,
    {
        let _ = ui;
        None
    }

    /// Draws one frame on the CPU into `out`, for a window without a GPU: `size` physical pixels
    /// at `scale` physical pixels per logical pixel, 4 bytes per pixel in the order blue, green,
    /// red, alpha, with premultiplied alpha (what a Windows DIB takes). `out` may have any length
    /// on entry: the editor resizes it to `width * height * 4` bytes, and it is kept between frames
    /// so that a frame of the same size allocates nothing. Returns false when the frame could not
    /// be drawn. The default returns false (see [`new_cpu`](GpuEditor::new_cpu)).
    fn render_cpu(&mut self, ui: &Ui, size: (u32, u32), scale: f64, out: &mut Vec<u8>) -> bool {
        let _ = (ui, size, scale, out);
        false
    }
}

/// The GPU shared by all editors of the process.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The format of `Frame::target`.
    pub format: wgpu::TextureFormat,
}

/// One frame to draw.
#[derive(Debug)]
#[non_exhaustive]
pub struct Frame {
    pub target: wgpu::TextureView,
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical pixel.
    pub scale: f64,
    /// When this frame started, by the view's clock ([`Ui::now`]).
    pub time: std::time::Duration,
    /// Time since the previous frame (zero for the first), so animations need no clock of
    /// their own.
    pub delta: std::time::Duration,
    /// How many frames this editor has drawn before this one.
    pub index: u64,
}

impl Frame {
    /// A frame for `target` (tests and tools that draw an editor themselves): time zero, the
    /// first frame.
    pub fn new(target: wgpu::TextureView, width: u32, height: u32, scale: f64) -> Frame {
        Frame { target, width, height, scale, time: std::time::Duration::ZERO, delta: std::time::Duration::ZERO, index: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Mouse input in logical pixels from the editor's top left corner. Every
/// pointer event carries the modifier keys held at that moment.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Input {
    Press { x: f64, y: f64, button: Button, modifiers: Modifiers },
    DoubleClick { x: f64, y: f64, button: Button, modifiers: Modifiers },
    Move { x: f64, y: f64, modifiers: Modifiers },
    Release { x: f64, y: f64, button: Button, modifiers: Modifiers },
    /// `delta` in notches, positive away from the user; `delta_x` sideways
    /// (positive to the right; 0 where the device or platform has none).
    Wheel { x: f64, y: f64, delta: f64, delta_x: f64, modifiers: Modifiers },
    /// A key went down (`pressed`, with `repeat` for auto-repeat) or up. Only
    /// keys the editor wants ([`GpuEditor::wants_key`]) arrive, and only while
    /// it has the keyboard focus (a click gives it the focus).
    Key { key: Key, pressed: bool, repeat: bool, modifiers: Modifiers },
    /// Text typed: one character after keyboard layout and input method
    /// (so Japanese and other composed text arrive as finished characters).
    Char(char),
    /// The editor gained or lost the keyboard focus.
    Focus(bool),
    /// The mouse left the editor (no button held): hover effects end here.
    Leave,
    /// While the pointer is locked ([`Ui::lock_pointer`]): how far the mouse moved, in logical
    /// pixels (positive right and down). No `Move` events arrive during the lock.
    PointerDelta { dx: f64, dy: f64, modifiers: Modifiers },
    /// The system switched between a light (`false`) and a dark (`true`) theme.
    ThemeChanged(bool),
}

/// The shape of the mouse pointer over the editor ([`Ui::set_cursor`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cursor {
    #[default]
    Arrow,
    /// A pointing hand: something to click.
    Hand,
    /// An open hand: something to drag.
    Grab,
    /// A closed hand: dragging.
    Grabbing,
    /// A text cursor.
    Text,
    Crosshair,
    /// Moves in every direction.
    Move,
    ResizeHorizontal,
    ResizeVertical,
    NotAllowed,
    /// No pointer (while the editor draws its own).
    Hidden,
}

/// Asks an editor to redraw from any thread ([`Ui::redraw_handle`]): for
/// changes the editor cannot see in its own parameters, such as another
/// instance moving. Cheap; several requests before the next frame make one
/// redraw. Does nothing once the editor is closed.
#[derive(Clone, Debug)]
pub struct RedrawHandle(Arc<AtomicBool>);

impl RedrawHandle {
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Redraws the editor whenever an instance event passes `filter` (another instance of the
    /// plugin moved, opened its editor, ...; see `ferritron::instances_watch`). Keep the
    /// returned watch as long as the editor wants the redraws.
    pub fn wake_on(&self, filter: impl Fn(&ferritron_core::InstanceEvent) -> bool + Send + 'static) -> ferritron_core::Watch {
        let me = self.clone();
        ferritron_core::instances_watch(move |event| {
            if filter(event) {
                me.request();
            }
        })
    }
}

/// A key, by what it does rather than the layout under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Backspace,
    Delete,
    Enter,
    Escape,
    Tab,
    Space,
    /// A letter or digit key, as the upper-case letter or digit.
    Character(char),
    /// Any other key, by the operating system's key code.
    Other(u32),
}

/// Modifier keys held during a key event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// The editor's link to the plugin: parameters, gestures and messages.
pub struct Ui {
    host: ViewHost,
    infos: &'static [ParamInfo],
    format: fn(usize, &ParamInfo, f64) -> String,
    messages: HashMap<String, String>,
    language: String,
    size: Cell<(f64, f64)>,
    instance: Option<InstanceHandle>,
    cursor: Cell<Cursor>,
    redraw: Arc<AtomicBool>,
    /// The parameter values of a `Ui` made by [`Ui::for_tests`].
    test_values: Option<Box<TestValues>>,
    /// The instance's data streams (`[streams]`), refreshed once per frame.
    streams: Vec<StreamBuf>,
    /// The time source of the view (replaceable in tests).
    clock: Arc<dyn Clock>,
    /// Set for a `for_tests` `Ui`: the same clock, to be moved by hand.
    test_clock: Option<ManualClock>,
    /// Physical pixels per logical pixel.
    scale: Cell<f64>,
    /// A change of the pointer lock the editor asked for and the view has not applied yet.
    lock_request: Cell<Option<bool>>,
    locked: Cell<bool>,
    /// Set while the editor takes typed text: the caret, in logical pixels (see `set_text_input`).
    text_caret: Cell<Option<(f64, f64)>>,
}

/// The newest values of one data stream, as of the last refresh.
struct StreamBuf {
    name: String,
    reader: RefCell<Option<ferritron_core::StreamReader>>,
    /// Written only by `Ui::refresh_streams`, which the frame driver calls while no editor
    /// code runs; `Ui::stream` hands out shared references to it.
    data: std::cell::UnsafeCell<Vec<f32>>,
    has: Cell<bool>,
}

/// Parameter values behind a test `Ui`, by ID (normalized).
struct TestValues(RefCell<HashMap<u32, f64>>);

unsafe extern "C" fn test_get(ctx: *mut c_void, id: u32) -> f64 {
    unsafe { &*(ctx as *const TestValues) }.0.borrow().get(&id).copied().unwrap_or(0.0)
}

unsafe extern "C" fn test_set(ctx: *mut c_void, id: u32, normalized: f64) {
    unsafe { &*(ctx as *const TestValues) }.0.borrow_mut().insert(id, normalized);
}

unsafe extern "C" fn test_gesture(_ctx: *mut c_void, _id: u32) {}

impl Ui {
    pub(crate) fn new<P: Process + PluginMeta>(host: ViewHost, size: (f64, f64)) -> Self {
        let localized = P::localized();
        Self {
            host,
            infos: P::Params::infos(),
            format: |index, info, plain| P::localized().format(index, info, plain),
            messages: localized.ui_messages().iter().cloned().collect(),
            language: localized.language.clone(),
            size: Cell::new(size),
            // The shim's object for the instance is the view host's context.
            instance: {
                let instance = InstanceHandle::for_shim_ctx(host.ctx);
                if let Some(i) = &instance {
                    i.editor_opened(true);
                }
                instance
            },
            cursor: Cell::new(Cursor::Arrow),
            redraw: Arc::new(AtomicBool::new(false)),
            test_values: None,
            streams: Vec::new(),
            clock: Arc::new(SystemClock),
            test_clock: None,
            scale: Cell::new(1.0),
            lock_request: Cell::new(None),
            locked: Cell::new(false),
            text_caret: Cell::new(None),
        }
        .with_streams()
    }

    fn with_streams(mut self) -> Self {
        self.load_streams();
        self
    }

    fn load_streams(&mut self) {
        if let Some(i) = &self.instance {
            self.streams = i
                .streams()
                .into_iter()
                .filter(|(_, _, kind)| *kind == ferritron_core::StreamKind::Data)
                .filter_map(|(name, _, _)| {
                    Some(StreamBuf { name: name.to_string(), reader: RefCell::new(Some(i.stream(name)?)), data: Default::default(), has: Cell::new(false) })
                })
                .collect();
        }
    }

    /// The newest values of data stream `name` (`[streams]` in the project file), or `None`
    /// when there is no such stream or nothing was written yet. The values are those of the
    /// start of the frame. The audio thread writes only while an editor is open, so a stream
    /// starts to fill when the editor opens. A new frame is drawn whenever new values arrive.
    pub fn stream(&self, name: &str) -> Option<&[f32]> {
        let b = self.streams.iter().find(|b| b.name == name)?;
        if !b.has.get() {
            return None;
        }
        // SAFETY: `refresh_streams` is the only writer and never runs while editor code does.
        Some(unsafe { &*b.data.get() })
    }

    /// Takes the newest stream values; true when any stream had new values. Called by the
    /// frame driver before it asks the editor to draw.
    #[doc(hidden)]
    pub fn refresh_streams(&self) -> bool {
        let mut any = false;
        for b in &self.streams {
            let mut reader = b.reader.borrow_mut();
            let Some(reader) = reader.as_mut() else { continue };
            // SAFETY: see `stream`.
            let data = unsafe { &mut *b.data.get() };
            if reader.read_new(data) {
                b.has.set(true);
                any = true;
            }
        }
        any
    }

    /// Links `instance` to a [`for_tests`](Self::for_tests) `Ui` as a host shim links the editor's
    /// instance: the editor counts as open and the instance's data streams reach
    /// [`stream`](Self::stream) (after `refresh_streams`, which the frame driver calls).
    pub fn attach_instance_for_tests(&mut self, instance: InstanceHandle) {
        instance.editor_opened(true);
        self.instance = Some(instance);
        self.load_streams();
    }

    /// Gives a [`for_tests`](Self::for_tests) `Ui` the values of stream `name`, as if the
    /// audio thread had written them.
    pub fn set_stream_for_tests(&mut self, name: &str, values: &[f32]) {
        match self.streams.iter().find(|b| b.name == name) {
            Some(b) => {
                // SAFETY: `&mut self`: no references are out.
                let data = unsafe { &mut *b.data.get() };
                data.clear();
                data.extend_from_slice(values);
                b.has.set(true);
            }
            None => self.streams.push(StreamBuf { name: name.to_string(), reader: RefCell::new(None), data: std::cell::UnsafeCell::new(values.to_vec()), has: Cell::new(true) }),
        }
    }

    /// A `Ui` for unit tests of an editor's logic: no host, no window, no
    /// GPU. Parameters start at their defaults; edits change them at once
    /// (gestures are accepted and ignored). [`instance`](Self::instance) is
    /// `None`.
    ///
    /// ```ignore
    /// let ui = Ui::for_tests::<MyPlugin>(480.0, 240.0);
    /// my_editor_logic.click(&ui, 10.0, 20.0);
    /// assert_eq!(ui.value("gain"), -6.0);
    /// ```
    pub fn for_tests<P: Process + PluginMeta>(width: f64, height: f64) -> Self {
        let values = Box::new(TestValues(RefCell::new(P::Params::infos().iter().map(|i| (i.id, i.normalize(i.default))).collect())));
        let host = ViewHost {
            ctx: &*values as *const TestValues as *mut c_void,
            get_param: test_get,
            begin_edit: test_gesture,
            perform_edit: test_set,
            end_edit: test_gesture,
            request_resize: None,
        };
        let mut ui = Self::new::<P>(host, (width, height));
        ui.instance = None;
        ui.test_values = Some(values);
        let clock = ManualClock::new();
        ui.clock = Arc::new(clock.clone());
        ui.test_clock = Some(clock);
        ui
    }

    /// The clock of a [`for_tests`](Self::for_tests) `Ui`: it starts at zero and moves only when
    /// told to (`advance`), and [`now`](Self::now) follows it. `None` for a real editor.
    pub fn test_clock(&self) -> Option<&ManualClock> {
        self.test_clock.as_ref()
    }

    /// The current time by the view's clock. Use it instead of `Instant::now()` for anything an
    /// editor decides by time, so tests can move the clock ([`test_clock`](Self::test_clock)).
    pub fn now(&self) -> std::time::Duration {
        self.clock.now()
    }

    /// Physical pixels per logical pixel (the DPI scale) of the window now. It changes when the
    /// window moves to another monitor or the user changes the scaling.
    pub fn scale_factor(&self) -> f64 {
        self.scale.get()
    }

    /// Locks (`true`) or unlocks the mouse pointer, for knobs and faders that turn endlessly: the
    /// pointer is hidden and stays where it is, and the editor gets [`Input::PointerDelta`] for
    /// every movement instead of `Move`. The pointer comes back to where it was when the lock ends
    /// (also when the editor closes). Windows only so far; elsewhere the call is ignored. Call it
    /// from `input`, for example on `Press`, and unlock on `Release`.
    pub fn lock_pointer(&self, lock: bool) {
        self.lock_request.set(Some(lock));
    }

    /// Whether the pointer is locked now (the lock takes effect right after the call returns).
    pub fn is_pointer_locked(&self) -> bool {
        self.locked.get() || self.lock_request.get() == Some(true)
    }

    pub(crate) fn take_lock_request(&self) -> Option<bool> {
        self.lock_request.take()
    }

    pub(crate) fn set_locked(&self, locked: bool) {
        self.locked.set(locked);
    }

    pub(crate) fn set_scale_factor(&self, scale: f64) {
        self.scale.set(scale);
    }

    pub(crate) fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Changes the size of a [`for_tests`](Self::for_tests) `Ui` (logical pixels).
    pub fn resize_for_tests(&self, width: f64, height: f64) {
        self.set_size(width, height);
    }

    /// The editor's plugin instance: its identity, track, store and the
    /// other instances of the plugin ([`ferritron::instances`]). `None` when
    /// the host links no instance to the editor (the editor in a process of
    /// its own, hot reload, [`for_tests`](Self::for_tests)).
    ///
    /// [`ferritron::instances`]: ferritron_core::instances
    pub fn instance(&self) -> Option<&InstanceHandle> {
        self.instance.as_ref()
    }

    /// The instance's identity (see [`InstanceId`]).
    pub fn instance_id(&self) -> Option<InstanceId> {
        self.instance.as_ref().map(InstanceHandle::id)
    }

    /// The track the instance sits on, as far as the host says.
    pub fn track(&self) -> TrackInfo {
        self.instance.as_ref().map(InstanceHandle::track).unwrap_or_default()
    }

    /// The track's name, if the host says it.
    pub fn track_name(&self) -> Option<String> {
        self.track().name
    }

    /// Sets the mouse pointer's shape over the editor; it stays until set
    /// again (typically from [`GpuEditor::input`] on `Move`).
    pub fn set_cursor(&self, cursor: Cursor) {
        self.cursor.set(cursor);
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor.get()
    }

    /// A handle another thread may use to ask for a redraw.
    pub fn redraw_handle(&self) -> RedrawHandle {
        RedrawHandle(self.redraw.clone())
    }

    /// Asks for a redraw at the next tick (on the UI thread).
    pub fn request_redraw(&self) {
        self.redraw.store(true, Ordering::Release);
    }

    /// Tells the view that the editor takes typed text (a text field has the focus): `Some` with
    /// the caret's position in logical pixels, where the Japanese (or other) input method shows
    /// its composition window, and text the input method finishes arrives as [`Input::Char`].
    /// `None` when the editor takes no text. Call it from `render` or `input`; the view reads it
    /// on the UI thread.
    pub fn set_text_input(&self, caret: Option<(f64, f64)>) {
        self.text_caret.set(caret);
    }

    /// The caret set by [`set_text_input`](Ui::set_text_input), if the editor takes text.
    pub(crate) fn text_input(&self) -> Option<(f64, f64)> {
        self.text_caret.get()
    }

    /// Takes a pending redraw request.
    pub(crate) fn take_redraw(&self) -> bool {
        self.redraw.swap(false, Ordering::AcqRel)
    }

    fn info(&self, key: &str) -> Option<(usize, &'static ParamInfo)> {
        self.infos.iter().enumerate().find(|(_, i)| i.key == key)
    }

    /// The plugin's parameters, in project-file order.
    pub fn params(&self) -> &'static [ParamInfo] {
        self.infos
    }

    /// The value of parameter `key` (from the project file), 0..1.
    /// Unknown keys read as 0.
    pub fn normalized(&self, key: &str) -> f64 {
        self.info(key).map_or(0.0, |(_, i)| unsafe { (self.host.get_param)(self.host.ctx, i.id) })
    }

    /// The plain value (e.g. dB) of parameter `key`.
    pub fn value(&self, key: &str) -> f64 {
        self.info(key).map_or(0.0, |(_, i)| i.denormalize(self.normalized(key)))
    }

    /// The display text of parameter `key` in the user's language, without the unit.
    pub fn text(&self, key: &str) -> String {
        self.info(key).map_or_else(String::new, |(n, i)| (self.format)(n, i, self.value(key)))
    }

    /// Starts a gesture (e.g. a drag) on parameter `key`.
    pub fn begin(&self, key: &str) {
        if let Some((_, i)) = self.info(key) {
            unsafe { (self.host.begin_edit)(self.host.ctx, i.id) };
        }
    }

    /// Sets parameter `key` to a plain value during a gesture.
    pub fn set(&self, key: &str, plain: f64) {
        if let Some((_, i)) = self.info(key) {
            unsafe { (self.host.perform_edit)(self.host.ctx, i.id, i.normalize(plain)) };
        }
    }

    /// Sets parameter `key` to a normalized value (0..1) during a gesture.
    pub fn set_normalized(&self, key: &str, normalized: f64) {
        if let Some((_, i)) = self.info(key) {
            unsafe { (self.host.perform_edit)(self.host.ctx, i.id, normalized.clamp(0.0, 1.0)) };
        }
    }

    /// Ends the gesture on parameter `key`.
    pub fn end(&self, key: &str) {
        if let Some((_, i)) = self.info(key) {
            unsafe { (self.host.end_edit)(self.host.ctx, i.id) };
        }
    }

    /// A one-shot change: begin, set, end.
    pub fn edit(&self, key: &str, plain: f64) {
        self.begin(key);
        self.set(key, plain);
        self.end(key);
    }

    /// A one-shot change to a normalized value.
    pub fn edit_normalized(&self, key: &str, normalized: f64) {
        self.begin(key);
        self.set_normalized(key, normalized);
        self.end(key);
    }

    /// A message from `locales/<language>/*.ftl` in the user's language;
    /// `{ $name }` is filled from `args`. Unknown keys return the key.
    pub fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut text = self.messages.get(key).cloned().unwrap_or_else(|| key.to_string());
        for (name, value) in args {
            text = text.replace(&format!("{{${name}}}"), value);
        }
        text
    }

    /// The user's language, e.g. `"ja"`.
    /// Asks the host to make the editor window `width` x `height` logical
    /// pixels. Returns whether the request was passed on; the host decides and
    /// answers with the new size (the editor is resized then). Formats that
    /// cannot resize on request (or a test `Ui`) return false.
    pub fn request_resize(&self, width: f64, height: f64) -> bool {
        let Some(resize) = self.host.request_resize else { return false };
        let scale = self.scale_factor();
        // SAFETY: the shim's callback, called on the UI thread with its own context.
        unsafe { resize(self.host.ctx, (width * scale).round().max(1.0) as u32, (height * scale).round().max(1.0) as u32) }
    }

    /// Whether the system is set to a dark theme (light where it cannot be
    /// told). [`Input::ThemeChanged`] arrives when it changes (Windows: only
    /// where the host passes the message on; not verified in a host).
    pub fn system_dark(&self) -> bool {
        #[cfg(windows)]
        {
            imp::system_dark()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    /// The editor's size in logical pixels.
    pub fn width(&self) -> f64 {
        self.size.get().0
    }

    pub fn height(&self) -> f64 {
        self.size.get().1
    }

    pub(crate) fn set_size(&self, width: f64, height: f64) {
        self.size.set((width, height));
    }

    /// The normalized values of all parameters, to notice changes.
    pub(crate) fn snapshot(&self) -> Vec<f64> {
        self.infos.iter().map(|i| unsafe { (self.host.get_param)(self.host.ctx, i.id) }).collect()
    }
}

/// The `ferritron.view.v1` table for plugin `P` with editor `E`: default size
/// and resize limits in logical pixels.
pub const fn view_api<P: Process + PluginMeta, E: GpuEditor>(size: (u32, u32), min: (u32, u32), max: (u32, u32)) -> ViewApi {
    ViewApi {
        width: size.0,
        height: size.1,
        min_width: min.0,
        min_height: min.1,
        max_width: max.0,
        max_height: max.1,
        open: open::<P, E>,
        close,
        set_size,
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod common;
#[cfg(windows)]
mod imp;
#[cfg(target_os = "macos")]
mod mac;

#[cfg(windows)]
use imp::{close, open, set_size};
#[cfg(target_os = "macos")]
use mac::{close, open, set_size};

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn open<P: Process + PluginMeta, E: GpuEditor>(
    _parent: *mut std::ffi::c_void,
    _host: *const ViewHost,
    _width: u32,
    _height: u32,
    _scale: f64,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn close(_view: *mut std::ffi::c_void) {}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe extern "C" fn set_size(_view: *mut std::ffi::c_void, _width: u32, _height: u32, _scale: f64) {}

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(i) = &self.instance {
            i.editor_opened(false);
        }
    }
}
