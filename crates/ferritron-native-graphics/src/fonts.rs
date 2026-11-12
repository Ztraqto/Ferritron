//! Finding fonts in the operating system.
//!
//! All operating-system specific code of the library lives here. Windows uses DirectWrite.
//! macOS / iOS (CoreText), Linux (fontconfig), Android (fonts.xml) and Web (registered fonts
//! only) are stage M6 and not written yet: there `find_system_font` finds nothing, and only
//! fonts given to [`Fonts::register`](crate::Fonts::register) are used.

/// The generic family to fall back to when a requested family is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Generic {
    SansSerif,
    Serif,
    Monospace,
}

/// What [`Fonts::query`](crate::Fonts::query) looks for.
#[derive(Clone, Copy, Debug)]
pub struct FontQuery<'a> {
    /// Preferred families in order; missing ones are skipped.
    pub families: &'a [&'a str],
    pub generic: Generic,
    /// 100..=900 (400 regular, 700 bold).
    pub weight: u16,
    pub italic: bool,
    /// BCP 47 language such as `"ja"`, `"zh-Hans"`, `"ko"`: picks the right CJK fallbacks.
    pub language: Option<&'a str>,
}

impl Default for FontQuery<'_> {
    fn default() -> Self {
        FontQuery { families: &[], generic: Generic::SansSerif, weight: 400, italic: false, language: None }
    }
}

/// Families that follow the requested ones (generic family first, then the language's
/// fallbacks, then a Japanese-capable font so that plain queries can still show Japanese).
pub(crate) fn fallback_families(generic: Generic, language: Option<&str>) -> Vec<&'static str> {
    let lang = language.unwrap_or("").to_ascii_lowercase();
    let mut v: Vec<&'static str> = Vec::new();
    if cfg!(windows) {
        match generic {
            Generic::SansSerif => v.push("Segoe UI"),
            Generic::Serif => v.push("Times New Roman"),
            Generic::Monospace => v.push("Consolas"),
        }
        let cjk: &[&'static str] = if lang.starts_with("ja") {
            match generic {
                Generic::Serif => &["Yu Mincho", "MS Mincho", "Yu Gothic UI", "Meiryo UI"],
                Generic::Monospace => &["BIZ UDGothic", "MS Gothic", "Yu Gothic UI"],
                Generic::SansSerif => &["Yu Gothic UI", "Meiryo UI", "Yu Gothic", "Meiryo", "MS UI Gothic"],
            }
        } else if lang.starts_with("zh-hant") || lang == "zh-tw" || lang == "zh-hk" {
            &["Microsoft JhengHei UI", "Yu Gothic UI"]
        } else if lang.starts_with("zh") {
            &["Microsoft YaHei UI", "Yu Gothic UI"]
        } else if lang.starts_with("ko") {
            &["Malgun Gothic", "Yu Gothic UI"]
        } else {
            &["Yu Gothic UI", "Meiryo UI", "Microsoft YaHei UI", "Malgun Gothic"]
        };
        v.extend_from_slice(cjk);
        v.push("Segoe UI Symbol");
    } else {
        v.push(generic_default(generic));
    }
    v
}

fn generic_default(g: Generic) -> &'static str {
    match g {
        Generic::SansSerif => "sans-serif",
        Generic::Serif => "serif",
        Generic::Monospace => "monospace",
    }
}

/// Look up a family in the system and return the font file and the face index in it.
pub(crate) fn find_system_font(family: &str, weight: u16, italic: bool) -> Option<(String, u32)> {
    #[cfg(all(windows, feature = "system-fonts"))]
    {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        type Cache = Mutex<HashMap<(String, u16, bool), Option<(String, u32)>>>;
        static CACHE: OnceLock<Cache> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let key = (family.to_ascii_lowercase(), weight, italic);
        if let Some(hit) = cache.lock().ok()?.get(&key) {
            return hit.clone();
        }
        let found = windows_impl::find(family, weight, italic);
        if let Ok(mut c) = cache.lock() {
            c.insert(key, found.clone());
        }
        found
    }
    #[cfg(not(all(windows, feature = "system-fonts")))]
    {
        let _ = (family, weight, italic);
        None
    }
}

#[cfg(all(windows, feature = "system-fonts"))]
mod windows_impl {
    
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_WEIGHT, DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteFontFile, IDWriteLocalFontFileLoader,
    };
    use windows::core::{BOOL, Interface, PCWSTR};

    pub fn find(family: &str, weight: u16, italic: bool) -> Option<(String, u32)> {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
            let mut coll: Option<IDWriteFontCollection> = None;
            factory.GetSystemFontCollection(&mut coll, false).ok()?;
            let coll = coll?;
            let wide: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
            let mut index = 0u32;
            let mut exists = BOOL(0);
            coll.FindFamilyName(PCWSTR(wide.as_ptr()), &mut index, &mut exists).ok()?;
            if !exists.as_bool() {
                return None;
            }
            let fam = coll.GetFontFamily(index).ok()?;
            let style = if italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
            let font = fam.GetFirstMatchingFont(DWRITE_FONT_WEIGHT(weight as i32), DWRITE_FONT_STRETCH_NORMAL, style).ok()?;
            let face = font.CreateFontFace().ok()?;
            let mut n = 0u32;
            face.GetFiles(&mut n, None).ok()?;
            if n == 0 {
                return None;
            }
            let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
            face.GetFiles(&mut n, Some(files.as_mut_ptr())).ok()?;
            let file = files.into_iter().next()??;
            let mut key: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut key_size = 0u32;
            file.GetReferenceKey(&mut key as *mut _ as *mut *mut core::ffi::c_void, &mut key_size).ok()?;
            let loader = file.GetLoader().ok()?;
            let local: IDWriteLocalFontFileLoader = loader.cast().ok()?;
            let len = local.GetFilePathLengthFromKey(key, key_size).ok()?;
            let mut buf = vec![0u16; len as usize + 1];
            local.GetFilePathFromKey(key, key_size, &mut buf).ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            Some((path, face.GetIndex()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallbacks_for_japanese_start_with_a_japanese_font_after_the_latin_one() {
        let v = fallback_families(Generic::SansSerif, Some("ja"));
        if cfg!(windows) {
            assert_eq!(v[0], "Segoe UI");
            assert_eq!(v[1], "Yu Gothic UI");
        }
        assert!(!v.is_empty());
    }

    #[cfg(all(windows, feature = "system-fonts"))]
    #[test]
    fn finds_segoe_ui_on_windows() {
        let (path, _) = find_system_font("Segoe UI", 400, false).expect("Segoe UI is part of Windows");
        assert!(path.to_ascii_lowercase().ends_with(".ttf") || path.to_ascii_lowercase().ends_with(".ttc"), "{path}");
        assert!(find_system_font("No Such Family 12345", 400, false).is_none());
    }
}
