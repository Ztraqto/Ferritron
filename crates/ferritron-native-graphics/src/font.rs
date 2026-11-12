//! Font data and the font registry.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fonts::FontQuery;
use crate::geom::Point;
use crate::path::Path;

/// A parsed-on-demand font face: the file's bytes plus the index of the face in the file.
pub struct FontData {
    pub(crate) uid: u64,
    data: Arc<[u8]>,
    index: u32,
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    weight: u16,
    italic: bool,
    family: String,
}

impl fmt::Debug for FontData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontData").field("family", &self.family).field("weight", &self.weight).field("index", &self.index).finish()
    }
}

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

impl FontData {
    /// Parse face `index` of the font file `data` (`index` is 0 for plain `.ttf` / `.otf`).
    pub fn new(data: Arc<[u8]>, index: u32) -> Option<FontData> {
        let (units_per_em, ascender, descender, line_gap, weight, italic, family) = {
            let face = ttf_parser::Face::parse(&data, index).ok()?;
            let family = face
                .names()
                .into_iter()
                .filter(|n| n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
                .chain(face.names().into_iter().filter(|n| n.name_id == ttf_parser::name_id::FAMILY))
                .find_map(|n| n.to_string())
                .unwrap_or_default();
            (face.units_per_em(), face.ascender(), face.descender(), face.line_gap(), face.weight().to_number(), face.is_italic(), family)
        };
        Some(FontData {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            data,
            index,
            units_per_em: units_per_em.max(1),
            ascender,
            descender,
            line_gap,
            weight,
            italic,
            family,
        })
    }

    /// Process-unique id of this face (a cache key).
    pub fn uid(&self) -> u64 {
        self.uid
    }
    pub fn family(&self) -> &str {
        &self.family
    }
    pub fn weight(&self) -> u16 {
        self.weight
    }
    pub fn is_italic(&self) -> bool {
        self.italic
    }
    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
    pub fn face_index(&self) -> u32 {
        self.index
    }
    /// The parsed face (cheap to create: tables are read lazily).
    pub fn face(&self) -> ttf_parser::Face<'_> {
        ttf_parser::Face::parse(&self.data, self.index).expect("validated in FontData::new")
    }
    /// Ascent above the baseline, in pixels at font size `size` (positive).
    pub fn ascent(&self, size: f32) -> f32 {
        self.ascender as f32 * size / self.units_per_em as f32
    }
    /// Descent below the baseline, in pixels (positive).
    pub fn descent(&self, size: f32) -> f32 {
        -(self.descender as f32) * size / self.units_per_em as f32
    }
    pub fn line_gap(&self, size: f32) -> f32 {
        self.line_gap as f32 * size / self.units_per_em as f32
    }

    /// The outline of `glyph` scaled to `size` pixels per em, y down, origin at the glyph's
    /// baseline origin. Returns false for glyphs without an outline (spaces).
    pub fn outline(&self, face: &ttf_parser::Face<'_>, glyph: u16, size: f32, path: &mut Path) -> bool {
        let scale = size / self.units_per_em as f32;
        let mut b = OutlineBuilder { path, scale, any: false };
        face.outline_glyph(ttf_parser::GlyphId(glyph), &mut b).is_some() && b.any
    }
}

struct OutlineBuilder<'a> {
    path: &'a mut Path,
    scale: f32,
    any: bool,
}

impl ttf_parser::OutlineBuilder for OutlineBuilder<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.any = true;
        self.path.move_to(Point::new(x * self.scale, -y * self.scale));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(Point::new(x * self.scale, -y * self.scale));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path.quad_to(Point::new(x1 * self.scale, -y1 * self.scale), Point::new(x * self.scale, -y * self.scale));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path.cubic_to(
            Point::new(x1 * self.scale, -y1 * self.scale),
            Point::new(x2 * self.scale, -y2 * self.scale),
            Point::new(x * self.scale, -y * self.scale),
        );
    }
    fn close(&mut self) {
        self.path.close();
    }
}

/// Handle of a face inside a [`Fonts`] registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontId(pub u32);

/// A fallback chain: the first font that has a glyph for a character is used.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontChain(pub Vec<FontId>);

impl FontChain {
    pub fn new(ids: &[FontId]) -> FontChain {
        FontChain(ids.to_vec())
    }
}

