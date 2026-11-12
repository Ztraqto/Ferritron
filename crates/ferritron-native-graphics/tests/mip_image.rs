//! U6: a large image drawn small is averaged (mip levels), not sampled at one texel per pixel.
//! The CPU and the GPU backend must agree, and a fine checkerboard must not turn into moire.
use fng::*;

/// 1-texel black and white checkerboard, opaque, `n` x `n`.
fn checker(n: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let v = if (x + y) % 2 == 0 { 255 } else { 0 };
            px.extend_from_slice(&[v, v, v, 255]);
        }
    }
    px
}

fn scene_with(img: ImageId, size: (f32, f32)) -> Scene {
    let mut s = Scene::new();
    s.image(img, Rect::new(0.0, 0.0, size.0, size.1));
    s
}

/// The average of a quarter-size draw of a fine checkerboard stays near mid grey (no moire),
/// and it is smooth: no pixel is far from the average.
#[test]
fn a_fine_checkerboard_drawn_small_averages_out_on_the_cpu() {
    let mut cpu = CpuRenderer::new();
    let id = cpu.images().insert(&checker(512), 512, 512);
    let img = cpu.render(&scene_with(id, (128.0, 128.0)), (128, 128), 1.0);
    let mut sum = 0u64;
    let mut worst = 0i32;
    for px in img.data.chunks_exact(4) {
        sum += px[0] as u64;
        worst = worst.max((px[0] as i32 - 128).abs());
    }
    let mean = sum as f64 / (128.0 * 128.0);
    assert!((mean - 128.0).abs() < 4.0, "the mean is mid grey: {mean}");
    assert!(worst < 40, "no pixel is far from the average (worst {worst}): aliasing");
}

#[test]
fn a_fine_checkerboard_drawn_small_matches_on_the_gpu() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("fng-mip"), ..Default::default() })) else {
        eprintln!("skipped: no device");
        return;
    };
    let size = (128u32, 128u32);
    let mut cpu = CpuRenderer::new();
    let cid = cpu.images().insert(&checker(512), 512, 512);
    let want = cpu.render(&scene_with(cid, (128.0, 128.0)), size, 1.0);

    let mut r = Renderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
    let gid = r.images().insert(&checker(512), 512, 512);
    let scene = scene_with(gid, (128.0, 128.0));
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    let mut enc = device.create_command_encoder(&Default::default());
    r.render(&device, &queue, &mut enc, &view, &scene, RenderOptions::new(size));
    queue.submit([enc.finish()]);
    let data = fng::gpu::read_texture_rgba8(&device, &queue, &tex, size);
    let got = RgbaImage { width: size.0, height: size.1, data };

    let mut max = 0u8;
    let mut sum = 0u64;
    for (a, b) in got.data.chunks_exact(4).zip(want.data.chunks_exact(4)) {
        let d = a[0].abs_diff(b[0]);
        max = max.max(d);
        sum += d as u64;
    }
    let mean = sum as f64 / (128.0 * 128.0);
    eprintln!("mip image: GPU vs CPU max diff {max}, mean {mean:.3}");
    assert!(mean < 1.0, "GPU and CPU agree on the averaged picture (mean diff {mean})");
    assert!(max < 40, "GPU and CPU agree (max diff {max})");
}
