//! Text: shaping, font fallback, line breaking and layout.
//!
//! A [`Layout`] is computed from a string and a [`TextStyle`] without touching the GPU, so it
//! can be measured (or built on another thread) before anything is drawn. Shaping uses
//! `rustybuzz` (kerning, ligatures, combining marks), line breaking follows UAX #14
//! (`unicode-linebreak`, which includes the Japanese line-start / line-end rules), and font
//! fallback is chosen per cluster from the style's [`FontChain`].
//!
//! Not supported: bidirectional text (right-to-left runs are laid out left to right),
//! vertical text, color emoji.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::font::{FontChain, Fonts};
use crate::geom::Point;
pub use crate::font::FontData;

/// Horizontal alignment of lines inside the layout width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Start,
    Center,
    End,
}

/// Everything besides the text that determines a layout.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub font: FontChain,
    /// Font size in logical pixels.
    pub size: f32,
    /// Line height as a multiple of `size` (for example 1.3). 0 or less uses the font's
    /// ascent + descent + line gap.
    pub line_height: f32,
    pub align: Align,
    /// Wrap lines wider than this (logical pixels). `None`: only newlines break lines.
    pub max_width: Option<f32>,
    /// Extra space after each character but the last of a line (logical pixels; spaced
    /// capitals in titles, for example). 0 by default.
    pub letter_spacing: f32,
}

impl TextStyle {
    pub fn new(font: FontChain, size: f32) -> TextStyle {
        TextStyle { font, size, line_height: 1.3, align: Align::Start, max_width: None, letter_spacing: 0.0 }
    }
    pub fn with_max_width(mut self, w: f32) -> TextStyle {
        self.max_width = Some(w);
        self
    }
    pub fn with_align(mut self, a: Align) -> TextStyle {
        self.align = a;
        self
    }
    pub fn with_letter_spacing(mut self, spacing: f32) -> TextStyle {
        self.letter_spacing = spacing;
        self
    }
    pub fn with_line_height(mut self, lh: f32) -> TextStyle {
        self.line_height = lh;
        self
    }
}

/// One shaped glyph, placed relative to the layout's top-left.
#[derive(Clone, Copy, Debug)]
pub struct PlacedGlyph {
    /// Index into [`Layout::fonts`].
    pub font: u16,
    pub glyph: u16,
    /// Pen position of the glyph origin (baseline).
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    /// Byte offset of the cluster in the text.
    pub cluster: u32,
}

/// A line of the layout.
#[derive(Clone, Copy, Debug)]
pub struct Line {
    /// Top of the line box.
    pub y: f32,
    pub height: f32,
    pub baseline: f32,
    /// Left edge of the first glyph (after alignment).
    pub x: f32,
    /// Width without trailing white space.
    pub width: f32,
    /// Byte range of the text on this line.
    pub start: u32,
    pub end: u32,
    pub(crate) c0: u32,
    pub(crate) c1: u32,
}

#[derive(Clone, Copy, Debug)]
struct Cluster {
    start: u32,
    end: u32,
    x0: f32,
    advance: f32,
}

/// Where a point falls in a layout (for text input).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Byte offset of the caret position nearest to the point.
    pub index: usize,
    pub line: usize,
    /// False when the point is outside the text (above, below, or beside the line).
    pub inside: bool,
}

/// A laid-out piece of text.
#[derive(Debug)]
pub struct Layout {
    fonts: Vec<Arc<FontData>>,
    glyphs: Vec<PlacedGlyph>,
    lines: Vec<Line>,
    clusters: Vec<Cluster>,
    size: f32,
    width: f32,
    height: f32,
}

fn is_space(c: char) -> bool {
    c.is_whitespace()
}

/// Characters that stay with the previous character's font.
fn inherits_font(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0x3099..=0x309A
        | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x200B..=0x200F | 0xE0100..=0xE01EF)
}

/// A coarse script class: runs are split where it changes (neutral characters join the run
/// they are in).
fn script_class(c: char) -> u8 {
    match c as u32 {
        0x0600..=0x06FF | 0x0750..=0x077F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => 3,
        0x0590..=0x05FF => 4,
        0x0E00..=0x0E7F => 5,
        0x0900..=0x0DFF => 6,
        0x3040..=0x30FF | 0x3100..=0x312F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xAC00..=0xD7AF
        | 0x1100..=0x11FF | 0xFF66..=0xFF9F | 0x20000..=0x2FFFF => 2,
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x024F | 0x0370..=0x058F | 0x1E00..=0x1FFF => 1,
        _ => 0,
    }
}

