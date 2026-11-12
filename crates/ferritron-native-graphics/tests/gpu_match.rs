//! GPU backend (M2): the same scene drawn with wgpu and with the CPU backend must agree.
//! Skipped (with a message) when the machine has no usable GPU adapter.
mod common;
use common::*;
use fng::*;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: String,
}

fn gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let info = format!("{:?}", adapter.get_info());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("fng-test"), ..Default::default() })).ok()?;
    Some(Gpu { device, queue, info })
}

/// Render with wgpu into a texture of `format`, read it back and convert to straight alpha.
fn render_gpu(g: &Gpu, r: &mut Renderer, format: wgpu::TextureFormat, scene: &Scene, size: (u32, u32), scale: f32) -> RgbaImage {
    let tex = g.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render(&g.device, &g.queue, &mut enc, &view, scene, RenderOptions::new(size).with_scale(scale));
    g.queue.submit([enc.finish()]);
    let mut data = gpu::read_texture_rgba8(&g.device, &g.queue, &tex, size);
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as f32 / 255.0;
        if a > 0.0 {
            for c in &mut px[..3] {
                *c = ((*c as f32 / 255.0 / a).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
    }
    RgbaImage { width: size.0, height: size.1, data }
}

fn cpu_image(scene: &Scene, size: (u32, u32), scale: f32) -> RgbaImage {
    let mut cpu = CpuRenderer::new();
    let (px, w, h) = test_image();
    cpu.images().insert(&px, w, h);
    cpu.render(scene, size, scale)
}

fn gpu_renderer(g: &Gpu, format: wgpu::TextureFormat) -> Renderer {
    let mut r = Renderer::new(&g.device, format);
    let (px, w, h) = test_image();
    r.images().insert(&px, w, h);
    r
}

/// Count pixels that differ by more than `limit` in any channel.
fn count_over(a: &RgbaImage, b: &RgbaImage, limit: u8) -> usize {
    a.data.chunks_exact(4).zip(b.data.chunks_exact(4)).filter(|(x, y)| x.iter().zip(y.iter()).any(|(p, q)| p.abs_diff(*q) > limit)).count()
}

fn compare(name: &str, scene: &Scene, scale: f32, mean_allowed: f64, max_bad_pixels: usize) {
    let Some(g) = gpu() else {
        eprintln!("skipped {name}: no GPU adapter");
        return;
    };
    let size = ((W as f32 * scale) as u32, (H as f32 * scale) as u32);
    let mut r = gpu_renderer(&g, wgpu::TextureFormat::Rgba8Unorm);
    let a = render_gpu(&g, &mut r, wgpu::TextureFormat::Rgba8Unorm, scene, size, scale);
    let b = cpu_image(scene, size, scale);
    let (max, mean) = diff(&a, &b);
    let bad = count_over(&a, &b, 48);
    eprintln!("{name}: adapter {}: max diff {max}, mean {mean:.4}, pixels off by more than 48: {bad}", g.info);
    if mean > mean_allowed || bad > max_bad_pixels {
        let _ = a.save_png(std::env::temp_dir().join(format!("fng-gpu-{name}.png")));
        let _ = b.save_png(std::env::temp_dir().join(format!("fng-cpu-{name}.png")));
        panic!("{name}: GPU and CPU pictures differ (mean {mean:.4} > {mean_allowed} or {bad} bad pixels > {max_bad_pixels}); saved to the temp folder");
    }
}

/// Like `compare`, but the anti-aliased fringe is left out of the bad-pixel count: where one side
/// has an alpha of 2/255 or less, the straight color is recovered from almost nothing, so a
/// coverage difference of a fraction of a level shows as a large color difference. The layer
/// zoom has such pixels only at the corners of its rounded rectangle (the GPU's and the CPU's
/// coverage round differently there). The mean still counts every pixel.
fn compare_fringe(name: &str, scene: &Scene, scale: f32, mean_allowed: f64, max_bad_pixels: usize) {
    let Some(g) = gpu() else {
        eprintln!("skipped {name}: no GPU adapter");
        return;
    };
    let size = ((W as f32 * scale) as u32, (H as f32 * scale) as u32);
    let mut r = gpu_renderer(&g, wgpu::TextureFormat::Rgba8Unorm);
    let a = render_gpu(&g, &mut r, wgpu::TextureFormat::Rgba8Unorm, scene, size, scale);
    let b = cpu_image(scene, size, scale);
    let (max, mean) = diff(&a, &b);
    let bad = a.data.chunks_exact(4).zip(b.data.chunks_exact(4)).filter(|(x, y)| x[3] > 2 && y[3] > 2 && x.iter().zip(y.iter()).any(|(p, q)| p.abs_diff(*q) > 48)).count();
    let fringe = a.data.chunks_exact(4).zip(b.data.chunks_exact(4)).filter(|(x, y)| (x[3] <= 2 || y[3] <= 2) && x.iter().zip(y.iter()).any(|(p, q)| p.abs_diff(*q) > 48)).count();
    eprintln!("{name}: adapter {}: max diff {max}, mean {mean:.4}, pixels off by more than 48 with alpha > 2: {bad} (fringe: {fringe})", g.info);
    if mean > mean_allowed || bad > max_bad_pixels {
        panic!("{name}: GPU and CPU pictures differ (mean {mean:.4} > {mean_allowed} or {bad} bad pixels > {max_bad_pixels})");
    }
}

#[test]
fn gpu_matches_cpu_shapes() {
    compare("shapes", &scene_shapes(), 1.0, 0.4, 20);
}

#[test]
fn gpu_matches_cpu_shapes_2x() {
    compare("shapes_2x", &scene_shapes(), 2.0, 0.3, 40);
}

#[test]
fn gpu_matches_cpu_paths() {
    compare("paths", &scene_paths(), 1.0, 0.2, 10);
}

#[test]
fn gpu_matches_cpu_paints() {
    compare("paints", &scene_paints(), 1.0, 0.3, 10);
}

#[test]
fn gpu_matches_cpu_state() {
    compare("state", &scene_state(), 1.0, 0.3, 10);
}

#[test]
fn gpu_matches_cpu_layer_under_a_zoom() {
    // the layer texture is drawn at the zoom of its transform on both backends
    let mut s = Scene::new();
    s.push_transform(Affine::scale(2.5, 2.5));
    s.layer(LayerId(9), (30.0, 20.0), 1, |l| {
        l.fill_round_rect(Rect::new(2.0, 2.0, 22.0, 14.0), 4.0, ACCENT);
        l.stroke_circle(Point::new(20.0, 10.0), 6.0, &Stroke::width(1.5), FG);
    });
    s.pop_transform();
    compare_fringe("layer_zoom", &s, 1.0, 0.3, 0);
}

#[test]
fn gpu_matches_cpu_text() {
    let mut fonts = Fonts::new();
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], language: Some("ja"), ..Default::default() });
    if chain.0.len() < 2 {
        eprintln!("skipped: no fonts");
        return;
    }
    let scene = text_scene(&mut fonts, &chain);
    compare("text", &scene, 1.0, 0.3, 20);
}

