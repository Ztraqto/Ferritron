//! Performance goal of the design (section 6): 10,000 basic shapes + 5,000 glyphs prepared on the
//! CPU in under 2 ms. Run with `cargo test --release --test perf -- --ignored --nocapture`.
use std::time::Instant;

use fng::prepare_only::Prepare;
use fng::*;

fn record(s: &mut Scene, layout: &Layout) {
    s.fill_rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), Color::hex(0x20242c));
    for i in 0..10_000 {
        let x = (i % 100) as f32 * 19.0;
        let y = (i / 100) as f32 * 10.0;
        match i % 4 {
            0 => s.fill_rect(Rect::new(x, y, 16.0, 8.0), Color::hex(0x3d4455)),
            1 => s.fill_round_rect(Rect::new(x, y, 16.0, 8.0), 3.0, Color::hex(0xff9f1c)),
            2 => s.fill_circle(Point::new(x + 8.0, y + 4.0), 4.0, Color::hex(0x2ec4b6)),
            _ => s.stroke_line(Point::new(x, y), Point::new(x + 16.0, y + 8.0), &Stroke::width(1.5), Color::WHITE),
        }
    }
    s.text(layout, Point::new(10.0, 10.0), Color::WHITE);
}

#[test]
#[ignore = "timing: run in release mode"]
fn prepare_10k_shapes_and_5k_glyphs() {
    let mut fonts = Fonts::new();
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], language: Some("ja"), ..Default::default() });
    if chain.0.is_empty() {
        eprintln!("skipped: no fonts");
        return;
    }
    let mut text = String::new();
    while text.chars().count() < 5000 {
        text.push_str("Volume 音量 Frequency 周波数 -6.0 dB ");
    }
    let layout = Layout::new(&fonts, &text, &TextStyle::new(chain, 12.0).with_max_width(1900.0));
    println!("glyphs in the layout: {}", layout.glyphs().len());
    let images = ImageStore::new();
    let opts = RenderOptions::new((1920, 1080)).with_clear(Color::BLACK);
    let mut scene = Scene::new();
    let mut prep = Prepare::new();

    let t = Instant::now();
    scene.clear();
    record(&mut scene, &layout);
    let rec_first = t.elapsed();
    let t = Instant::now();
    let st = prep.plan(&scene, &images, &opts);
    println!("first frame: record {rec_first:?}, prepare {:?} (glyphs rasterized into the atlas) {st:?}", t.elapsed());

    let mut best_rec = std::time::Duration::MAX;
    let mut best_plan = std::time::Duration::MAX;
    for _ in 0..50 {
        let t = Instant::now();
        scene.clear();
        record(&mut scene, &layout);
        best_rec = best_rec.min(t.elapsed());
        let t = Instant::now();
        prep.plan(&scene, &images, &opts);
        best_plan = best_plan.min(t.elapsed());
    }
    println!("steady state (best of 50): record {best_rec:?}, prepare {best_plan:?}");
}