impl Layout {
    /// Lay out `text`. The chain's fonts come from `fonts`.
    pub fn new(fonts: &Fonts, text: &str, style: &TextStyle) -> Layout {
        build(fonts, text, style)
    }

    /// Overall size in logical pixels (widest line, total height).
    pub fn size(&self) -> (f32, f32) {
        (self.width, self.height)
    }
    pub fn width(&self) -> f32 {
        self.width
    }
    pub fn height(&self) -> f32 {
        self.height
    }
    /// The baseline of the first line, from the top of the layout (0 for an empty layout).
    pub fn baseline(&self) -> f32 {
        self.lines.first().map_or(0.0, |l| l.baseline)
    }
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }
    pub fn glyphs(&self) -> &[PlacedGlyph] {
        &self.glyphs
    }
    pub fn fonts(&self) -> &[Arc<FontData>] {
        &self.fonts
    }
    pub fn font_size(&self) -> f32 {
        self.size
    }

    /// The caret position (nearest to the point) for a click at `p`, relative to the layout's
    /// top-left.
    pub fn hit_test(&self, p: Point) -> Hit {
        if self.lines.is_empty() {
            return Hit { index: 0, line: 0, inside: false };
        }
        let lh = self.lines[0].height.max(1e-3);
        let mut inside = p.y >= 0.0 && p.y < self.height;
        let li = ((p.y / lh).floor().max(0.0) as usize).min(self.lines.len() - 1);
        let line = &self.lines[li];
        let cl = &self.clusters[line.c0 as usize..line.c1 as usize];
        if cl.is_empty() {
            return Hit { index: line.start as usize, line: li, inside: false };
        }
        if p.x < cl[0].x0 {
            return Hit { index: cl[0].start as usize, line: li, inside: false };
        }
        for c in cl {
            if p.x < c.x0 + c.advance {
                let mid = c.x0 + c.advance * 0.5;
                let index = if p.x < mid { c.start } else { c.end } as usize;
                return Hit { index, line: li, inside };
            }
        }
        inside = false;
        Hit { index: cl.last().unwrap().end as usize, line: li, inside }
    }

    /// Position of the caret before byte `index`: `(x, top, height)` relative to the layout.
    pub fn caret(&self, index: usize) -> (f32, f32, f32) {
        for (li, line) in self.lines.iter().enumerate() {
            let cl = &self.clusters[line.c0 as usize..line.c1 as usize];
            for c in cl {
                if c.start as usize == index {
                    return (c.x0, line.y, line.height);
                }
            }
            let last_line = li + 1 == self.lines.len();
            if (index >= line.end as usize && last_line) || (index == line.end as usize && !cl.is_empty()) {
                if let Some(c) = cl.last() {
                    return (c.x0 + c.advance, line.y, line.height);
                }
                return (line.x, line.y, line.height);
            }
        }
        match self.lines.last() {
            Some(l) => (l.x, l.y, l.height),
            None => (0.0, 0.0, self.size),
        }
    }

}

#[derive(Clone, Copy)]
struct LineSpec {
    start: u32,
    end: u32,
    c0: u32,
    c1: u32,
    g0: u32,
    g1: u32,
    width: f32,
}

struct GlyphTmp {
    font: u16,
    glyph: u16,
    advance: f32,
    dx: f32,
    dy: f32,
    cluster: u32,
}

