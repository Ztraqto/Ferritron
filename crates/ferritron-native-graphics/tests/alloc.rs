//! M4: a frame that repeats the previous one must not allocate: neither while the scene is
//! recorded again nor while the wgpu backend prepares it (the CPU half, without a device).
use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::cell::Cell;

use fng::prepare_only::Prepare;
use fng::*;

struct Counting;

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static ON: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        let _ = ON.try_with(|on| {
            if on.get() {
                let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, n: usize) -> *mut u8 {
        let _ = ON.try_with(|on| {
            if on.get() {
                let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn count<R>(f: impl FnOnce() -> R) -> (R, usize) {
    COUNT.with(|c| c.set(0));
    ON.with(|o| o.set(true));
    let r = f();
    ON.with(|o| o.set(false));
    (r, COUNT.with(|c| c.get()))
}

fn record(s: &mut Scene, star: &Path, layout: Option<&Layout>, img: ImageId) {
    s.fill_rect(Rect::new(0.0, 0.0, 256.0, 192.0), Color::hex(0x20242c));
    for i in 0..40 {
        let x = (i % 8) as f32 * 30.0;
        let y = (i / 8) as f32 * 20.0;
        s.fill_round_rect(Rect::new(x, y, 26.0, 16.0), 4.0, Color::hex(0x3d4455));
        s.fill_circle(Point::new(x + 8.0, y + 8.0), 5.0, Color::hex(0xff9f1c));
    }
    let pts: Vec<Point> = Vec::new();
    let _ = pts;
    s.stroke_line(Point::new(0.0, 100.0), Point::new(200.0, 110.0), &Stroke::width(3.0).with_dash(Dash::new(6.0, 3.0)), Color::WHITE);
    s.fill_path(star, FillRule::NonZero, Paint::Linear { start: Point::new(0.0, 0.0), end: Point::new(100.0, 100.0), stops: Stops::two(Color::WHITE, Color::hex(0x2ec4b6)) });
    s.stroke_path(star, &Stroke::width(2.0).with_join(Join::Round), Color::hex(0xe71d36));
    s.shadow(Rect::new(10.0, 120.0, 50.0, 30.0), 6.0, ShadowStyle { blur: 12.0, offset: Point::new(0.0, 3.0), color: Color::BLACK.with_alpha(0.6) });
    s.image(img, Rect::new(70.0, 120.0, 30.0, 30.0));
    s.push_clip_round_rect(Rect::new(110.0, 120.0, 60.0, 30.0), 8.0);
    s.fill_rect(Rect::new(100.0, 110.0, 80.0, 50.0), Color::hex(0x2ec4b6));
    s.pop_clip();
    s.push_clip_path(star, FillRule::EvenOdd);
    s.fill_rect(Rect::new(0.0, 0.0, 120.0, 120.0), Color::hex(0xe71d36));
    s.pop_clip();
    s.push_opacity(0.5);
    s.fill_circle(Point::new(200.0, 150.0), 20.0, Color::WHITE);
    s.pop_opacity();
    s.layer(LayerId(1), (60.0, 30.0), 1, |l| {
        l.fill_round_rect(Rect::new(0.0, 0.0, 60.0, 30.0), 6.0, Color::hex(0x3d4455));
    });
    if let Some(l) = layout {
        s.text(l, Point::new(10.0, 170.0), Color::WHITE);
    }
}

#[test]
fn an_unchanged_frame_allocates_nothing() {
    let (_, sanity) = count(|| std::hint::black_box(Box::new(7u64)));
    assert!(sanity > 0, "the counting allocator works");
    let mut fonts = Fonts::new();
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], language: Some("ja"), ..Default::default() });
    let layout = if chain.0.is_empty() { None } else { Some(fonts.layout("音量 Volume -6.0 dB", &TextStyle::new(chain, 13.0))) };
    if layout.is_none() {
        eprintln!("no system fonts: the text part is not measured");
    }
    let mut star = Path::new();
    for i in 0..5 {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * 4.0 * std::f32::consts::PI / 5.0;
        let p = Point::new(60.0 + 50.0 * a.cos(), 60.0 + 50.0 * a.sin());
        if i == 0 {
            star.move_to(p);
        } else {
            star.line_to(p);
        }
    }
    star.close();

    let mut images = ImageStore::new();
    let img = images.insert(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255], 2, 2);
    let opts = RenderOptions::new((512, 384)).with_scale(2.0).with_clear(Color::BLACK);

    let mut scene = Scene::new();
    let mut prep = Prepare::new();
    // warm up: the first frames fill the pools, the atlas, the gradient table
    for _ in 0..3 {
        scene.clear();
        record(&mut scene, &star, layout.as_deref(), img);
        prep.plan(&scene, &images, &opts);
    }
    let (_, recording) = count(|| {
        scene.clear();
        record(&mut scene, &star, layout.as_deref(), img);
    });
    // the closure passed to `layer` is not run again; nothing else may allocate
    assert_eq!(recording, 0, "recording an identical scene into a reused Scene allocated {recording} times");

    let (stats, planning) = count(|| prep.plan(&scene, &images, &opts));
    assert_eq!(planning, 0, "preparing an identical frame allocated {planning} times");
    assert!(stats.instances > 100, "something was planned: {stats:?}");
    assert_eq!(stats.restarts, 0);
}

/// The CPU backend, drawing a repeat of the same frame into a buffer the caller owns: from the
/// second frame on, nothing is allocated (surface, state, masks, output, layers and groups all
/// keep their buffers). Strokes are in it too (solid, dashed, round joins and caps): the stroker
/// keeps its buffers in the renderer (N1 / U1).
#[test]
fn a_repeated_cpu_frame_allocates_nothing() {
    let mut cpu = CpuRenderer::new();
    let img = cpu.images().insert(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255], 2, 2);
    let opts = RenderOptions::new((256, 192)).with_scale(1.5).with_clear(Color::BLACK);
    let mut out = vec![0u8; 256 * 192 * 4];
    let mut scene = Scene::new();
    let mut ring = Path::new();
    ring.circle(Point::new(200.0, 100.0), 15.0);
    let record = |s: &mut Scene| {
        s.stroke_line(Point::new(5.0, 180.0), Point::new(250.0, 185.0), &Stroke::width(3.0).with_dash(Dash::new(6.0, 3.0)), Color::WHITE);
        s.stroke_polyline(&[Point::new(5.0, 5.0), Point::new(40.0, 25.0), Point::new(80.0, 5.0)], &Stroke::width(4.0).with_join(Join::Round).with_cap(Cap::Round), Color::hex(0xe71d36));
        s.stroke_path(&ring, &Stroke::width(2.0).with_dash(Dash::new(4.0, 2.0)), Color::WHITE);
        s.fill_rect(Rect::new(0.0, 0.0, 256.0, 192.0), Color::hex(0x20242c));
        s.fill_round_rect(Rect::new(10.0, 10.0, 80.0, 40.0), 8.0, Color::hex(0x3d4455));
        s.fill_circle(Point::new(60.0, 60.0), 20.0, Color::hex(0xff9f1c));
        s.shadow(Rect::new(20.0, 90.0, 120.0, 60.0), 8.0, ShadowStyle { blur: 16.0, offset: Point::new(0.0, 4.0), color: Color::BLACK.with_alpha(0.6) });
        s.image(img, Rect::new(150.0, 20.0, 40.0, 40.0));
        s.push_opacity(0.5);
        s.fill_round_rect(Rect::new(150.0, 100.0, 60.0, 30.0), 6.0, Color::WHITE);
        s.pop_opacity();
        s.push_clip_round_rect(Rect::new(10.0, 150.0, 100.0, 30.0), 8.0);
        s.fill_rect(Rect::new(0.0, 140.0, 200.0, 50.0), Color::hex(0x2ec4b6));
        s.pop_clip();
        s.layer(LayerId(1), (60.0, 30.0), 1, |l| {
            l.fill_round_rect(Rect::new(0.0, 0.0, 60.0, 30.0), 6.0, Color::hex(0x3d4455));
        });
    };
    for _ in 0..3 {
        scene.clear();
        record(&mut scene);
        cpu.render_into(&scene, &opts, &mut out, PixelFormat::Rgba8).unwrap();
    }
    scene.clear();
    record(&mut scene);
    let (res, allocs) = count(|| cpu.render_into(&scene, &opts, &mut out, PixelFormat::Bgra8Premultiplied));
    res.unwrap();
    assert_eq!(allocs, 0, "a repeated CPU frame allocated {allocs} times");
    // the background covers the picture: the corner is 0x20242c in premultiplied BGRA
    assert_eq!(&out[0..4], &[0x2c, 0x24, 0x20, 0xff], "the corner is the background, premultiplied BGRA");
}

#[test]
fn render_into_refuses_a_bad_size_or_buffer() {
    let scene = Scene::new();
    let mut cpu = CpuRenderer::new();
    let mut out = vec![0u8; 16];
    let too_big = RenderOptions::new((MAX_SIDE + 1, 4));
    assert_eq!(cpu.render_into(&scene, &too_big, &mut out, PixelFormat::Rgba8), Err(RenderError::TooLarge));
    let ok = RenderOptions::new((4, 4));
    assert_eq!(cpu.render_into(&scene, &ok, &mut out[..8], PixelFormat::Rgba8), Err(RenderError::BufferSize { expected: 64, actual: 8 }));
    assert!(cpu.render_into(&scene, &ok, &mut out, PixelFormat::Rgba8).is_err(), "16 bytes is not 4x4x4");
}
