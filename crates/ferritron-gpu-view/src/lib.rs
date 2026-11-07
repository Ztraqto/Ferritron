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
//! after input that asks for it, on resize, and on every tick while
//! [`GpuEditor::animating`] (meters, animations). Sizes are logical pixels
//! (points on macOS); `frame` carries the physical size and the scale factor.
//! On macOS, Command counts as `ctrl` in [`Modifiers`], and typed text arrives
//! after the keyboard layout (input methods such as Japanese composition are
//! not supported there yet).
//!
//! All views of a process share one wgpu device. Edits go to the host as
//! gestures (`begin` / `set` / `end`), so DAWs record them as automation.

// Linux has no GPU view yet: the shared code is unused there.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code, clippy::extra_unused_type_parameters))]

use ferritron_core::__private::PluginMeta;
use ferritron_core::abi::{ViewApi, ViewHost};
use ferritron_core::{ParamInfo, ParamSet, Process};
use std::cell::Cell;
use std::collections::HashMap;

pub use wgpu;

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

    /// Whether to redraw on every tick (about 60 per second), e.g. for meters.
    fn animating(&self) -> bool {
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
pub struct Frame {
    pub target: wgpu::TextureView,
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical pixel.
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Mouse input in logical pixels from the editor's top left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    Press { x: f64, y: f64, button: Button },
    DoubleClick { x: f64, y: f64, button: Button },
    Move { x: f64, y: f64 },
    Release { x: f64, y: f64, button: Button },
    /// `delta` in notches; positive away from the user.
    Wheel { x: f64, y: f64, delta: f64 },
    /// A key went down (`pressed`, with `repeat` for auto-repeat) or up. Only
    /// keys the editor wants ([`GpuEditor::wants_key`]) arrive, and only while
    /// it has the keyboard focus (a click gives it the focus).
    Key { key: Key, pressed: bool, repeat: bool, modifiers: Modifiers },
    /// Text typed: one character after keyboard layout and input method
    /// (so Japanese and other composed text arrive as finished characters).
    Char(char),
    /// The editor gained or lost the keyboard focus.
    Focus(bool),
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
}

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
        }
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
