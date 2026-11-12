//! Text (M3): layout, measuring, fallback, wrapping, hit testing and drawing.
//!
//! These tests use the operating system's fonts (Segoe UI and Yu Gothic UI on Windows). Where
//! the fonts are missing they skip themselves with a message.
mod common;
use common::*;
use fng::*;

fn setup() -> Option<(Fonts, FontChain)> {
    let mut fonts = Fonts::new();
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], language: Some("ja"), ..Default::default() });
    if chain.0.len() < 2 {
        eprintln!("skipped: needs Segoe UI and a Japanese font (found {} fonts)", chain.0.len());
        return None;
    }
    Some((fonts, chain))
}

fn style(chain: &FontChain, size: f32) -> TextStyle {
    TextStyle::new(chain.clone(), size)
}

#[test]
fn query_returns_a_fallback_chain_with_japanese() {
    let Some((fonts, chain)) = setup() else { return };
    assert_eq!(fonts.get(chain.0[0]).unwrap().family(), "Segoe UI");
    let japanese = chain.0.iter().any(|id| {
        let f = fonts.get(*id).unwrap();
        f.face().glyph_index('音').is_some()
    });
    assert!(japanese, "some font of the chain has Japanese glyphs");
}

#[test]
fn mixed_japanese_and_latin_uses_two_fonts_without_missing_glyphs() {
    let Some((fonts, chain)) = setup() else { return };
    let l = Layout::new(&fonts, "音量 Volume -6.0 dB", &style(&chain, 13.0));
    assert!(l.width() > 60.0 && l.height() > 13.0, "{:?}", l.size());
    assert!(l.glyphs().iter().all(|g| g.glyph != 0), "no .notdef glyphs");
    let kanji_font = l.glyphs()[0].font;
    let latin_font = l.glyphs().iter().find(|g| g.cluster as usize >= "音量 ".len()).unwrap().font;
    assert_ne!(kanji_font, latin_font, "kanji and Latin come from different fonts");
    assert_eq!(latin_font, 0, "Latin text stays in the first font of the chain");
}

#[test]
fn kerning_narrows_av() {
    let Some((fonts, chain)) = setup() else { return };
    let s = style(&chain, 40.0);
    let av = Layout::new(&fonts, "AV", &s).width();
    let a = Layout::new(&fonts, "A", &s).width();
    let v = Layout::new(&fonts, "V", &s).width();
    assert!(av < a + v - 0.5, "AV {av} should be tighter than A + V {}", a + v);
}

#[test]
fn width_scales_with_font_size() {
    let Some((fonts, chain)) = setup() else { return };
    let w10 = Layout::new(&fonts, "Hello", &style(&chain, 10.0)).width();
    let w20 = Layout::new(&fonts, "Hello", &style(&chain, 20.0)).width();
    assert!((w20 / w10 - 2.0).abs() < 0.02, "{w10} {w20}");
}

#[test]
fn newlines_make_lines() {
    let Some((fonts, chain)) = setup() else { return };
    let s = style(&chain, 20.0).with_line_height(1.5);
    let l = Layout::new(&fonts, "one\ntwo\n\nfour", &s);
    assert_eq!(l.lines().len(), 4);
    assert!((l.height() - 4.0 * 30.0).abs() < 1e-3);
    assert!((l.lines()[1].baseline - l.lines()[0].baseline - 30.0).abs() < 1e-3);
}

#[test]
fn latin_text_wraps_at_spaces() {
    let Some((fonts, chain)) = setup() else { return };
    let text = "The quick brown fox jumps over the lazy dog";
    let full = Layout::new(&fonts, text, &style(&chain, 16.0)).width();
    let s = style(&chain, 16.0).with_max_width(full / 3.0);
    let l = Layout::new(&fonts, text, &s);
    assert!(l.lines().len() >= 3, "{} lines", l.lines().len());
    for line in l.lines() {
        assert!(line.width <= full / 3.0 + 0.01, "line wider than the limit: {}", line.width);
        // lines break only after spaces
        let b = line.end as usize;
        if b < text.len() {
            assert_eq!(text.as_bytes()[b - 1], b' ');
        }
    }
    // the lines cover the whole text without gaps
    assert_eq!(l.lines()[0].start, 0);
    for w in l.lines().windows(2) {
        assert_eq!(w[0].end, w[1].start);
    }
    assert_eq!(l.lines().last().unwrap().end as usize, text.len());
}

