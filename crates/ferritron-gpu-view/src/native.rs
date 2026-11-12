//! Editors drawn with Ferritron Native Graphics (feature `fng`).
//!
//! A [`FngEditor`] describes each frame as an `fng::Scene` in logical pixels;
//! [`FngAdapter`] turns it into a [`GpuEditor`] (the renderer, atlas and
//! device handling are done for you):
//!
//! ```toml
//! [view]
//! backend = "gpu"
//! editor  = "FngAdapter<Editor>"
//! ```
//!
//! Anything an editor needs beyond that stays possible: `GpuEditor` is still
//! the open extension point, and `FngAdapter` is only one implementation.

use super::{Frame, Gpu, GpuEditor, Input, Key, Modifiers, Ui};
use ::fng;

/// An editor that draws with FNG.
pub trait FngEditor: 'static {
    /// Creates the editor when the host opens it.
    fn new(ui: &Ui) -> Self
    where
        Self: Sized;

    /// Describes one frame. `scene` arrives empty; coordinates are logical
    /// pixels from the top left, `ui.width()` by `ui.height()`.
    fn draw(&mut self, scene: &mut fng::Scene, ui: &Ui);

    /// The editor's own wgpu drawing, between [`draw`](FngEditor::draw) and
    /// [`draw_over`](FngEditor::draw_over): a shader of its own, a 3D view, video. `frame.target`
    /// already holds `draw`'s picture, so load it (`LoadOp::Load`), do not clear it, and submit
    /// the work before returning. Not called when the view draws on the CPU (no GPU); then
    /// [`draw_without_gpu`](FngEditor::draw_without_gpu) stands in for it. Default: nothing.
    fn render_gpu(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
        let _ = (gpu, frame, ui);
    }

    /// What [`render_gpu`](FngEditor::render_gpu) draws, with FNG, for the CPU mode (no GPU):
    /// drawn between `draw` and `draw_over`. Default: nothing.
    fn draw_without_gpu(&mut self, scene: &mut fng::Scene, ui: &Ui) {
        let _ = (scene, ui);
    }

    /// Drawn over the editor's own GPU drawing (an FNG scene of its own; arrives empty).
    /// Default: nothing, so editors without [`render_gpu`](FngEditor::render_gpu) draw
    /// everything in [`draw`](FngEditor::draw).
    fn draw_over(&mut self, scene: &mut fng::Scene, ui: &Ui) {
        let _ = (scene, ui);
    }

    /// The color the frame starts from.
    fn background(&self) -> fng::Color {
        fng::Color::hex(0x1b1c20)
    }

    /// Mouse and key input (see [`GpuEditor::input`]). Returns whether to redraw.
    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        let _ = (event, ui);
        false
    }

    fn wants_key(&self, key: Key, modifiers: Modifiers) -> bool {
        let _ = (key, modifiers);
        false
    }

    /// Whether to redraw continuously (meters, animation): once per display refresh.
    fn animating(&self) -> bool {
        false
    }

    /// When the next frame is wanted (see [`GpuEditor::next_frame`]); the default is made from
    /// [`animating`](FngEditor::animating).
    fn next_frame(&self) -> Option<std::time::Duration> {
        self.animating().then_some(std::time::Duration::ZERO)
    }
}

/// Makes a [`FngEditor`] a [`GpuEditor`].
///
/// It draws on two backends: the GPU one (`fng::Renderer`, made by [`GpuEditor::new`]) and the
/// CPU one (`fng::CpuRenderer`, used by [`GpuEditor::render_cpu`] when the view has no GPU). The
/// scene is the same for both, so the picture is the same up to the backends' rounding.
pub struct FngAdapter<E: FngEditor> {
    pub editor: E,
    /// The GPU renderer; `None` when the editor was made for the CPU mode.
    renderer: Option<fng::Renderer>,
    scene: fng::Scene,
    /// The scene of [`FngEditor::draw_over`].
    over: fng::Scene,
    /// The CPU renderer, made the first time a frame is drawn on the CPU.
    cpu: Option<fng::CpuRenderer>,
}

impl<E: FngEditor> GpuEditor for FngAdapter<E> {
    fn new(gpu: &Gpu, ui: &Ui) -> Self {
        Self { editor: E::new(ui), renderer: Some(fng::Renderer::new(&gpu.device, gpu.format)), scene: fng::Scene::new(), over: fng::Scene::new(), cpu: None }
    }

    fn new_cpu(ui: &Ui) -> Option<Self> {
        Some(Self { editor: E::new(ui), renderer: None, scene: fng::Scene::new(), over: fng::Scene::new(), cpu: None })
    }

    fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
        let Some(renderer) = self.renderer.as_mut() else { return };
        self.scene.clear();
        self.editor.draw(&mut self.scene, ui);
        let options = fng::RenderOptions::new((frame.width, frame.height)).with_scale(frame.scale as f32).with_clear(self.editor.background());
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fng-editor") });
        renderer.render(&gpu.device, &gpu.queue, &mut encoder, &frame.target, &self.scene, options);
        // `render` writes buffers through the queue: submit before the next `render`.
        gpu.queue.submit([encoder.finish()]);
        self.editor.render_gpu(gpu, frame, ui);
        self.over.clear();
        self.editor.draw_over(&mut self.over, ui);
        if !self.over.is_empty() {
            let options = fng::RenderOptions::new((frame.width, frame.height)).with_scale(frame.scale as f32);
            let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fng-editor-over") });
            renderer.render(&gpu.device, &gpu.queue, &mut encoder, &frame.target, &self.over, options);
            gpu.queue.submit([encoder.finish()]);
        }
    }

    fn render_cpu(&mut self, ui: &Ui, size: (u32, u32), scale: f64, out: &mut Vec<u8>) -> bool {
        // The buffer is resized only when its size changed: a repeat frame allocates nothing.
        let len = size.0 as usize * size.1 as usize * 4;
        if out.len() != len {
            out.resize(len, 0);
        }
        // One scene on the CPU: `draw`, the stand-in for the GPU drawing, then `draw_over`.
        self.scene.clear();
        self.editor.draw(&mut self.scene, ui);
        self.over.clear();
        self.editor.draw_without_gpu(&mut self.over, ui);
        self.editor.draw_over(&mut self.over, ui);
        self.scene.draw_scene(&self.over);
        let options = fng::RenderOptions::new(size).with_scale(scale as f32).with_clear(self.editor.background());
        let cpu = self.cpu.get_or_insert_with(fng::CpuRenderer::new);
        match cpu.render_into(&self.scene, &options, out, fng::PixelFormat::Bgra8Premultiplied) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("ferritron-gpu-view: the CPU frame could not be drawn: {error}");
                false
            }
        }
    }

    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        self.editor.input(event, ui)
    }

    fn wants_key(&self, key: Key, modifiers: Modifiers) -> bool {
        self.editor.wants_key(key, modifiers)
    }

    fn animating(&self) -> bool {
        self.editor.animating()
    }

    fn next_frame(&self) -> Option<std::time::Duration> {
        self.editor.next_frame()
    }
}
