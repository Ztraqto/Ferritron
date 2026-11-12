use ferritron::gpu_view::fng::{self, Color, Fonts, Point, Rect, Scene, TextStyle};
use ferritron::gpu_view::{Button, FngAdapter, FngEditor, Input, Ui};
use ferritron::prelude::*;

#[derive(Default)]
struct FngGain {
    ramp: Vec<f32>,
}

#[ferritron::plugin]
impl Process for FngGain {
    fn activate(&mut self, setup: &Setup) {
        self.ramp = vec![0.0; setup.max_block_size];
    }

    fn process(&mut self, ctx: &mut Context) {
        let frames = ctx.io().frames();
        let ramp = &mut self.ramp[..frames];
        if ctx.ramp(&ctx.params().gain, ramp) {
            for c in 0..ctx.io().num_channels() {
                for (s, db) in ctx.io().channel_mut(c).iter_mut().zip(ramp.iter()) {
                    *s *= db_to_gain(*db);
                }
            }
        } else {
            let gain = db_to_gain(ctx.params().gain.value());
            ctx.io().for_each_sample(|s| *s *= gain);
        }
    }
}

/// A slider drawn with FNG: a rounded track, the filled part, and a label (Japanese and English).
struct Editor {
    fonts: Fonts,
    style: TextStyle,
    dragging: bool,
}

impl Editor {
    const MARGIN: f32 = 24.0;

    fn track(ui: &Ui) -> Rect {
        let (w, h) = (ui.width() as f32, ui.height() as f32);
        Rect::new(Self::MARGIN, h / 2.0 - 8.0, w - 2.0 * Self::MARGIN, 16.0)
    }

    fn at(x: f64, ui: &Ui) -> f64 {
        let t = Self::track(ui);
        ((x as f32 - t.x) / t.w).clamp(0.0, 1.0) as f64
    }
}

impl FngEditor for Editor {
    fn new(_ui: &Ui) -> Self {
        let mut fonts = Fonts::new();
        let font = fonts.query(&fng::FontQuery { language: Some("ja"), ..Default::default() });
        Self { fonts, style: TextStyle::new(font, 20.0), dragging: false }
    }

