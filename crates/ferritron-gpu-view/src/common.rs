//! What every platform's view shares: the wgpu instance and device of the
//! process, how a surface is configured, and the [`Canvas`] that draws the
//! editor into it. A canvas draws on the GPU; when there is no usable GPU it
//! draws on the CPU instead (Windows, see [`CPU_VIEW`]).

use super::pacing::{Clock, Pacer, Tick, IDLE_CHECK};
use super::{Frame, Gpu, GpuEditor, Input, Ui};
use ferritron_core::__private::PluginMeta;
use ferritron_core::Process;
use ferritron_core::abi::ViewHost;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

pub(crate) fn instance() -> &'static wgpu::Instance {
    static INSTANCE: OnceLock<wgpu::Instance> = OnceLock::new();
    INSTANCE.get_or_init(|| wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()))
}

/// The device shared by all editors of this process: one that can present
/// to the first editor's window. When the device is lost, or wgpu reports an
/// error it cannot hand to anyone, `lost` is set; the next editor that needs
/// a device makes a new one with the next `generation`.
pub(crate) struct Shared {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub generation: u64,
    pub lost: Arc<AtomicBool>,
}

/// Something a [`renew`] slot holds: it can be lost, and is then replaced.
pub(crate) trait Replaceable {
    fn is_lost(&self) -> bool;
    fn generation(&self) -> u64;
}

impl Replaceable for Shared {
    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        self.generation
    }
}

/// The value in `slot` if it is not lost; otherwise a new one from `make`, with the next
/// generation (1 for the first). A failed `make` leaves the lost value in place, so the next
/// call tries again.
pub(crate) fn renew<T: Replaceable>(slot: &Mutex<Option<Arc<T>>>, make: impl FnOnce(u64) -> Option<T>) -> Option<Arc<T>> {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(current) = slot.as_ref()
        && !current.is_lost()
    {
        return Some(current.clone());
    }
    let next = slot.as_ref().map_or(1, |lost| lost.generation() + 1);
    let fresh = Arc::new(make(next)?);
    *slot = Some(fresh.clone());
    Some(fresh)
}

/// The process's shared device, made (or made again after a loss) for `surface`.
pub(crate) fn shared(surface: &wgpu::Surface) -> Option<Arc<Shared>> {
    static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
    renew(&SHARED, |generation| {
        let adapter = pollster::block_on(instance().request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(surface), ..Default::default() })).ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("ferritron-gpu-view"), ..Default::default() })).ok()?;
        let lost = Arc::new(AtomicBool::new(false));
        // An error nobody else handles would panic; mark the device instead, and the views make a new one.
        let flag = lost.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            eprintln!("ferritron-gpu-view: GPU error, the device is replaced: {error}");
            flag.store(true, Ordering::Release);
        }));
        let flag = lost.clone();
        device.set_device_lost_callback(move |reason, message| {
            eprintln!("ferritron-gpu-view: GPU device lost ({reason:?}: {message}), the device is replaced");
            flag.store(true, Ordering::Release);
        });
        Some(Shared { adapter, device, queue, generation, lost })
    })
}

/// Whether a window can show a CPU frame. Windows does (`StretchDIBits` in the Windows view);
/// elsewhere there is no CPU mode yet, so a view without a GPU does not open.
pub(crate) const CPU_VIEW: bool = cfg!(windows);

/// The largest side a CPU frame may have (the same limit as Ferritron Native Graphics).
const CPU_MAX_SIDE: u32 = 16384;

/// How long a view waits before it tries to make a device again.
const RETRY: Duration = Duration::from_secs(5);

/// Failed attempts to make a device again in a row before the view draws on the CPU.
const FALLBACK_AFTER: u32 = 3;

/// A size the device (or the CPU mode) accepts: at least 1, at most `max` (`N3`).
fn clamp_size(size: u32, max: u32) -> u32 {
    size.clamp(1, max)
}

