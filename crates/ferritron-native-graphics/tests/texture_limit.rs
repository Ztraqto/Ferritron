//! N3: a layer texture never exceeds the device's largest texture side. The layer is drawn
//! smaller and stretched back when it is composited, so nothing is dropped and no validation
//! error is raised.
use fng::prepare_only::Prepare;
use fng::*;

fn big_layer_scene() -> Scene {
    let mut s = Scene::new();
    s.layer(LayerId(7), (600.0, 300.0), 1, |l| {
        l.fill_rect(Rect::new(0.0, 0.0, 600.0, 300.0), Color::hex(0xe71d36));
    });
    s
}

#[test]
fn a_layer_is_drawn_no_larger_than_the_limit() {
    let images = ImageStore::new();
    let opts = RenderOptions::new((200, 100));
    let scene = big_layer_scene();

    let mut unlimited = Prepare::new();
    let st = unlimited.plan(&scene, &images, &opts);
    assert_eq!(st.max_pass_px, 600, "without a limit the layer keeps its size: {st:?}");

    let mut limited = Prepare::with_max_texture(256);
    let st = limited.plan(&scene, &images, &opts);
    assert!(st.max_pass_px <= 256, "the layer is at most 256 pixels a side: {st:?}");
    // the layer's rectangle inside the layer, and the layer quad itself
    assert_eq!(st.instances, 2, "the layer is still drawn: {st:?}");
    assert_eq!(st.dropped, 0, "{st:?}");
}

/// A device with the smallest texture side WebGPU guarantees (2048). The layer is 2500 logical
/// pixels wide, so without the limit its texture would be rejected by the device.
#[test]
fn a_layer_wider_than_the_device_limit_is_drawn_without_an_error() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("fng-limit"),
        required_limits: wgpu::Limits::downlevel_defaults(),
        ..Default::default()
    })) else {
        eprintln!("skipped: no device with the smaller limits");
        return;
    };
    let mut scene = Scene::new();
    scene.layer(LayerId(9), (2500.0, 60.0), 1, |l| {
        l.fill_rect(Rect::new(0.0, 0.0, 2500.0, 60.0), Color::hex(0xe71d36));
    });
    let size = (300u32, 100u32);
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
    let mut r = Renderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
    let mut enc = device.create_command_encoder(&Default::default());
    r.render(&device, &queue, &mut enc, &view, &scene, RenderOptions::new(size));
    queue.submit([enc.finish()]);
    let data = fng::gpu::read_texture_rgba8(&device, &queue, &tex, size);
    let i = ((30 * size.0 + 150) * 4) as usize;
    assert!(data[i] > 200 && data[i + 1] < 60, "the layer is drawn (red at the middle): {:?}", &data[i..i + 4]);
    assert_eq!(r.stats().dropped, 0);
}
