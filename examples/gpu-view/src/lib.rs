//! A gain whose editor is drawn on the GPU: one full-window shader draws a
//! level bar with dB ticks. Drag horizontally (or use the wheel) to change
//! the gain; double-click resets it. With the keyboard focus (click the
//! editor): Up/Right and Down/Left change it by 1 dB (Shift: 0.1 dB), Home
//! resets it, and typing a number then Enter sets it (Escape cancels).

use ferritron::gpu_view::{Frame, Gpu, GpuEditor, Input, Key, Modifiers, Ui, wgpu};
use ferritron::prelude::*;

#[derive(Default)]
struct GpuGain;

#[ferritron::plugin]
impl Process for GpuGain {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
    }
}

const SHADER: &str = r#"
struct Look { size: vec2<f32>, value: f32, hover: f32 };
@group(0) @binding(0) var<uniform> look: Look;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = pos.xy / look.size;
    var color = mix(vec3<f32>(0.11, 0.11, 0.13), vec3<f32>(0.07, 0.07, 0.08), uv.y);
    // The bar: 12% margins left/right, centered vertically.
    let left = 0.08; let right = 0.92;
    let x = (uv.x - left) / (right - left);
    let inside = x >= 0.0 && x <= 1.0 && abs(uv.y - 0.5) < 0.12;
    if (inside) {
        let lit = x <= look.value;
        let accent = mix(vec3<f32>(0.04, 0.52, 1.0), vec3<f32>(0.30, 0.68, 1.0), look.hover);
        color = select(vec3<f32>(0.20, 0.20, 0.23), accent * (0.85 + 0.15 * (1.0 - uv.y)), lit);
    }
    // Ticks every 6 dB over -60..12 (12 steps).
    let tick = fract(x * 12.0 + 0.5) - 0.5;
    if (x >= -0.001 && x <= 1.001 && abs(tick) * look.size.x * (right - left) / 12.0 < 1.0 && uv.y > 0.66 && uv.y < 0.72) {
        color = vec3<f32>(0.55, 0.55, 0.6);
    }
    return vec4<f32>(color, 1.0);
}
"#;

/// The editor: one pipeline and one small uniform buffer.
pub struct Editor {
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    dragging: bool,
    hover: bool,
    /// Digits typed since the last Enter.
    typed: String,
}

impl Editor {
    /// The gain for a horizontal position, matching the bar in the shader.
    fn at(x: f64, ui: &Ui) -> f64 {
        ((x / ui.width() - 0.08) / 0.84).clamp(0.0, 1.0)
    }
}

impl GpuEditor for Editor {
    fn new(gpu: &Gpu, _ui: &Ui) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu-view"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("look"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gpu-view"),
            layout: None,
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(gpu.format.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() }],
        });
        Self { pipeline, uniforms, bind_group, dragging: false, hover: false, typed: String::new() }
    }

    fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
        let look = [frame.width as f32, frame.height as f32, ui.normalized("gain") as f32, if self.hover { 1.0 } else { 0.0 }];
        let bytes: Vec<u8> = look.iter().flat_map(|v| v.to_le_bytes()).collect();
        gpu.queue.write_buffer(&self.uniforms, 0, &bytes);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    fn wants_key(&self, key: Key, _modifiers: Modifiers) -> bool {
        matches!(
            key,
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Home | Key::Enter | Key::Escape | Key::Backspace | Key::Character('0'..='9') | Key::Other(_)
        )
    }

    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        match *event {
            Input::Key { key, pressed: true, modifiers, .. } => {
                let step = if modifiers.shift { 0.1 } else { 1.0 };
                let gain = ui.value("gain");
                match key {
                    Key::Up | Key::Right => ui.edit("gain", (gain + step).clamp(-60.0, 12.0)),
                    Key::Down | Key::Left => ui.edit("gain", (gain - step).clamp(-60.0, 12.0)),
                    Key::Home => ui.edit("gain", 0.0),
                    Key::Backspace => {
                        self.typed.pop();
                    }
                    Key::Escape => self.typed.clear(),
                    Key::Enter => {
                        if let Ok(db) = self.typed.parse::<f64>() {
                            ui.edit("gain", db.clamp(-60.0, 12.0));
                        }
                        self.typed.clear();
                    }
                    _ => return false,
                }
            }
            Input::Char(c) if c.is_ascii_digit() || c == '-' || c == '.' => self.typed.push(c),
            Input::Focus(false) => self.typed.clear(),
            Input::Press { x, .. } => {
                self.dragging = true;
                ui.begin("gain");
                ui.set_normalized("gain", Self::at(x, ui));
            }
            Input::Move { x, y } => {
                let hover = (y / ui.height() - 0.5).abs() < 0.12;
                if self.dragging {
                    ui.set_normalized("gain", Self::at(x, ui));
                } else if hover == self.hover {
                    return false;
                }
                self.hover = hover;
            }
            Input::Release { .. } if self.dragging => {
                self.dragging = false;
                ui.end("gain");
            }
            Input::DoubleClick { .. } => ui.edit("gain", 0.0),
            Input::Wheel { delta, .. } => ui.edit("gain", (ui.value("gain") + delta).clamp(-60.0, 12.0)),
            _ => return false,
        }
        true
    }
}