/// The configuration for a surface of `width` x `height` physical pixels:
/// a plain (non-sRGB) format, so colors written are the colors shown (as in
/// CSS), and a present mode that never blocks the host's UI thread on
/// vertical sync.
pub(crate) fn surface_config(surface: &wgpu::Surface, shared: &Shared, width: u32, height: u32) -> Option<wgpu::SurfaceConfiguration> {
    let caps = surface.get_capabilities(&shared.adapter);
    let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).or(caps.formats.first().copied())?;
    let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
        .into_iter()
        .find(|m| caps.present_modes.contains(m))
        .unwrap_or(wgpu::PresentMode::Fifo);
    let limit = shared.device.limits().max_texture_dimension_2d;
    Some(wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: clamp_size(width, limit),
        height: clamp_size(height, limit),
        present_mode,
        desired_maximum_frame_latency: 1,
        alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    })
}

/// The configuration of a view in the CPU mode: only its size is used (nothing is presented to
/// the surface, the window paints the CPU frame).
fn cpu_config(width: u32, height: u32) -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: wgpu::TextureFormat::Bgra8Unorm,
        width: clamp_size(width, CPU_MAX_SIDE),
        height: clamp_size(height, CPU_MAX_SIDE),
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 1,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    }
}

/// What a view draws with on the GPU: the device, the configuration of the surface for it, the
/// device's lost flag and its generation.
struct GpuSide {
    gpu: Gpu,
    config: wgpu::SurfaceConfiguration,
    lost: Arc<AtomicBool>,
    generation: u64,
}

/// The process's device for `surface`, with the surface configured for `pixels`. None when no
/// device can present to the surface or no configuration fits.
fn gpu_side(surface: &wgpu::Surface<'static>, pixels: (u32, u32)) -> Option<GpuSide> {
    let shared = shared(surface)?;
    let config = surface_config(surface, &shared, pixels.0, pixels.1)?;
    surface.configure(&shared.device, &config);
    let gpu = Gpu { device: shared.device.clone(), queue: shared.queue.clone(), format: config.format };
    Some(GpuSide { gpu, config, lost: shared.lost.clone(), generation: shared.generation })
}

/// Makes editor `E` for a device; a panic in `E::new` gives `None`.
fn make_editor<E: GpuEditor>(gpu: &Gpu, ui: &Ui) -> Option<Box<dyn GpuEditor>> {
    catch_unwind(AssertUnwindSafe(|| E::new(gpu, ui))).ok().map(|editor| Box::new(editor) as Box<dyn GpuEditor>)
}

/// Makes editor `E` for the CPU mode (`GpuEditor::new_cpu`); a panic gives `None`.
fn make_cpu_editor<E: GpuEditor>(ui: &Ui) -> Option<Box<dyn GpuEditor>> {
    catch_unwind(AssertUnwindSafe(|| E::new_cpu(ui))).ok().flatten().map(|editor| Box::new(editor) as Box<dyn GpuEditor>)
}