fn text_scene(fonts: &mut Fonts, chain: &FontChain) -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    let l = fonts.layout("音量 Volume -6.0 dB", &TextStyle::new(chain.clone(), 13.0));
    s.text(&l, Point::new(10.0, 10.0), FG);
    let l = fonts.layout("周波数フィルター Frequency", &TextStyle::new(chain.clone(), 20.0));
    s.text(&l, Point::new(10.3, 34.6), ACCENT);
    s.push_transform(Affine::rotate(-0.3).then(&Affine::translate(20.0, 150.0)));
    let l = fonts.layout("Rotated 回転", &TextStyle::new(chain.clone(), 16.0));
    s.text(&l, Point::ZERO, Paint::Linear { start: Point::new(0.0, 0.0), end: Point::new(100.0, 0.0), stops: Stops::two(ACCENT, BLUE) });
    s.pop_transform();
    s
}

#[test]
fn srgb_targets_look_like_plain_targets_for_solid_areas() {
    let Some(g) = gpu() else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, 16.0, 16.0), Color::rgb8(200, 100, 50));
    s.fill_rect(Rect::new(4.0, 4.0, 8.0, 8.0), Color::rgb8(30, 60, 90).with_alpha(0.5));
    let mut plain = gpu_renderer(&g, wgpu::TextureFormat::Rgba8Unorm);
    let mut srgb = gpu_renderer(&g, wgpu::TextureFormat::Rgba8UnormSrgb);
    let a = render_gpu(&g, &mut plain, wgpu::TextureFormat::Rgba8Unorm, &s, (16, 16), 1.0);
    // reading an sRGB texture back as bytes gives the sRGB-encoded values, which is what the
    // plain target holds directly: both targets must look the same, translucent blends too
    let b = render_gpu(&g, &mut srgb, wgpu::TextureFormat::Rgba8UnormSrgb, &s, (16, 16), 1.0);
    assert_eq!(a.pixel(1, 1), b.pixel(1, 1));
    let (pa, pb) = (a.pixel(8, 8), b.pixel(8, 8));
    for i in 0..3 {
        assert!((pa[i] as i32 - pb[i] as i32).abs() <= 2, "{pa:?} vs {pb:?}");
    }
}