#[derive(Debug)]
pub enum FontError {
    /// The bytes are not a font file this library can read.
    Invalid,
    Io(std::io::Error),
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FontError::Invalid => write!(f, "invalid font data / フォントのデータが正しくありません"),
            FontError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FontError {}

/// The font registry: fonts registered by the app and fonts found in the system. Create one,
/// keep it, and pass it to layout calls.
pub struct Fonts {
    pub(crate) faces: Vec<Arc<FontData>>,
    /// Faces that came from `register` (the app's own fonts), in order.
    registered: Vec<FontId>,
    by_file: HashMap<(String, u32), FontId>,
    pub(crate) layouts: crate::text::LayoutCache,
    system_enabled: bool,
}

impl Default for Fonts {
    fn default() -> Self {
        Fonts::new()
    }
}

impl Fonts {
    /// A registry that also searches the operating system's fonts (when the `system-fonts`
    /// feature is on).
    pub fn new() -> Fonts {
        Fonts {
            faces: Vec::new(),
            registered: Vec::new(),
            by_file: HashMap::new(),
            layouts: crate::text::LayoutCache::default(),
            system_enabled: cfg!(feature = "system-fonts"),
        }
    }

    /// A registry that never looks at the operating system (only `register`ed fonts).
    pub fn without_system_fonts() -> Fonts {
        Fonts { system_enabled: false, ..Fonts::new() }
    }

    /// Register font file bytes (a `.ttf`, `.otf`, or a `.ttc` collection: every face is added).
    /// Registered fonts win over system fonts in [`Fonts::query`].
    pub fn register(&mut self, bytes: impl Into<Arc<[u8]>>) -> Result<Vec<FontId>, FontError> {
        let bytes: Arc<[u8]> = bytes.into();
        let n = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
        let mut ids = Vec::new();
        for i in 0..n {
            let fd = FontData::new(bytes.clone(), i).ok_or(FontError::Invalid)?;
            let id = FontId(self.faces.len() as u32);
            self.faces.push(Arc::new(fd));
            self.registered.push(id);
            ids.push(id);
        }
        if ids.is_empty() { Err(FontError::Invalid) } else { Ok(ids) }
    }

    /// Read and register a font file.
    pub fn register_file(&mut self, path: impl AsRef<std::path::Path>) -> Result<Vec<FontId>, FontError> {
        let bytes = std::fs::read(path).map_err(FontError::Io)?;
        self.register(bytes)
    }

    pub fn get(&self, id: FontId) -> Option<&Arc<FontData>> {
        self.faces.get(id.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.faces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Load (once) a face from a font file in the system.
    pub(crate) fn load_system(&mut self, path: &str, index: u32) -> Option<FontId> {
        if let Some(id) = self.by_file.get(&(path.to_string(), index)) {
            return Some(*id);
        }
        let bytes: Arc<[u8]> = Arc::from(std::fs::read(path).ok()?);
        let fd = FontData::new(bytes, index)?;
        let id = FontId(self.faces.len() as u32);
        self.faces.push(Arc::new(fd));
        self.by_file.insert((path.to_string(), index), id);
        Some(id)
    }

    /// Find fonts for `q` and return the fallback chain (see [`FontQuery`]). The order is:
    /// registered fonts of the requested families, system fonts of the requested families, the
    /// generic family and the language's fallbacks from the system, then the remaining
    /// registered fonts.
    pub fn query(&mut self, q: &FontQuery<'_>) -> FontChain {
        let mut chain: Vec<FontId> = Vec::new();
        let push = |chain: &mut Vec<FontId>, id: FontId| {
            if !chain.contains(&id) {
                chain.push(id);
            }
        };
        let want_w = q.weight as i32;
        // 1. registered fonts by family
        for fam in q.families {
            let mut best: Option<(i32, FontId)> = None;
            for id in &self.registered {
                let f = &self.faces[id.0 as usize];
                if f.family.eq_ignore_ascii_case(fam) {
                    let score = (f.weight as i32 - want_w).abs() + if f.italic != q.italic { 1000 } else { 0 };
                    if best.is_none_or(|(s, _)| score < s) {
                        best = Some((score, *id));
                    }
                }
            }
            if let Some((_, id)) = best {
                push(&mut chain, id);
            }
        }
        // 2. system fonts by family, then generic and language fallbacks
        if self.system_enabled {
            let mut names: Vec<&str> = q.families.to_vec();
            let extra = crate::fonts::fallback_families(q.generic, q.language);
            names.extend(extra.iter().copied());
            for name in names {
                if let Some((path, index)) = crate::fonts::find_system_font(name, q.weight, q.italic) {
                    if let Some(id) = self.load_system(&path, index) {
                        push(&mut chain, id);
                    }
                }
            }
        }
        // 3. everything else that was registered
        for id in self.registered.clone() {
            push(&mut chain, id);
        }
        FontChain(chain)
    }

    /// Make a chain out of registered faces only (all of them, in registration order).
    pub fn registered_chain(&self) -> FontChain {
        FontChain(self.registered.clone())
    }
}