#[test]
fn japanese_text_wraps_and_respects_line_start_rules() {
    let Some((fonts, chain)) = setup() else { return };
    let text = "これは日本語の長い文章です。句読点、括弧「テスト」が行頭に来てはいけません。";
    let size = 16.0;
    let s = style(&chain, size).with_max_width(size * 7.5);
    let l = Layout::new(&fonts, text, &s);
    assert!(l.lines().len() >= 4, "{} lines", l.lines().len());
    for line in l.lines() {
        let first = text[line.start as usize..].chars().next().unwrap();
        assert!(!"。、」）".contains(first), "line starts with a prohibited character {first}");
        assert!(line.width <= size * 7.5 + 0.5);
    }
}

#[test]
fn a_word_longer_than_the_width_is_not_lost() {
    let Some((fonts, chain)) = setup() else { return };
    let l = Layout::new(&fonts, "Supercalifragilistic", &style(&chain, 16.0).with_max_width(20.0));
    assert!(l.lines().len() > 1);
    assert_eq!(l.glyphs().len(), 20);
}

#[test]
fn hit_test_finds_caret_positions() {
    let Some((fonts, chain)) = setup() else { return };
    let l = Layout::new(&fonts, "Hello", &style(&chain, 20.0));
    let g = l.glyphs();
    assert_eq!(l.hit_test(Point::new(-5.0, 5.0)), Hit { index: 0, line: 0, inside: false });
    let h = l.hit_test(Point::new(g[1].x + g[1].advance * 0.25, 5.0));
    assert_eq!((h.index, h.inside), (1, true), "left half of 'e' puts the caret before it");
    let h = l.hit_test(Point::new(g[1].x + g[1].advance * 0.75, 5.0));
    assert_eq!(h.index, 2, "right half puts the caret after it");
    let h = l.hit_test(Point::new(l.width() + 10.0, 5.0));
    assert_eq!((h.index, h.inside), (5, false));
    let (x, top, height) = l.caret(2);
    assert!((x - g[2].x).abs() < 1e-3 && top == 0.0 && height > 20.0);
    let (x_end, _, _) = l.caret(5);
    assert!((x_end - (g[4].x + g[4].advance)).abs() < 1e-3);
}

#[test]
fn hit_test_on_a_second_line() {
    let Some((fonts, chain)) = setup() else { return };
    let l = Layout::new(&fonts, "ab\ncd", &style(&chain, 20.0));
    let lh = l.lines()[0].height;
    let h = l.hit_test(Point::new(1.0, lh * 1.5));
    assert_eq!((h.line, h.index), (1, 3));
}

#[test]
fn alignment_shifts_lines() {
    let Some((fonts, chain)) = setup() else { return };
    let base = style(&chain, 16.0).with_max_width(200.0);
    let start = Layout::new(&fonts, "Hi", &base);
    let center = Layout::new(&fonts, "Hi", &base.clone().with_align(Align::Center));
    let end = Layout::new(&fonts, "Hi", &base.clone().with_align(Align::End));
    let w = start.width();
    assert!((start.lines()[0].x).abs() < 1e-4);
    assert!((center.lines()[0].x - (200.0 - w) * 0.5).abs() < 1e-3);
    assert!((end.lines()[0].x - (200.0 - w)).abs() < 1e-3);
    assert!((end.glyphs()[0].x - (200.0 - w)).abs() < 1e-3);
}

#[test]
fn combining_marks_stay_in_one_cluster() {
    let Some((fonts, chain)) = setup() else { return };
    let l = Layout::new(&fonts, "e\u{301}x", &style(&chain, 20.0));
    // 'e' + acute is one caret position: clicking inside it lands before or after both chars
    let first = l.glyphs()[0];
    let h = l.hit_test(Point::new(first.x + 1.0, 5.0));
    assert_eq!(h.index, 0);
    let h = l.hit_test(Point::new(first.x + first.advance - 1.0, 5.0));
    assert_eq!(h.index, 3, "after e + U+0301 (3 bytes)");
}