    fn draw(&mut self, scene: &mut Scene, ui: &Ui) {
        let track = Self::track(ui);
        let gain = ui.value("gain");
        let t = ((gain + 60.0) / 72.0).clamp(0.0, 1.0) as f32;
        scene.fill_round_rect(track, 8.0, Color::hex(0x34363d));
        scene.fill_round_rect(Rect::new(track.x, track.y, (track.w * t).max(16.0), track.h), 8.0, Color::hex(0x4f9dff));
        scene.fill_circle(Point::new(track.x + track.w * t, track.y + 8.0), 12.0, Color::hex(0xf2f4f8));
        let label = self.fonts.layout(&format!("ゲイン  {gain:.1} dB"), &self.style);
        scene.text(&label, Point::new(Self::MARGIN, 36.0), Color::hex(0xe6e8ee));
    }

    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        match *event {
            Input::Press { x, button: Button::Left, .. } => {
                self.dragging = true;
                ui.edit_normalized("gain", Self::at(x, ui));
                true
            }
            Input::Move { x, .. } if self.dragging => {
                ui.edit_normalized("gain", Self::at(x, ui));
                true
            }
            Input::Release { .. } => {
                self.dragging = false;
                false
            }
            Input::DoubleClick { .. } => {
                ui.edit("gain", 0.0);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::gpu_view::wgpu;

    #[test]
    fn dragging_moves_the_gain_and_the_frame_shows_it() {
        let ui = Ui::for_tests::<FngGain>(480.0, 240.0);
        let mut editor = Editor::new(&ui);
        assert!(!ui.request_resize(300.0, 200.0), "a test Ui has no host window to resize");
        let track = Editor::track(&ui);
        assert!(editor.input(&Input::Press { x: (track.x + track.w) as f64, y: 120.0, button: Button::Left, modifiers: Default::default() }, &ui));
        assert_eq!(ui.value("gain"), 12.0);

        // Draw it on the GPU into a texture and look for the blue fill and the white knob.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else { return };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 480, height: 240, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let gpu = ferritron::gpu_view::Gpu { device, queue, format };
        let mut adapter_editor = <FngAdapter<Editor> as ferritron::gpu_view::GpuEditor>::new(&gpu, &ui);
        let frame = ferritron::gpu_view::Frame::new(texture.create_view(&Default::default()), 480, 240, 1.0);
        ferritron::gpu_view::GpuEditor::render(&mut adapter_editor, &gpu, &frame, &ui);
        let pixels = fng::gpu::read_texture_rgba8(&gpu.device, &gpu.queue, &texture, (480, 240));
        let at = |x: usize, y: usize| &pixels[(y * 480 + x) * 4..(y * 480 + x) * 4 + 3];
        assert_eq!(at(2, 2), &[0x1b, 0x1c, 0x20], "the background");
        assert_eq!(at(240, 120 - 4), &[0x4f, 0x9d, 0xff], "the filled part of the track");
    }

    /// An editor with its own GPU drawing between two FNG scenes (it counts the GPU passes).
    struct Layered {
        gpu_passes: u32,
    }

    impl FngEditor for Layered {
        fn new(_ui: &Ui) -> Self {
            Self { gpu_passes: 0 }
        }
        fn draw(&mut self, scene: &mut Scene, _ui: &Ui) {
            scene.fill_rect(Rect::new(0.0, 0.0, 60.0, 20.0), Color::hex(0xff0000));
        }
        fn render_gpu(&mut self, _gpu: &ferritron::gpu_view::Gpu, _frame: &ferritron::gpu_view::Frame, _ui: &Ui) {
            self.gpu_passes += 1;
        }
        fn draw_without_gpu(&mut self, scene: &mut Scene, _ui: &Ui) {
            scene.fill_rect(Rect::new(20.0, 0.0, 20.0, 20.0), Color::hex(0x00ff00));
        }
        fn draw_over(&mut self, scene: &mut Scene, _ui: &Ui) {
            scene.fill_rect(Rect::new(0.0, 0.0, 10.0, 20.0), Color::hex(0x0000ff));
        }
    }

    #[test]
    fn draw_over_comes_after_the_editors_gpu_pass_on_both_backends() {
        let ui = Ui::for_tests::<FngGain>(60.0, 20.0);
        // CPU: draw, the stand-in for the GPU pass, then draw_over.
        let mut cpu = <FngAdapter<Layered> as ferritron::gpu_view::GpuEditor>::new_cpu(&ui).unwrap();
        let mut out = Vec::new();
        assert!(ferritron::gpu_view::GpuEditor::render_cpu(&mut cpu, &ui, (60, 20), 1.0, &mut out));
        let bgra = |x: usize| &out[(10 * 60 + x) * 4..(10 * 60 + x) * 4 + 3];
        assert_eq!(bgra(5), &[0xff, 0, 0], "draw_over (blue) on top");
        assert_eq!(bgra(30), &[0, 0xff, 0], "the stand-in (green)");
        assert_eq!(bgra(50), &[0, 0, 0xff], "draw (red)");
        assert_eq!(cpu.editor.gpu_passes, 0);

        // GPU: draw, the editor's pass (counted), then draw_over over it, not cleared.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else { return };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 60, height: 20, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let gpu = ferritron::gpu_view::Gpu { device, queue, format };
        let mut editor = <FngAdapter<Layered> as ferritron::gpu_view::GpuEditor>::new(&gpu, &ui);
        let frame = ferritron::gpu_view::Frame::new(texture.create_view(&Default::default()), 60, 20, 1.0);
        ferritron::gpu_view::GpuEditor::render(&mut editor, &gpu, &frame, &ui);
        let pixels = fng::gpu::read_texture_rgba8(&gpu.device, &gpu.queue, &texture, (60, 20));
        let rgb = |x: usize| &pixels[(10 * 60 + x) * 4..(10 * 60 + x) * 4 + 3];
        assert_eq!(rgb(5), &[0, 0, 0xff], "draw_over on top");
        assert_eq!(rgb(30), &[0xff, 0, 0], "no stand-in on the GPU; draw's picture kept");
        assert_eq!(editor.editor.gpu_passes, 1);
    }
}