/// An editor drawing into a surface: what is the same on every platform.
/// The platform view owns one and forwards its events to it.
pub(crate) struct Canvas {
    /// Declared first: dropped before the surface.
    editor: Option<Box<dyn GpuEditor>>,
    /// Makes the editor again on a new device (the same steps as opening it).
    make: fn(&Gpu, &Ui) -> Option<Box<dyn GpuEditor>>,
    /// Makes the editor for the CPU mode.
    make_cpu: fn(&Ui) -> Option<Box<dyn GpuEditor>>,
    surface: wgpu::Surface<'static>,
    /// The surface configuration of the GPU. In the CPU mode only its size is used (the window's).
    config: wgpu::SurfaceConfiguration,
    /// The device, while the view draws on the GPU; `None` in the CPU mode.
    gpu: Option<Gpu>,
    /// The generation of the device this view draws on (0 when it has none).
    generation: u64,
    /// The generation of the device the view fell back from to the CPU (0: it never had one).
    /// The view goes back to the GPU only with a device of another generation.
    fell_back_at: u64,
    /// Failed attempts to make a device again, in a row.
    failures: u32,
    /// `FERRITRON_GPU_VIEW=cpu`: the view draws on the CPU and never tries the GPU.
    forced_cpu: bool,
    ui: Ui,
    /// Physical pixels per logical pixel.
    pub scale: f64,
    last: Vec<f64>,
    /// The instance's revision and the instance list's, as last drawn.
    seen: (u64, u64),
    dirty: bool,
    /// Set while the editor runs, against re-entry from nested events.
    busy: bool,
    pacer: Pacer,
    clock: Arc<dyn Clock>,
    /// Frames drawn so far, and when the last one started.
    frames: u64,
    last_frame: Option<Duration>,
    /// Checks until the display refresh is read again (the window may move to another monitor).
    refresh_countdown: u32,
    /// Set by a check when the display refresh should be read again (see `take_refresh_due`).
    refresh_due: bool,
    /// The display refresh as last read (None: unknown).
    refresh_hz: Option<f64>,
    /// The device this view draws on; set when the view's device is lost.
    lost: Arc<AtomicBool>,
    /// When the next attempt to make a device (or, in the CPU mode, to use the GPU again) may be
    /// made: five seconds after a failure.
    retry_at: Option<Instant>,
    /// The last CPU frame, BGRA with premultiplied alpha, and its size in pixels.
    pixels: Vec<u8>,
    pixels_size: (u32, u32),
    /// A CPU frame waits for the window to paint it (see `take_paint_request`).
    cpu_ready: bool,
    /// A panic in the drawing code: the view stays open and draws nothing more.
    broken: bool,
}

