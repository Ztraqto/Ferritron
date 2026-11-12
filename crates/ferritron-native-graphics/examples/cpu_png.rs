//! Draws a small scene with the CPU backend and writes it as a PNG file.
//!
//! `cargo run --example cpu_png -- out.png`

use fng::*;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "fng-example.png".to_string());
    let mut fonts = Fonts::new();
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], language: Some("ja"), ..Default::default() });

    let bg = Color::hex(0x20242c);
    let accent = Color::hex(0xff9f1c);
    let mut scene = Scene::new();
    scene.fill_rect(Rect::new(0.0, 0.0, 320.0, 200.0), bg);
    scene.shadow(
        Rect::new(20.0, 20.0, 280.0, 120.0),
        12.0,
        ShadowStyle { blur: 16.0, offset: Point::new(0.0, 4.0), color: Color::BLACK.with_alpha(0.6) },
    );
    scene.fill_round_rect(Rect::new(20.0, 20.0, 280.0, 120.0), 12.0, Color::hex(0x3d4455));
    scene.fill_circle(
        Point::new(70.0, 80.0),
        28.0,
        Paint::Radial { center: Point::new(62.0, 72.0), radius: 36.0, stops: Stops::two(Color::WHITE, accent) },
    );
    scene.stroke_line(Point::new(120.0, 100.0), Point::new(280.0, 100.0), &Stroke::width(3.0).with_cap(Cap::Round), Color::hex(0x2ec4b6));
    if !chain.0.is_empty() {
        let layout = fonts.layout("音量 Volume -6.0 dB", &TextStyle::new(chain, 16.0));
        scene.text(&layout, Point::new(120.0, 60.0), Color::hex(0xe8ecf4));
    }

    let image = CpuRenderer::new().render(&scene, (640, 400), 2.0);
    image.save_png(&path)?;
    println!("wrote {path} ({}x{})", image.width, image.height);
    Ok(())
}