fn build(fonts: &Fonts, text: &str, style: &TextStyle) -> Layout {
    let size = style.size.max(0.01);
    // faces of the chain (skipping ids that do not exist)
    let datas: Vec<Arc<FontData>> = style.font.0.iter().filter_map(|id| fonts.get(*id).cloned()).collect();
    let tfaces: Vec<ttf_parser::Face<'_>> = datas.iter().map(|d| d.face()).collect();
    let bfaces: Vec<Option<rustybuzz::Face<'_>>> = datas.iter().map(|d| rustybuzz::Face::from_slice(d.bytes(), d.face_index())).collect();

    let mut layout = Layout { fonts: datas.clone(), glyphs: Vec::new(), lines: Vec::new(), clusters: Vec::new(), size, width: 0.0, height: 0.0 };

    // line box metrics from the primary font
    let (asc, desc, gap) = match datas.first() {
        Some(f) => (f.ascent(size), f.descent(size), f.line_gap(size)),
        None => (size * 0.8, size * 0.2, 0.0),
    };
    let lh = if style.line_height > 0.0 { size * style.line_height } else { asc + desc + gap };
    let baseline_in_line = (lh - (asc + desc)) * 0.5 + asc;

    let mut buffer = Some(rustybuzz::UnicodeBuffer::new());
    let mut line_specs: Vec<LineSpec> = Vec::new();
    let mut para_start = 0usize;
    let bytes_len = text.len();
    loop {
        let rest = &text[para_start..];
        let (para_end, next_start) = match rest.find('\n') {
            Some(i) => (para_start + i, para_start + i + 1),
            None => (bytes_len, bytes_len + 1),
        };
        let mut para = &text[para_start..para_end];
        let para_off = para_start;
        if para.ends_with('\r') {
            para = &para[..para.len() - 1];
        }
        shape_paragraph(&mut layout, para, para_off, &tfaces, &bfaces, &mut buffer, size, style, &mut line_specs);
        if next_start > bytes_len {
            break;
        }
        para_start = next_start;
    }
    // vertical placement and alignment
    let widest = line_specs.iter().map(|l| l.width).fold(0.0f32, f32::max);
    let box_w = style.max_width.map_or(widest, |m| m.max(widest));
    for (i, l) in line_specs.iter().enumerate() {
        let x_off = match style.align {
            Align::Start => 0.0,
            Align::Center => (box_w - l.width) * 0.5,
            Align::End => box_w - l.width,
        };
        let y = i as f32 * lh;
        let baseline = y + baseline_in_line;
        layout.lines.push(Line { y, height: lh, baseline, x: x_off, width: l.width, start: l.start, end: l.end, c0: l.c0, c1: l.c1 });
        for c in &mut layout.clusters[l.c0 as usize..l.c1 as usize] {
            c.x0 += x_off;
        }
        for g in &mut layout.glyphs[l.g0 as usize..l.g1 as usize] {
            g.x += x_off;
            g.y += baseline;
        }
    }
    layout.width = widest;
    layout.height = line_specs.len() as f32 * lh;
    layout
}