#[test]
fn layout_cache_returns_the_same_layout() {
    let Some((mut fonts, chain)) = setup() else { return };
    let s = style(&chain, 14.0);
    let a = fonts.layout("cache me", &s);
    let b = fonts.layout("cache me", &s);
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let c = fonts.layout("cache me too", &s);
    assert!(!std::sync::Arc::ptr_eq(&a, &c));
    assert_eq!(fonts.cached_layouts(), 2);
}

#[test]
fn registered_fonts_win_over_system_fonts() {
    let Some((mut fonts, _)) = setup() else { return };
    // register a copy of Segoe UI under the registry: the app's own font is preferred
    let chain = fonts.query(&FontQuery { families: &["Segoe UI"], ..Default::default() });
    let data = fonts.get(chain.0[0]).unwrap().bytes().to_vec();
    let ids = fonts.register(data).unwrap();
    let chain2 = fonts.query(&FontQuery { families: &["Segoe UI"], ..Default::default() });
    assert_eq!(chain2.0[0], ids[0]);
}

fn render_text(scene: &Scene, scale: f32) -> RgbaImage {
    CpuRenderer::new().render(scene, ((W as f32 * scale) as u32, (H as f32 * scale) as u32), scale)
}

/// The text scene used by the reference picture and the GPU comparison.
pub fn scene_text(fonts: &mut Fonts, chain: &FontChain) -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    let l = fonts.layout("音量 Volume -6.0 dB", &TextStyle::new(chain.clone(), 13.0));
    s.text(&l, Point::new(10.0, 10.0), FG);
    let l = fonts.layout("周波数フィルター Frequency", &TextStyle::new(chain.clone(), 20.0));
    s.text(&l, Point::new(10.0, 34.0), ACCENT);
    let l = fonts.layout("AVATAR To Wave fi ffi", &TextStyle::new(chain.clone(), 24.0));
    s.text(&l, Point::new(10.0, 64.0), BLUE);
    let l = fonts.layout("これは日本語の長い文章です。禁則処理も確かめます。", &TextStyle::new(chain.clone(), 14.0).with_max_width(150.0));
    s.text(&l, Point::new(10.0, 110.0), FG);
    let l = fonts.layout("Center\n中央揃え", &TextStyle::new(chain.clone(), 14.0).with_max_width(80.0).with_align(Align::Center));
    s.text(&l, Point::new(170.0, 110.0), RED);
    s.push_transform(Affine::rotate(-0.2).then(&Affine::translate(20.0, 170.0)));
    let l = fonts.layout("Rotated 回転", &TextStyle::new(chain.clone(), 16.0));
    s.text(&l, Point::ZERO, Paint::Linear { start: Point::new(0.0, 0.0), end: Point::new(100.0, 0.0), stops: Stops::two(ACCENT, BLUE) });
    s.pop_transform();
    s
}

#[test]
fn drawn_text_has_ink_in_the_right_place() {
    let Some((mut fonts, chain)) = setup() else { return };
    let mut s = Scene::new();
    let l = fonts.layout("Hlll", &TextStyle::new(chain.clone(), 40.0));
    s.text(&l, Point::new(10.0, 10.0), Color::BLACK);
    let img = CpuRenderer::new().render(&s, (200, 80), 1.0);
    let ink: usize = img.data.chunks_exact(4).filter(|p| p[3] > 128).count();
    assert!(ink > 300, "text has ink: {ink}");
    // nothing above the layout's top or left of its origin
    for x in 0..200u32 {
        assert_eq!(img.pixel(x, 5)[3], 0);
    }
    for y in 0..80u32 {
        assert_eq!(img.pixel(5, y)[3], 0);
    }
    // the baseline: no ink more than a descent below it
    let baseline = (10.0 + l.lines()[0].baseline) as u32;
    for x in 0..200u32 {
        assert_eq!(img.pixel(x, baseline + 3)[3], 0, "no descender expected in 'Hlll'");
    }
}

