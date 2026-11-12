//! The wgpu backend: [`Renderer`] draws a [`Scene`] into a texture the app owns.

use std::collections::HashMap;

use crate::image::ImageStore;
use crate::prepare::{Instance, PlanStats, Planner, RAMP_ROWS, RAMP_W, Target, TexRef};
use crate::render::RenderOptions;
use crate::scene::Scene;

const SHADER: &str = include_str!("shader.wgsl");
const UNIFORM_STRIDE: u64 = 256;
const INTERMEDIATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct GpuTarget {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    size: (u32, u32),
}

struct GpuImage {
    _tex: wgpu::Texture,
    bind: wgpu::BindGroup,
    generation: u64,
}

/// Draws scenes with wgpu. FNG never creates a device: pass the app's `Device`, `Queue` and
/// the `TextureView` to draw into.
pub struct Renderer {
    format: wgpu::TextureFormat,
    images: ImageStore,
    planner: Planner,
    shader: wgpu::ShaderModule,
    bgl0: wgpu::BindGroupLayout,
    bgl1: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    sampler_lin: wgpu::Sampler,
    sampler_near: wgpu::Sampler,
    atlas_tex: wgpu::Texture,
    atlas_view: wgpu::TextureView,
    atlas_size: u32,
    ramp_tex: wgpu::Texture,
    ramp_view: wgpu::TextureView,
    uniforms: wgpu::Buffer,
    uniform_slots: u64,
    bg0: wgpu::BindGroup,
    bg0_dirty: bool,
    instances: wgpu::Buffer,
    instance_cap: u64,
    dummy_bind: wgpu::BindGroup,
    gpu_images: HashMap<u32, GpuImage>,
    layer_tex: HashMap<u32, GpuTarget>,
    transient: Vec<GpuTarget>,
    /// For sRGB targets: the scene is drawn here (blending in sRGB space like on any other
    /// target) and converted into the real target at the end.
    proxy: Option<GpuTarget>,
}