#[allow(clippy::too_many_arguments)]
fn shape_paragraph(
    layout: &mut Layout,
    para: &str,
    para_off: usize,
    tfaces: &[ttf_parser::Face<'_>],
    bfaces: &[Option<rustybuzz::Face<'_>>],
    buffer: &mut Option<rustybuzz::UnicodeBuffer>,
    size: f32,
    style: &TextStyle,
    line_specs: &mut Vec<LineSpec>,
) {
    let c0_base = layout.clusters.len() as u32;
    if para.is_empty() || tfaces.is_empty() {
        let g = layout.glyphs.len() as u32;
        line_specs.push(LineSpec { start: para_off as u32, end: para_off as u32, c0: c0_base, c1: c0_base, g0: g, g1: g, width: 0.0 });
        return;
    }
    // 1. itemize: font index and script class per char
    let mut items: Vec<(usize, u16, u8)> = Vec::new(); // byte offset, font, script
    let mut prev_font: Option<u16> = None;
    let mut prev_script = 0u8;
    for (i, c) in para.char_indices() {
        let font = if inherits_font(c) || (is_space(c) && prev_font.is_some()) {
            prev_font.unwrap_or(0)
        } else {
            tfaces.iter().position(|f| f.glyph_index(c).is_some()).unwrap_or(0) as u16
        };
        let sc = script_class(c);
        let script = if sc == 0 { prev_script } else { sc };
        items.push((i, font, script));
        prev_font = Some(font);
        prev_script = script;
    }
    // 2. runs
    let mut glyphs: Vec<GlyphTmp> = Vec::new();
    let mut run_start = 0usize;
    for k in 1..=items.len() {
        let boundary = k == items.len() || items[k].1 != items[run_start].1 || items[k].2 != items[run_start].2;
        if !boundary {
            continue;
        }
        let b0 = items[run_start].0;
        let b1 = if k == items.len() { para.len() } else { items[k].0 };
        let font = items[run_start].1;
        let tf = &tfaces[font as usize];
        let scale = size / tf.units_per_em() as f32;
        match &bfaces[font as usize] {
            Some(bf) => {
                let mut buf = buffer.take().unwrap_or_default();
                buf.push_str(&para[b0..b1]);
                buf.set_direction(rustybuzz::Direction::LeftToRight);
                let out = rustybuzz::shape(bf, &[], buf);
                for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
                    glyphs.push(GlyphTmp {
                        font,
                        glyph: info.glyph_id as u16,
                        advance: pos.x_advance as f32 * scale,
                        dx: pos.x_offset as f32 * scale,
                        dy: -(pos.y_offset as f32) * scale,
                        cluster: (para_off + b0) as u32 + info.cluster,
                    });
                }
                *buffer = Some(out.clear());
            }
            None => {
                for (i, c) in para[b0..b1].char_indices() {
                    let g = tf.glyph_index(c).map_or(0, |g| g.0);
                    let adv = tf.glyph_hor_advance(ttf_parser::GlyphId(g)).unwrap_or(0) as f32 * scale;
                    glyphs.push(GlyphTmp { font, glyph: g, advance: adv, dx: 0.0, dy: 0.0, cluster: (para_off + b0 + i) as u32 });
                }
            }
        }
        run_start = k;
    }
    // 3. clusters: consecutive glyphs with the same cluster value
    struct Cl {
        g0: usize,
        g1: usize,
        start: u32,
        advance: f32,
        space: bool,
    }
    let mut cls: Vec<Cl> = Vec::new();
    for (gi, g) in glyphs.iter().enumerate() {
        match cls.last_mut() {
            Some(c) if c.start == g.cluster => {
                c.g1 = gi + 1;
                c.advance += g.advance;
            }
            _ => cls.push(Cl { g0: gi, g1: gi + 1, start: g.cluster, advance: g.advance, space: false }),
        }
    }
    // letter spacing: after each cluster (the last glyph of the cluster carries it)
    let spacing = style.letter_spacing;
    if spacing != 0.0 {
        for c in &mut cls {
            c.advance += spacing;
            glyphs[c.g1 - 1].advance += spacing;
        }
    }
    let para_end_byte = (para_off + para.len()) as u32;
    // glyph order may be non-monotonic in clusters only for RTL; we shape LTR, so clusters grow.
    for i in 0..cls.len() {
        let end = if i + 1 < cls.len() { cls[i + 1].start } else { para_end_byte };
        let t = &layout_text_slice(para, para_off, cls[i].start, end);
        cls[i].space = !t.is_empty() && t.chars().all(is_space);
    }
    // 4. break opportunities (byte offsets in the whole text)
    let mut allowed: Vec<u32> = Vec::new();
    for (idx, _) in unicode_linebreak::linebreaks(para) {
        allowed.push((para_off + idx) as u32);
    }
    // 5. greedy line breaking
    let max_w = style.max_width.filter(|w| *w > 0.0);
    let n = cls.len();
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut ls = 0usize;
    let mut x = 0.0f32;
    let mut last_break: Option<usize> = None;
    let mut i = 0usize;
    while i < n {
        if i > ls && allowed.binary_search(&cls[i].start).is_ok() {
            last_break = Some(i);
        }
        let w = cls[i].advance;
        if let Some(mw) = max_w {
            if !cls[i].space && x + w > mw + 1e-3 && i > ls {
                let at = last_break.filter(|b| *b > ls).unwrap_or(i);
                lines.push((ls, at));
                ls = at;
                i = at;
                x = 0.0;
                last_break = None;
                continue;
            }
        }
        x += w;
        i += 1;
    }
    lines.push((ls, n));
    // 6. emit glyphs and clusters line by line
    for (a, b) in lines {
        let glyph_base = layout.glyphs.len();
        let c0 = layout.clusters.len() as u32;
        let mut pen = 0.0f32;
        let mut width_no_trailing = 0.0f32;
        for ci in a..b {
            let c = &cls[ci];
            let end = if ci + 1 < n { cls[ci + 1].start } else { para_end_byte };
            layout.clusters.push(Cluster { start: c.start, end, x0: pen, advance: c.advance });
            for g in &glyphs[c.g0..c.g1] {
                layout.glyphs.push(PlacedGlyph { font: g.font, glyph: g.glyph, x: pen + g.dx, y: g.dy, advance: g.advance, cluster: g.cluster });
                pen += g.advance;
            }
            if !c.space {
                // the spacing after the last character is not part of the width
                width_no_trailing = pen - spacing;
            }
        }
        let start_byte = cls.get(a).map_or(para_off as u32, |c| c.start);
        let end_byte = if b < n { cls[b].start } else { para_end_byte };
        line_specs.push(LineSpec {
            start: start_byte,
            end: end_byte,
            c0,
            c1: layout.clusters.len() as u32,
            g0: glyph_base as u32,
            g1: layout.glyphs.len() as u32,
            width: width_no_trailing,
        });
    }
}

fn layout_text_slice(para: &str, para_off: usize, start: u32, end: u32) -> &str {
    let s = (start as usize).saturating_sub(para_off).min(para.len());
    let e = (end as usize).saturating_sub(para_off).min(para.len());
    para.get(s..e).unwrap_or("")
}

/// Cache of layouts, keyed by text and style.
#[derive(Default)]
pub(crate) struct LayoutCache {
    map: HashMap<u64, CacheEntry>,
    stamp: u64,
}

struct CacheEntry {
    text: String,
    style: TextStyle,
    layout: Arc<Layout>,
    used: u64,
}

fn style_hash(text: &str, style: &TextStyle) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    style.font.hash(&mut h);
    style.size.to_bits().hash(&mut h);
    style.line_height.to_bits().hash(&mut h);
    style.align.hash(&mut h);
    style.max_width.map(f32::to_bits).hash(&mut h);
    style.letter_spacing.to_bits().hash(&mut h);
    h.finish()
}