#[test]
fn sub_pixel_positions_change_the_picture_but_whole_pixels_only_shift_it() {
    let Some((mut fonts, chain)) = setup() else { return };
    let l = fonts.layout("l", &TextStyle::new(chain.clone(), 30.0));
    let draw = |x: f32| {
        let mut s = Scene::new();
        s.text(&l, Point::new(x, 5.0), Color::BLACK);
        CpuRenderer::new().render(&s, (40, 50), 1.0)
    };
    let a = draw(10.0);
    let b = draw(10.25);
    let c = draw(11.0);
    assert_ne!(a.data, b.data, "a quarter pixel moves the glyph");
    for y in 0..50u32 {
        for x in 0..39u32 {
            assert_eq!(a.pixel(x, y), c.pixel(x + 1, y), "one pixel right is a plain shift ({x},{y})");
        }
    }
}

#[test]
fn reference_text() {
    let Some((mut fonts, chain)) = setup() else { return };
    let s = scene_text(&mut fonts, &chain);
    // The reference was made with Segoe UI / Yu Gothic UI on Windows: a different font version
    // changes the picture, so the allowed difference is generous.
    check_reference("text", &render_text(&s, 1.0), 255, 1.5);
}

#[test]
fn letter_spacing_widens_by_the_gaps_between_characters() {
    let Some((fonts, chain)) = setup() else { return };
    let plain = Layout::new(&fonts, "PANSTUDIO", &style(&chain, 12.0));
    let spaced = Layout::new(&fonts, "PANSTUDIO", &style(&chain, 12.0).with_letter_spacing(3.0));
    // nine characters: eight gaps, none after the last one
    assert!((spaced.width() - plain.width() - 8.0 * 3.0).abs() < 0.01, "{} {}", plain.width(), spaced.width());
}

#[test]
fn fit_cuts_with_an_ellipsis_and_keeps_text_that_fits() {
    let Some((mut fonts, chain)) = setup() else { return };
    let st = style(&chain, 12.0);
    assert_eq!(fonts.fit("Kick", &st, 200.0), "Kick");
    let long = "トラックの名前がとても長い Lead Vocal";
    let cut = fonts.fit(long, &st, 80.0);
    assert!(cut.ends_with('…') && cut.chars().count() < long.chars().count(), "{cut}");
    assert!(fonts.layout(&cut, &st).width() <= 80.0);
    // one more character would not fit
    let kept = cut.trim_end_matches('…');
    let next: String = long.chars().take(kept.chars().count() + 1).collect();
    assert!(fonts.layout(&format!("{}…", next.trim_end()), &st).width() > 80.0);
    assert_eq!(fonts.fit(long, &st, 1.0), "…");
}

#[test]
fn text_line_puts_the_baseline_at_y_and_aligns_at_x() {
    let Some((mut fonts, chain)) = setup() else { return };
    let l = fonts.layout("Pan", &style(&chain, 20.0));
    assert!(l.baseline() > 10.0 && l.baseline() < l.height());
    let mut s = Scene::new();
    assert_eq!(s.text_line(&l, Point::new(100.0, 50.0), Align::Start, Color::WHITE), 100.0);
    assert!((s.text_line(&l, Point::new(100.0, 50.0), Align::End, Color::WHITE) - (100.0 - l.width())).abs() < 1e-4);
    assert!((s.text_line(&l, Point::new(100.0, 50.0), Align::Center, Color::WHITE) - (100.0 - l.width() / 2.0)).abs() < 1e-4);
    // drawn: ink above the baseline at y = 50, little below it
    let mut one = Scene::new();
    one.text_line(&l, Point::new(10.0, 50.0), Align::Start, Color::WHITE);
    let img = CpuRenderer::new().render(&one, (120, 80), 1.0);
    let ink = |y0: u32, y1: u32| (y0..y1).flat_map(|y| (0..120).map(move |x| (x, y))).filter(|&(x, y)| img.pixel(x, y)[3] > 128).count();
    assert!(ink(36, 50) > 40 && ink(51, 80) == 0, "{} {}", ink(36, 50), ink(51, 80));
}