fn create_texture(device: &wgpu::Device, label: &str, size: (u32, u32), format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size.0.max(1), height: size.1.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

impl Renderer {
    /// A renderer for targets of `format` (the format of the `TextureView` passed to
    /// [`Renderer::render`]). `*Srgb` and plain formats look the same.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Renderer {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("fng"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let bgl0 = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fng-globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: true, min_binding_size: wgpu::BufferSize::new(32) },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
                wgpu::BindGroupLayoutEntry { binding: 4, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let bgl1 = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("fng-image"), entries: &[tex_entry(0)] });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("fng"), bind_group_layouts: &[Some(&bgl0), Some(&bgl1)], immediate_size: 0 });
        // images have mip levels (see `ImageStore`); the linear sampler blends between them
        let sampler_lin = device.create_sampler(&wgpu::SamplerDescriptor { label: Some("fng-linear"), mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::MipmapFilterMode::Linear, ..Default::default() });
        let sampler_near = device.create_sampler(&wgpu::SamplerDescriptor { label: Some("fng-nearest"), mag_filter: wgpu::FilterMode::Nearest, min_filter: wgpu::FilterMode::Nearest, ..Default::default() });
        let mut planner = Planner::new();
        // layer textures are kept within the device's texture limit (see `Planner::max_dim`)
        planner.max_dim = device.limits().max_texture_dimension_2d;
        let atlas_size = planner.atlas.size;
        let atlas_tex = create_texture(device, "fng-atlas", (atlas_size, atlas_size), wgpu::TextureFormat::R8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let atlas_view = atlas_tex.create_view(&Default::default());
        let ramp_tex = create_texture(device, "fng-ramps", (RAMP_W as u32, RAMP_ROWS as u32), wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let ramp_view = ramp_tex.create_view(&Default::default());
        let uniform_slots = 8;
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("fng-uniforms"), size: UNIFORM_STRIDE * uniform_slots, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let instance_cap = 64 * std::mem::size_of::<Instance>() as u64;
        let instances = device.create_buffer(&wgpu::BufferDescriptor { label: Some("fng-instances"), size: instance_cap, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let dummy = create_texture(device, "fng-dummy", (1, 1), INTERMEDIATE_FORMAT, wgpu::TextureUsages::TEXTURE_BINDING);
        let dummy_view = dummy.create_view(&Default::default());
        let dummy_bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("fng-dummy"), layout: &bgl1, entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&dummy_view) }] });
        let bg0 = Self::make_bg0(device, &bgl0, &uniforms, &atlas_view, &ramp_view, &sampler_lin, &sampler_near);
        let mut r = Renderer {
            format,
            images: ImageStore::new(),
            planner,
            shader,
            bgl0,
            bgl1,
            layout,
            pipelines: HashMap::new(),
            sampler_lin,
            sampler_near,
            atlas_tex,
            atlas_view,
            atlas_size,
            ramp_tex,
            ramp_view,
            uniforms,
            uniform_slots,
            bg0,
            bg0_dirty: false,
            instances,
            instance_cap,
            dummy_bind,
            gpu_images: HashMap::new(),
            layer_tex: HashMap::new(),
            transient: Vec::new(),
            proxy: None,
        };
        r.pipeline(device, format);
        r.pipeline(device, INTERMEDIATE_FORMAT);
        r
    }

    fn make_bg0(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        uniforms: &wgpu::Buffer,
        atlas: &wgpu::TextureView,
        ramp: &wgpu::TextureView,
        lin: &wgpu::Sampler,
        near: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fng-globals"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: uniforms, offset: 0, size: wgpu::BufferSize::new(32) }) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(atlas) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(ramp) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(lin) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(near) },
            ],
        })
    }

    fn pipeline(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if self.pipelines.contains_key(&format) {
            return;
        }
        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4,
            7 => Float32x4, 8 => Float32x4, 9 => Float32x4, 10 => Float32x4, 11 => Float32x4, 12 => Float32x4
        ];
        let premul = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add };
        let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fng"),
            layout: Some(&self.layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Instance>() as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &attrs })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState { color: premul, alpha: premul }), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.insert(format, p);
    }

    /// The images this renderer can draw (see [`ImageStore`]).
    pub fn images(&mut self) -> &mut ImageStore {
        &mut self.images
    }

    /// Counters of the last frame.
    pub fn stats(&self) -> PlanStats {
        self.planner.stats
    }

    /// Draw `scene` into `target` (a view of a texture with the renderer's format and size
    /// `opts.size_px`), recording into `encoder`. The commands use `device` resources and `queue`
    /// uploads: submit the encoder after this call.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, scene: &Scene, opts: RenderOptions) {
        self.planner.plan(scene, &self.images, &opts);
        self.upload_atlas_and_ramps(device, queue);
        self.ensure_targets(device);
        self.upload_images(device, queue);
        self.upload_instances(device, queue, &opts);

        let srgb = self.format.is_srgb();
        let isz = std::mem::size_of::<Instance>() as u64;
        let mut base = 0u32;
        let npass = self.planner.passes.len();
        for (pi, pass) in self.planner.passes.iter().enumerate() {
            let is_root = pass.target == Target::Root;
            let (view, format): (&wgpu::TextureView, wgpu::TextureFormat) = match pass.target {
                Target::Root if srgb => (&self.proxy.as_ref().unwrap().view, INTERMEDIATE_FORMAT),
                Target::Root => (target, self.format),
                Target::Transient(s) => (&self.transient[s as usize].view, INTERMEDIATE_FORMAT),
                Target::Layer(id) => (&self.layer_tex[&id].view, INTERMEDIATE_FORMAT),
            };
            let load = if is_root && !srgb {
                match opts.clear {
                    Some(c) => wgpu::LoadOp::Clear(wgpu::Color { r: (c.r * c.a) as f64, g: (c.g * c.a) as f64, b: (c.b * c.a) as f64, a: c.a as f64 }),
                    None => wgpu::LoadOp::Load,
                }
            } else if is_root {
                // the proxy starts from the clear color (or transparent)
                match opts.clear {
                    Some(c) => wgpu::LoadOp::Clear(wgpu::Color { r: (c.r * c.a) as f64, g: (c.g * c.a) as f64, b: (c.b * c.a) as f64, a: c.a as f64 }),
                    None => wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                }
            } else {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fng"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !pass.batches.is_empty() {
                rp.set_pipeline(&self.pipelines[&format]);
                rp.set_bind_group(0, &self.bg0, &[(pi as u64 * UNIFORM_STRIDE) as u32]);
                rp.set_vertex_buffer(0, self.instances.slice(base as u64 * isz..(base as u64 + pass.instances.len() as u64) * isz));
                for b in &pass.batches {
                    let bind = match b.tex {
                        TexRef::None => &self.dummy_bind,
                        TexRef::Image(id) => &self.gpu_images[&id].bind,
                        TexRef::Transient(s) => &self.transient[s as usize].bind,
                        TexRef::Layer(id) => &self.layer_tex[&id].bind,
                    };
                    rp.set_bind_group(1, bind, &[]);
                    rp.draw(0..4, b.start..b.end);
                }
            }
            drop(rp);
            base += pass.instances.len() as u32;
        }
        if srgb {
            // convert the proxy into the sRGB target
            let load = if opts.clear.is_some() { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) } else { wgpu::LoadOp::Load };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fng-srgb"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: target, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipelines[&self.format]);
            rp.set_bind_group(0, &self.bg0, &[(npass as u64 * UNIFORM_STRIDE) as u32]);
            rp.set_vertex_buffer(0, self.instances.slice(base as u64 * isz..(base as u64 + 1) * isz));
            rp.set_bind_group(1, &self.proxy.as_ref().unwrap().bind, &[]);
            rp.draw(0..4, 0..1);
            drop(rp);
        }
        self.planner.recycle();
    }

    fn upload_atlas_and_ramps(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let atlas = &mut self.planner.atlas;
        if atlas.size != self.atlas_size {
            self.atlas_size = atlas.size;
            self.atlas_tex = create_texture(device, "fng-atlas", (atlas.size, atlas.size), wgpu::TextureFormat::R8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            self.atlas_view = self.atlas_tex.create_view(&Default::default());
            self.bg0_dirty = true;
        }
        let size = atlas.size;
        let (full, dirty) = atlas.take_dirty();
        if full {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &self.atlas_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &atlas.data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size), rows_per_image: Some(size) },
                wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            );
        } else {
            for (x, y, w, h) in dirty {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &self.atlas_tex, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                    &atlas.data[(y * size + x) as usize..],
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size), rows_per_image: Some(h) },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
            }
        }
        let ramps = &mut self.planner.ramps;
        if ramps.full_dirty {
            ramps.full_dirty = false;
            ramps.dirty.clear();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &self.ramp_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &ramps.data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(RAMP_W as u32 * 4), rows_per_image: Some(RAMP_ROWS as u32) },
                wgpu::Extent3d { width: RAMP_W as u32, height: RAMP_ROWS as u32, depth_or_array_layers: 1 },
            );
        } else {
            for r in ramps.dirty.drain(..) {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &self.ramp_tex, mip_level: 0, origin: wgpu::Origin3d { x: 0, y: r, z: 0 }, aspect: wgpu::TextureAspect::All },
                    &ramps.data[r as usize * RAMP_W * 4..(r as usize + 1) * RAMP_W * 4],
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(RAMP_W as u32 * 4), rows_per_image: Some(1) },
                    wgpu::Extent3d { width: RAMP_W as u32, height: 1, depth_or_array_layers: 1 },
                );
            }
        }
    }

    fn target_bind(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("fng-target"), layout: &self.bgl1, entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) }] })
    }

    /// Create or resize the textures of group and layer passes.
    fn ensure_targets(&mut self, device: &wgpu::Device) {
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        if self.format.is_srgb() {
            let size = self.planner.passes.last().map_or((1, 1), |p| p.size);
            if self.proxy.as_ref().is_none_or(|p| p.size != size) {
                let tex = create_texture(device, "fng-proxy", size, INTERMEDIATE_FORMAT, usage);
                let view = tex.create_view(&Default::default());
                let bind = self.target_bind(device, &view);
                self.proxy = Some(GpuTarget { _tex: tex, view, bind, size });
            }
        }
        // transient slots
        for pass in &self.planner.passes {
            match pass.target {
                Target::Transient(s) => {
                    let s = s as usize;
                    while self.transient.len() <= s {
                        let tex = create_texture(device, "fng-group", (1, 1), INTERMEDIATE_FORMAT, usage);
                        let view = tex.create_view(&Default::default());
                        let bind = self.target_bind(device, &view);
                        self.transient.push(GpuTarget { _tex: tex, view, bind, size: (1, 1) });
                    }
                    if self.transient[s].size != pass.size {
                        let tex = create_texture(device, "fng-group", pass.size, INTERMEDIATE_FORMAT, usage);
                        let view = tex.create_view(&Default::default());
                        let bind = self.target_bind(device, &view);
                        self.transient[s] = GpuTarget { _tex: tex, view, bind, size: pass.size };
                    }
                }
                Target::Layer(id) => {
                    let need = match self.layer_tex.get(&id) {
                        Some(t) => t.size != pass.size,
                        None => true,
                    };
                    if need {
                        let tex = create_texture(device, "fng-layer", pass.size, INTERMEDIATE_FORMAT, usage);
                        let view = tex.create_view(&Default::default());
                        let bind = self.target_bind(device, &view);
                        self.layer_tex.insert(id, GpuTarget { _tex: tex, view, bind, size: pass.size });
                    }
                }
                Target::Root => {}
            }
        }
        // drop textures of layers that are no longer drawn
        let live: Vec<u32> = self.planner.layers.iter().map(|l| l.id).collect();
        self.layer_tex.retain(|id, _| live.contains(id));
        self.transient.truncate(self.planner.transient_count as usize);
    }

    fn upload_images(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        for pass in &self.planner.passes {
            for b in &pass.batches {
                let TexRef::Image(id) = b.tex else { continue };
                let Some(img) = self.images.get(crate::scene::ImageId(id)) else { continue };
                let up_to_date = self.gpu_images.get(&id).is_some_and(|g| g.generation == img.generation);
                if up_to_date {
                    continue;
                }
                let tex = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("fng-image"),
                    size: wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
                    mip_level_count: 1 + img.mips.len() as u32,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: INTERMEDIATE_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                // level 0 is the image, the rest is the CPU-built chain (the same texels the CPU samples)
                let levels = std::iter::once((img.width, img.height, img.premul.as_slice())).chain(img.mips.iter().map(|(w, h, d)| (*w, *h, d.as_slice())));
                for (level, (w, h, data)) in levels.enumerate() {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: level as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                        data,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                }
                let view = tex.create_view(&Default::default());
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("fng-image"), layout: &self.bgl1, entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }] });
                self.gpu_images.insert(id, GpuImage { _tex: tex, bind, generation: img.generation });
            }
        }
        // forget textures of removed images
        let images = &self.images;
        self.gpu_images.retain(|id, _| images.get(crate::scene::ImageId(*id)).is_some());
    }

    fn upload_instances(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, opts: &RenderOptions) {
        let isz = std::mem::size_of::<Instance>() as u64;
        let total: u64 = self.planner.passes.iter().map(|p| p.instances.len() as u64).sum::<u64>() + 1;
        if total * isz > self.instance_cap {
            self.instance_cap = (total * isz).next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor { label: Some("fng-instances"), size: self.instance_cap, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        }
        let passes = self.planner.passes.len() as u64 + 1;
        if passes > self.uniform_slots {
            self.uniform_slots = passes.next_power_of_two();
            self.uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("fng-uniforms"), size: UNIFORM_STRIDE * self.uniform_slots, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            self.bg0_dirty = true;
        }
        if self.bg0_dirty {
            self.bg0 = Self::make_bg0(device, &self.bgl0, &self.uniforms, &self.atlas_view, &self.ramp_view, &self.sampler_lin, &self.sampler_near);
            self.bg0_dirty = false;
        }
        let mut offset = 0u64;
        for (i, pass) in self.planner.passes.iter().enumerate() {
            if !pass.instances.is_empty() {
                queue.write_buffer(&self.instances, offset * isz, bytemuck::cast_slice(&pass.instances));
            }
            offset += pass.instances.len() as u64;
            let u: [f32; 8] = [pass.size.0 as f32, pass.size.1 as f32, self.atlas_size as f32, RAMP_ROWS as f32, 0.0, 0.0, 0.0, 0.0];
            queue.write_buffer(&self.uniforms, i as u64 * UNIFORM_STRIDE, bytemuck::cast_slice(&u));
        }
        // the sRGB conversion pass: one full-target quad sampling the proxy
        if self.format.is_srgb() {
            let (w, h) = opts.size_px;
            let mut inst = Instance::default();
            inst.bbox = [0.0, 0.0, w as f32, h as f32];
            inst.inv0 = [1.0, 0.0, 0.0, 1.0];
            inst.inv1 = [0.0, 0.0, 1.0, crate::prepare::KIND_FULL];
            inst.paint = [crate::prepare::PAINT_IMAGE, 0.0, 1.0, 0.0];
            inst.pinv0 = [1.0, 0.0, 0.0, 1.0];
            inst.pinv1 = [0.0, 0.0, w as f32, h as f32];
            inst.color = [1.0; 4];
            inst.clip = [0.0, 0.0, w as f32, h as f32];
            inst.rclip2 = [-1.0, 0.0, 0.0, 0.0];
            queue.write_buffer(&self.instances, offset * isz, bytemuck::bytes_of(&inst));
            let u: [f32; 8] = [w as f32, h as f32, self.atlas_size as f32, RAMP_ROWS as f32, 1.0, 0.0, 0.0, 0.0];
            queue.write_buffer(&self.uniforms, self.planner.passes.len() as u64 * UNIFORM_STRIDE, bytemuck::cast_slice(&u));
        }
    }
}

/// Copy a texture of `Rgba8Unorm` / `Bgra8Unorm` (and their sRGB forms) back to the CPU and
/// return its bytes as RGBA rows (straight alpha is **not** restored: the pixels are exactly
/// what the GPU stored, premultiplied). Blocks until the GPU is done. For tests and screenshots.
pub fn read_texture_rgba8(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, size: (u32, u32)) -> Vec<u8> {
    let bpr = (size.0 * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("fng-readback"), size: (bpr * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(size.1) } },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buf.map_async(wgpu::MapMode::Read, .., |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let data = buf.get_mapped_range(..).expect("the readback buffer is mapped");
    let mut out = Vec::with_capacity((size.0 * size.1 * 4) as usize);
    for y in 0..size.1 {
        let row = &data[(y * bpr) as usize..(y * bpr + size.0 * 4) as usize];
        out.extend_from_slice(row);
    }
    drop(data);
    buf.unmap();
    let swap = matches!(texture.format(), wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    if swap {
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    out
}
