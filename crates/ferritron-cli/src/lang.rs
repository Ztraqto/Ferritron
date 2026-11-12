//! The language of the tool's own messages: Japanese when `FERRITRON_LANGUAGE` (else the
//! computer's language) is Japanese, English otherwise. Messages that appear in both
//! languages are written next to each other with [`tr`].

use std::sync::OnceLock;

/// Whether messages are Japanese.
pub fn japanese() -> bool {
    static JA: OnceLock<bool> = OnceLock::new();
    *JA.get_or_init(|| {
        let from_env = std::env::var("FERRITRON_LANGUAGE").ok().filter(|v| !v.is_empty());
        is_japanese(from_env.or_else(os_language).as_deref().unwrap_or(""))
    })
}

fn is_japanese(language: &str) -> bool {
    language.to_ascii_lowercase().starts_with("ja")
}

/// `ja` in Japanese, else `en`.
pub fn tr<'a>(ja: &'a str, en: &'a str) -> &'a str {
    if japanese() { ja } else { en }
}

#[cfg(windows)]
fn os_language() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
    }
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    (len > 1).then(|| String::from_utf16_lossy(&buf[..len as usize - 1]))
}

#[cfg(not(windows))]
fn os_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty() && v != "C"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags() {
        assert!(is_japanese("ja") && is_japanese("ja-JP") && is_japanese("JA_JP.UTF-8"));
        assert!(!is_japanese("en-US") && !is_japanese("") && !is_japanese("de"));
    }
}