/// The display refresh in Hz, if the system says.
fn display_refresh_hz() -> Option<f64> {
    #[cfg(windows)]
    {
        super::imp::dwm_refresh_hz()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// One frame period for a display at `hz` (60 Hz when unknown). With `Fifo` only, a present waits
/// for vertical sync and blocks the host's UI thread, so the view draws at half the rate.
fn refresh_period(hz: Option<f64>, fifo: bool) -> Duration {
    let hz = hz.filter(|h| (10.0..=1000.0).contains(h)).unwrap_or(60.0);
    let period = Duration::from_secs_f64(1.0 / hz);
    if fifo { period * 2 } else { period }
}

impl Canvas {
    /// Configures `surface` for `pixels` (physical) and creates editor `E` with a logical size of
    /// `pixels / scale`. Without a usable GPU the editor is made for the CPU mode instead, if it
    /// can draw on the CPU (see `GpuEditor::new_cpu`); otherwise the view does not open.
    /// `FERRITRON_GPU_VIEW=cpu` takes the CPU mode even when the GPU works.
    pub fn new<P: Process + PluginMeta, E: GpuEditor>(surface: wgpu::Surface<'static>, host: ViewHost, pixels: (u32, u32), scale: f64) -> Option<Self> {
        let forced_cpu = CPU_VIEW && std::env::var("FERRITRON_GPU_VIEW").is_ok_and(|v| v.trim().eq_ignore_ascii_case("cpu"));
        let side = if forced_cpu { None } else { gpu_side(&surface, pixels) };
        let ui = Ui::new::<P>(host, (pixels.0 as f64 / scale, pixels.1 as f64 / scale));
        ui.set_scale_factor(scale);
        let (gpu, config, lost, generation, editor) = match side {
            Some(side) => {
                let editor = make_editor::<E>(&side.gpu, &ui)?;
                (Some(side.gpu), side.config, side.lost, side.generation, editor)
            }
            None => {
                let Some(editor) = (if CPU_VIEW { make_cpu_editor::<E>(&ui) } else { None }) else {
                    if forced_cpu {
                        eprintln!("ferritron-gpu-view: FERRITRON_GPU_VIEW=cpu, but this editor draws only with the GPU, so it does not open");
                    } else {
                        eprintln!("ferritron-gpu-view: no GPU adapter can present to this window, and this editor draws only with the GPU, so it does not open");
                    }
                    return None;
                };
                if !forced_cpu {
                    eprintln!("ferritron-gpu-view: no GPU adapter can present to this window, so the editor draws on the CPU");
                }
                (None, cpu_config(pixels.0, pixels.1), Arc::new(AtomicBool::new(false)), 0, editor)
            }
        };
        let last = ui.snapshot();
        let seen = revisions(&ui);
        let clock = ui.clock().clone();
        let refresh_hz = display_refresh_hz();
        let fifo = gpu.is_some() && config.present_mode == wgpu::PresentMode::Fifo;
        let retry_at = if gpu.is_none() { Some(Instant::now() + RETRY) } else { None };
        Some(Self {
            editor: Some(editor),
            make: make_editor::<E>,
            make_cpu: make_cpu_editor::<E>,
            surface,
            config,
            gpu,
            generation,
            fell_back_at: 0,
            failures: 0,
            forced_cpu,
            ui,
            scale,
            last,
            seen,
            dirty: true,
            busy: false,
            pacer: Pacer::new(refresh_period(refresh_hz, fifo)),
            clock,
            frames: 0,
            last_frame: None,
            refresh_countdown: 120,
            refresh_due: false,
            refresh_hz,
            lost,
            retry_at,
            pixels: Vec::new(),
            pixels_size: (0, 0),
            cpu_ready: false,
            broken: false,
        })
    }

    /// Runs `f` on the editor, unless it panicked before or is already running. The GPU is
    /// `None` in the CPU mode.
    pub fn editor<R>(&mut self, f: impl FnOnce(&mut dyn GpuEditor, Option<&Gpu>, &Ui) -> R) -> Option<R> {
        if self.busy {
            return None;
        }
        let (gpu, ui) = (self.gpu.as_ref(), &self.ui);
        let editor = self.editor.as_deref_mut()?;
        self.busy = true;
        let result = catch_unwind(AssertUnwindSafe(|| f(editor, gpu, ui)));
        self.busy = false;
        match result {
            Ok(r) => Some(r),
            Err(_) => {
                // A panicking editor stays closed; the plugin keeps working.
                self.close_editor();
                None
            }
        }
    }

    /// Drops the editor (before the surface and its window go away).
    pub fn close_editor(&mut self) {
        let _ = catch_unwind(AssertUnwindSafe(|| self.editor = None));
    }

    /// A panic in the view's drawing code: the view stops drawing (it stays open, empty).
    pub fn mark_broken(&mut self) {
        self.broken = true;
        self.dirty = false;
        eprintln!("ferritron-gpu-view: the editor panicked in its window code and no longer draws");
    }

    /// Whether the view draws on the CPU now.
    #[cfg(windows)]
    pub fn on_cpu(&self) -> bool {
        self.gpu.is_none()
    }

    /// Makes the view draw with the GPU on `side`: the editor is made again on that device, and
    /// the previous one goes. False when the editor cannot be made (the view keeps what it had).
    fn adopt_gpu(&mut self, side: GpuSide) -> bool {
        let Some(editor) = (self.make)(&side.gpu, &self.ui) else { return false };
        self.close_editor();
        self.editor = Some(editor);
        self.gpu = Some(side.gpu);
        self.config = side.config;
        self.lost = side.lost;
        self.generation = side.generation;
        self.dirty = true;
        self.set_pacing();
        true
    }

    /// Draws on the CPU from now on: the GPU's device was lost and could not be made again.
    /// False when the editor cannot draw on the CPU (the view keeps trying the GPU).
    fn fall_back(&mut self) -> bool {
        if !CPU_VIEW {
            return false;
        }
        let Some(editor) = (self.make_cpu)(&self.ui) else { return false };
        self.close_editor();
        self.editor = Some(editor);
        self.fell_back_at = self.generation;
        self.gpu = None;
        self.failures = 0;
        self.retry_at = Some(Instant::now() + RETRY);
        self.dirty = true;
        self.set_pacing();
        eprintln!("ferritron-gpu-view: the GPU is not available, the editor draws on the CPU (the GPU is tried again every 5 seconds)");
        true
    }

    /// Whether the GPU's device can draw now. A lost device is replaced first; after the third
    /// failed attempt in a row the view falls back to the CPU (when the editor can draw there),
    /// and the next attempt waits five seconds. In the CPU mode the answer is always yes.
    fn device_ready(&mut self) -> bool {
        if self.gpu.is_none() {
            return true;
        }
        if !self.lost.load(Ordering::Acquire) {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        if let Some(side) = gpu_side(&self.surface, (self.config.width, self.config.height))
            && self.adopt_gpu(side)
        {
            self.retry_at = None;
            self.failures = 0;
            return true;
        }
        self.failures += 1;
        self.retry_at = Some(Instant::now() + RETRY);
        if self.failures >= FALLBACK_AFTER {
            if self.fall_back() {
                return true;
            }
            if self.failures == FALLBACK_AFTER {
                eprintln!("ferritron-gpu-view: the GPU device was lost and cannot be made again; this editor draws only with the GPU, so it waits");
            }
        }
        false
    }

    /// In the CPU mode, every five seconds: is a GPU device available again? A device of another
    /// generation than the one the view fell back from takes the view back to the GPU.
    fn check_gpu(&mut self) {
        if self.gpu.is_some() || self.forced_cpu || !CPU_VIEW {
            return;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.retry_at = Some(Instant::now() + RETRY);
        let Some(side) = gpu_side(&self.surface, (self.config.width, self.config.height)) else { return };
        if side.generation == self.fell_back_at {
            return;
        }
        if self.adopt_gpu(side) {
            self.retry_at = None;
            eprintln!("ferritron-gpu-view: the GPU is available again, the editor draws on the GPU");
        }
    }

    /// Sets the pacer for the mode the view draws in.
    fn set_pacing(&mut self) {
        let fifo = self.fifo();
        self.pacer.set_refresh(refresh_period(self.refresh_hz, fifo));
    }

    /// Whether the surface presents with `Fifo` only (the frame rate is then halved).
    fn fifo(&self) -> bool {
        self.gpu.is_some() && self.config.present_mode == wgpu::PresentMode::Fifo
    }

    pub fn render(&mut self) {
        self.dirty = false;
        if self.broken {
            return;
        }
        if !self.device_ready() {
            // Try again at a later check (see `tick`).
            self.dirty = true;
            return;
        }
        if self.gpu.is_none() {
            self.render_cpu();
            return;
        }
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                if let Some(gpu) = &self.gpu {
                    self.surface.configure(&gpu.device, &self.config);
                }
                self.dirty = true;
                return;
            }
            _ => return,
        };
        let now = self.clock.now();
        let mut frame = Frame::new(texture.texture.create_view(&wgpu::TextureViewDescriptor::default()), self.config.width, self.config.height, self.scale);
        frame.time = now;
        frame.delta = self.pacer.delta(self.last_frame, now);
        frame.index = self.frames;
        let drawn = self
            .editor(|editor, gpu, ui| match gpu {
                Some(gpu) => {
                    editor.render(gpu, &frame, ui);
                    true
                }
                None => false,
            })
            .unwrap_or(false);
        if drawn && let Some(gpu) = &self.gpu {
            gpu.queue.present(texture);
        }
        self.frames += 1;
        self.last_frame = Some(now);
    }

    /// Draws one frame on the CPU into the view's pixels; the window then paints them (see
    /// `take_paint_request`).
    fn render_cpu(&mut self) {
        let (w, h) = (self.config.width, self.config.height);
        let scale = self.scale;
        let now = self.clock.now();
        // Taken out of the view while the editor draws into it, so the buffer is kept between frames.
        let mut pixels = std::mem::take(&mut self.pixels);
        let drawn = self.editor(|editor, _, ui| editor.render_cpu(ui, (w, h), scale, &mut pixels)).unwrap_or(false);
        self.pixels = pixels;
        if drawn {
            self.pixels_size = (w, h);
            self.cpu_ready = true;
        }
        self.frames += 1;
        self.last_frame = Some(now);
    }

    /// The last CPU frame: its pixels (BGRA, premultiplied), its size in pixels, and the size of
    /// the window to paint it in. None while the view draws on the GPU or no frame was drawn yet.
    #[cfg(windows)]
    pub fn cpu_pixels(&self) -> Option<(&[u8], (u32, u32), (u32, u32))> {
        let (w, h) = self.pixels_size;
        if self.gpu.is_some() || w == 0 || self.pixels.len() != w as usize * h as usize * 4 {
            return None;
        }
        Some((&self.pixels, (w, h), (self.config.width, self.config.height)))
    }

    /// Whether a CPU frame was drawn since the last call: the window then asks the system to paint
    /// it (the paint handler shows the frame, it does not draw).
    #[cfg(windows)]
    pub fn take_paint_request(&mut self) -> bool {
        std::mem::take(&mut self.cpu_ready)
    }

    /// A check, from the view's timer. Draws a frame when something changed or the editor's
    /// [`next_frame`](GpuEditor::next_frame) says it is due (see the `pacing` module), and
    /// returns when to check again (see [`Tick`]).
    pub fn tick(&mut self) -> Tick {
        if self.broken {
            return Tick { render: false, next_check: IDLE_CHECK, due_in: None };
        }
        // In the CPU mode, every five seconds: is the GPU there again?
        self.check_gpu();
        // A lost device is replaced at the next check, whether or not anything changed.
        if self.gpu.is_some() && self.lost.load(Ordering::Acquire) {
            self.dirty = true;
        }
        // New stream values (`[streams]`) are a change worth a frame.
        if self.ui.refresh_streams() {
            self.dirty = true;
        }
        let now = self.ui.snapshot();
        if now != self.last {
            self.last = now;
            self.dirty = true;
        }
        // Asked by another thread, or the instance (track, store) or the
        // list of instances changed.
        if self.ui.take_redraw() {
            self.dirty = true;
        }
        let seen = revisions(&self.ui);
        if seen != self.seen {
            self.seen = seen;
            self.dirty = true;
        }
        self.refresh_countdown = self.refresh_countdown.saturating_sub(1);
        if self.refresh_countdown == 0 {
            self.refresh_countdown = 120;
            self.refresh_due = true;
        }
        let want = self.editor(|editor, _, _| editor.next_frame()).unwrap_or(None);
        let decision = self.pacer.tick(self.clock.now(), self.dirty, want);
        if decision.render {
            // `render` clears `dirty`; a frame that could not be drawn (lost surface) sets it again.
            self.render();
        }
        decision
    }

    /// Whether a check found that the display refresh should be read again (the window may have
    /// moved to another monitor); the caller answers with [`set_refresh_hz`](Canvas::set_refresh_hz).
    pub fn take_refresh_due(&mut self) -> bool {
        std::mem::take(&mut self.refresh_due)
    }

    /// The refresh rate of the display the window is on, in Hz (None: unknown, 60 Hz is taken).
    pub fn set_refresh_hz(&mut self, hz: Option<f64>) {
        self.refresh_hz = hz;
        self.set_pacing();
    }

    /// Whether the editor asked for a frame (by input) that the next check should draw.
    pub fn wants_frame(&self) -> bool {
        self.dirty
    }

    /// A pointer lock or unlock the editor asked for since the last call.
    pub fn take_lock_request(&self) -> Option<bool> {
        self.ui.take_lock_request()
    }

    pub fn set_locked(&self, locked: bool) {
        self.ui.set_locked(locked);
    }

    /// The pointer shape the editor asked for.
    pub fn cursor(&self) -> super::Cursor {
        self.ui.cursor()
    }

    /// The caret of the text the editor takes, if it takes text (see `Ui::set_text_input`).
    pub fn text_input(&self) -> Option<(f64, f64)> {
        self.ui.text_input()
    }

    pub fn input(&mut self, event: Input) {
        if self.broken {
            return;
        }
        if let (Input::Focus(true), Some(instance)) = (&event, self.ui.instance()) {
            instance.editor_focused();
        }
        // Drawn at the next check (see `tick`), not here: pointer moves can come hundreds of times a second.
        if self.editor(|editor, _, ui| editor.input(&event, ui)).unwrap_or(false) {
            self.dirty = true;
        }
    }

    /// A new size in physical pixels, at `scale` physical pixels per logical pixel.
    pub fn resize(&mut self, width: u32, height: u32, scale: f64) {
        self.scale = scale;
        let limit = match &self.gpu {
            Some(gpu) => gpu.device.limits().max_texture_dimension_2d,
            None => CPU_MAX_SIDE,
        };
        self.config.width = clamp_size(width, limit);
        self.config.height = clamp_size(height, limit);
        self.ui.set_size(width as f64 / scale, height as f64 / scale);
        self.ui.set_scale_factor(scale);
        if let Some(gpu) = &self.gpu {
            self.surface.configure(&gpu.device, &self.config);
        }
        self.render();
    }
}

fn revisions(ui: &Ui) -> (u64, u64) {
    (ui.instance().map_or(0, |i| i.revision()), ferritron_core::instances_revision())
}

/// The `open` entry point of every platform: checks the pointers and returns
/// the view `create` makes as an opaque pointer (null on failure or panic).
pub(crate) fn open_view<V>(parent: *mut c_void, host: *const ViewHost, create: impl FnOnce(ViewHost) -> Option<Box<V>>) -> *mut c_void {
    if parent.is_null() || host.is_null() {
        return std::ptr::null_mut();
    }
    let host = unsafe { *host };
    match catch_unwind(AssertUnwindSafe(|| create(host))) {
        Ok(Some(view)) => Box::into_raw(view).cast(),
        _ => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        generation: u64,
        lost: AtomicBool,
    }

    impl Replaceable for Fake {
        fn is_lost(&self) -> bool {
            self.lost.load(Ordering::Acquire)
        }
        fn generation(&self) -> u64 {
            self.generation
        }
    }

    fn fake(generation: u64) -> Option<Fake> {
        Some(Fake { generation, lost: AtomicBool::new(false) })
    }

    #[test]
    fn a_lost_device_is_made_again_with_the_next_generation() {
        let slot: Mutex<Option<Arc<Fake>>> = Mutex::new(None);
        let first = renew(&slot, fake).unwrap();
        assert_eq!(first.generation, 1);
        let same = renew(&slot, |_| panic!("made again while it is fine")).unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        // the device is lost (as its error or loss callback would mark it)
        first.lost.store(true, Ordering::Release);
        let second = renew(&slot, fake).unwrap();
        assert_eq!(second.generation, 2);
        assert!(!Arc::ptr_eq(&first, &second));
        // a failed remake keeps the lost device, so the next try makes it again
        second.lost.store(true, Ordering::Release);
        assert!(renew(&slot, |_| None).is_none());
        let third = renew(&slot, fake).unwrap();
        assert_eq!(third.generation, 3);
    }

    #[test]
    fn the_cpu_configuration_keeps_the_size_within_the_limit() {
        assert_eq!(cpu_config(0, 1).width, 1);
        assert_eq!(cpu_config(u32::MAX, 480).width, CPU_MAX_SIDE);
        assert_eq!(cpu_config(640, 480).height, 480);
    }
}