impl Fonts {
    /// Like [`Layout::new`], but remembers the result: the same text and style give the same
    /// `Arc<Layout>` without shaping again.
    pub fn layout(&mut self, text: &str, style: &TextStyle) -> Arc<Layout> {
        let key = style_hash(text, style);
        self.layouts.stamp += 1;
        let stamp = self.layouts.stamp;
        if let Some(e) = self.layouts.map.get_mut(&key) {
            if e.text == text && e.style == *style {
                e.used = stamp;
                return e.layout.clone();
            }
        }
        let layout = Arc::new(Layout::new(self, text, style));
        if self.layouts.map.len() >= 2048 {
            // drop the older half
            let mut stamps: Vec<u64> = self.layouts.map.values().map(|e| e.used).collect();
            stamps.sort_unstable();
            let cut = stamps[stamps.len() / 2];
            self.layouts.map.retain(|_, e| e.used > cut);
        }
        self.layouts.map.insert(key, CacheEntry { text: text.to_string(), style: style.clone(), layout: layout.clone(), used: stamp });
        layout
    }

    /// `text` as it fits in `max_width` on one line: the whole text when it fits, else the
    /// longest start of it followed by an ellipsis ("…") that fits (just "…" when nothing does).
    pub fn fit(&mut self, text: &str, style: &TextStyle, max_width: f32) -> String {
        if self.layout(text, style).width() <= max_width {
            return text.to_string();
        }
        // the longest char prefix whose "prefix…" fits (binary search over char boundaries)
        let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).chain(std::iter::once(text.len())).collect();
        let fits = |fonts: &mut Fonts, n: usize| -> bool {
            let candidate = format!("{}\u{2026}", text[..if n == 0 { 0 } else { ends[n - 1] }].trim_end());
            fonts.layout(&candidate, style).width() <= max_width
        };
        let (mut lo, mut hi) = (0usize, ends.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if fits(self, mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let cut = if lo == 0 { 0 } else { ends[lo - 1] };
        format!("{}\u{2026}", text[..cut].trim_end())
    }

    /// Number of cached layouts.
    pub fn cached_layouts(&self) -> usize {
        self.layouts.map.len()
    }
}
