//! What every platform's view shares: the wgpu instance and device of the
//! process, how a surface is configured, and the [`Canvas`] that draws the
//! editor into it.

use super::{Frame, Gpu, GpuEditor, Input, Ui};
use ferritron_core::__private::PluginMeta;
use ferritron_core::Process;
use ferritron_core::abi::ViewHost;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

pub(crate) fn instance() -> &'static wgpu::Instance {
    static INSTANCE: OnceLock<wgpu::Instance> = OnceLock::new();
    INSTANCE.get_or_init(|| wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()))
}

/// The device shared by all editors of this process: one that can present
/// to the first editor's window.
pub(crate) struct Shared {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

pub(crate) fn shared(surface: &wgpu::Surface) -> Option<&'static Shared> {
    static SHARED: OnceLock<Option<Shared>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let adapter =
                pollster::block_on(instance().request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(surface), ..Default::default() }))
                    .ok()?;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("ferritron-gpu-view"), ..Default::default() })).ok()?;
            Some(Shared { adapter, device, queue })
        })
        .as_ref()
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
    Some(wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: width.max(1),
        height: height.max(1),
        present_mode,
        desired_maximum_frame_latency: 1,
        alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    })
}

/// An editor drawing into a surface: what is the same on every platform.
/// The platform view owns one and forwards its events to it.
pub(crate) struct Canvas {
    /// Declared first: dropped before the surface.
    editor: Option<Box<dyn GpuEditor>>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    ui: Ui,
    /// Physical pixels per logical pixel.
    pub scale: f64,
    last: Vec<f64>,
    dirty: bool,
    /// Set while the editor runs, against re-entry from nested events.
    busy: bool,
}

impl Canvas {
    /// Configures `surface` for `pixels` (physical) and creates editor `E`
    /// with a logical size of `pixels / scale`.
    pub fn new<P: Process + PluginMeta, E: GpuEditor>(surface: wgpu::Surface<'static>, host: ViewHost, pixels: (u32, u32), scale: f64) -> Option<Self> {
        let shared = shared(&surface)?;
        let config = surface_config(&surface, shared, pixels.0, pixels.1)?;
        surface.configure(&shared.device, &config);
        let gpu = Gpu { device: shared.device.clone(), queue: shared.queue.clone(), format: config.format };
        let ui = Ui::new::<P>(host, (pixels.0 as f64 / scale, pixels.1 as f64 / scale));
        let editor: Box<dyn GpuEditor> = Box::new(catch_unwind(AssertUnwindSafe(|| E::new(&gpu, &ui))).ok()?);
        let last = ui.snapshot();
        Some(Self { editor: Some(editor), surface, config, gpu, ui, scale, last, dirty: true, busy: false })
    }

    /// Runs `f` on the editor, unless it panicked before or is already running.
    pub fn editor<R>(&mut self, f: impl FnOnce(&mut dyn GpuEditor, &Gpu, &Ui) -> R) -> Option<R> {
        if self.busy {
            return None;
        }
        let (gpu, ui) = (&self.gpu, &self.ui);
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

    pub fn render(&mut self) {
        self.dirty = false;
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                self.dirty = true;
                return;
            }
            _ => return,
        };
        let frame = Frame {
            target: texture.texture.create_view(&wgpu::TextureViewDescriptor::default()),
            width: self.config.width,
            height: self.config.height,
            scale: self.scale,
        };
        if self.editor(|editor, gpu, ui| editor.render(gpu, &frame, ui)).is_some() {
            self.gpu.queue.present(texture);
        }
    }

    /// Called about 60 times per second: redraws when parameters changed or
    /// the editor animates.
    pub fn tick(&mut self) {
        let now = self.ui.snapshot();
        if now != self.last {
            self.last = now;
            self.dirty = true;
        }
        if self.editor(|editor, _, _| editor.animating()).unwrap_or(false) {
            self.dirty = true;
        }
        if self.dirty {
            self.render();
        }
    }

    pub fn input(&mut self, event: Input) {
        if self.editor(|editor, _, ui| editor.input(&event, ui)).unwrap_or(false) {
            self.render();
        }
    }

    /// A new size in physical pixels, at `scale` physical pixels per logical pixel.
    pub fn resize(&mut self, width: u32, height: u32, scale: f64) {
        self.scale = scale;
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.ui.set_size(width as f64 / scale, height as f64 / scale);
        self.surface.configure(&self.gpu.device, &self.config);
        self.render();
    }
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