#[test]
fn clear_color_is_applied_and_load_keeps_content() {
    let Some(g) = gpu() else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let mut r = gpu_renderer(&g, wgpu::TextureFormat::Rgba8Unorm);
    let tex = g.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("t"),
        size: wgpu::Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, 4.0, 8.0), Color::rgb8(255, 0, 0));
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render(&g.device, &g.queue, &mut enc, &view, &s, RenderOptions::new((8, 8)).with_clear(Color::rgb8(0, 0, 255)));
    g.queue.submit([enc.finish()]);
    let px = gpu::read_texture_rgba8(&g.device, &g.queue, &tex, (8, 8));
    assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    assert_eq!(&px[(7 * 4)..(7 * 4 + 4)], &[0, 0, 255, 255]);
    // second frame without a clear draws over the first
    let mut s2 = Scene::new();
    s2.fill_rect(Rect::new(0.0, 0.0, 8.0, 4.0), Color::rgb8(0, 255, 0));
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render(&g.device, &g.queue, &mut enc, &view, &s2, RenderOptions::new((8, 8)));
    g.queue.submit([enc.finish()]);
    let px = gpu::read_texture_rgba8(&g.device, &g.queue, &tex, (8, 8));
    assert_eq!(&px[0..4], &[0, 255, 0, 255]);
    assert_eq!(&px[((7 * 8 + 7) * 4)..((7 * 8 + 7) * 4 + 4)], &[0, 0, 255, 255], "the bottom right was not touched");
    assert_eq!(&px[((7 * 8) * 4)..((7 * 8) * 4 + 4)], &[255, 0, 0, 255], "the first frame stays under the second");
}

#[test]
fn layers_are_reused_across_frames_on_the_gpu() {
    let Some(g) = gpu() else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let mut r = gpu_renderer(&g, wgpu::TextureFormat::Rgba8Unorm);
    let mut s = Scene::new();
    let mut calls = 0;
    let mut passes = Vec::new();
    for version in [1u64, 1, 2] {
        s.clear();
        s.layer(LayerId(1), (16.0, 16.0), version, |l| {
            calls += 1;
            l.fill_rect(Rect::new(0.0, 0.0, 16.0, 16.0), if version == 1 { Color::rgb8(255, 0, 0) } else { Color::rgb8(0, 0, 255) });
        });
        let img = render_gpu(&g, &mut r, wgpu::TextureFormat::Rgba8Unorm, &s, (16, 16), 1.0);
        passes.push(r.stats().passes);
        assert_eq!(img.pixel(8, 8), if version == 1 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
    }
    assert_eq!(calls, 2);
    assert_eq!(passes, vec![2, 1, 2], "the unchanged layer is not drawn again");
}
