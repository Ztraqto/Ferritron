//! File dialogs for `ferritron.openFile` / `ferritron.saveFile` (E12): the page sends
//! `dialog:<n>:<open|save>:<title>:<dir>:<filters>:<name>` (fields percent-encoded, filters as
//! `Name|ext;ext~Name|ext`) and gets the path back through `ferritron._dialog(n, path)`.
//! Windows shows the system dialog on the UI thread, owned by the host's window; other platforms
//! answer "nothing chosen". `FERRITRON_WEB_DIALOG_ANSWER` (a path, or `cancel`) answers
//! without showing anything, for tests that cannot click.

/// What the page asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u32,
    pub save: bool,
    pub title: String,
    pub dir: String,
    /// `(name, extensions)`
    pub filters: Vec<(String, Vec<String>)>,
    /// The file name to suggest (save).
    pub name: String,
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads the message the bridge sends (the part after `dialog:`).
pub fn parse(rest: &str) -> Option<Request> {
    let mut parts = rest.split(':');
    let id = parts.next()?.parse().ok()?;
    let save = match parts.next()? {
        "open" => false,
        "save" => true,
        _ => return None,
    };
    let (title, dir, filters, name) = (percent_decode(parts.next()?), percent_decode(parts.next()?), percent_decode(parts.next()?), percent_decode(parts.next()?));
    let filters = filters
        .split('~')
        .filter(|f| !f.is_empty())
        .map(|f| {
            let (label, exts) = f.split_once('|').unwrap_or((f, ""));
            (label.to_string(), exts.split(';').map(|e| e.trim().trim_start_matches("*.").trim_start_matches('.').to_string()).filter(|e| !e.is_empty()).collect())
        })
        .collect();
    Some(Request { id, save, title, dir, filters, name })
}

/// The double-null-terminated filter list of the Windows dialogs: `Name (*.a;*.b)\0*.a;*.b\0...\0\0`.
/// A filter without extensions matches everything. Without filters: all files.
pub fn filter_list(filters: &[(String, Vec<String>)]) -> Vec<u16> {
    let mut text = String::new();
    for (label, exts) in filters {
        let pattern = if exts.is_empty() { "*.*".to_string() } else { exts.iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";") };
        text.push_str(&format!("{label} ({pattern})\0{pattern}\0"));
    }
    if filters.is_empty() {
        text.push_str("All files (*.*)\0*.*\0");
    }
    text.push('\0');
    text.encode_utf16().collect()
}

/// The path chosen by the user, `None` when cancelled or not possible here.
pub fn ask(request: &Request, owner: *mut std::ffi::c_void) -> Option<String> {
    if let Ok(answer) = std::env::var("FERRITRON_WEB_DIALOG_ANSWER") {
        return (answer != "cancel").then_some(answer);
    }
    platform::ask(request, owner)
}

/// The script that hands the answer to the page.
pub fn answer_script(id: u32, path: Option<&str>) -> String {
    let value = path.map_or_else(|| "null".to_string(), crate::json_str);
    format!("ferritron._dialog({id},{value});")
}

#[cfg(windows)]
mod platform {
    use super::{Request, filter_list};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn ask(request: &Request, owner: *mut std::ffi::c_void) -> Option<String> {
        let filters = filter_list(&request.filters);
        let (title, dir) = (wide(&request.title), wide(&request.dir));
        let mut file = vec![0u16; 4096];
        for (slot, c) in file.iter_mut().zip(request.name.encode_utf16().take(4000)) {
            *slot = c;
        }
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: HWND(owner),
            lpstrFilter: PCWSTR(filters.as_ptr()),
            nFilterIndex: 1,
            lpstrFile: PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrInitialDir: if request.dir.is_empty() { PCWSTR::null() } else { PCWSTR(dir.as_ptr()) },
            lpstrTitle: if request.title.is_empty() { PCWSTR::null() } else { PCWSTR(title.as_ptr()) },
            Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | if request.save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST },
            ..Default::default()
        };
        let ok = unsafe { if request.save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) } };
        if !ok.as_bool() {
            return None;
        }
        let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        Some(String::from_utf16_lossy(&file[..len]))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Request;

    /// No file dialog here yet (macOS needs NSOpenPanel / NSSavePanel).
    pub fn ask(_request: &Request, _owner: *mut std::ffi::c_void) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_escapes_and_utf8() {
        assert_eq!(percent_decode("a%20b%3Ac"), "a b:c");
        assert_eq!(percent_decode("%E3%83%97%E3%83%AA%E3%82%BB%E3%83%83%E3%83%88"), "プリセット");
        assert_eq!(percent_decode("100%"), "100%", "a stray percent stays");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn reads_the_pages_message() {
        // encodeURIComponent("Audio|wav;aif~All|*") is what the bridge sends
        let r = parse("7:save:Save%20preset:C%3A%5CPresets:Preset%7Cjson~All%7C*:my%20preset.json").unwrap();
        assert_eq!(r.id, 7);
        assert!(r.save);
        assert_eq!((r.title.as_str(), r.dir.as_str(), r.name.as_str()), ("Save preset", "C:\\Presets", "my preset.json"));
        assert_eq!(r.filters, vec![("Preset".to_string(), vec!["json".to_string()]), ("All".to_string(), vec!["*".to_string()])]);
        let open = parse("1:open::::").unwrap();
        assert!(!open.save && open.filters.is_empty() && open.title.is_empty());
        assert!(parse("x:open::::").is_none());
        assert!(parse("1:delete::::").is_none());
        assert!(parse("1:open:").is_none());
    }

    #[test]
    fn extensions_may_be_written_with_dots_and_stars() {
        let r = parse("2:open:::A%7C.wav%3B*.aif%3B%20flac:").unwrap();
        assert_eq!(r.filters, vec![("A".to_string(), vec!["wav".to_string(), "aif".to_string(), "flac".to_string()])]);
    }

    #[test]
    fn the_windows_filter_list_ends_with_two_nulls() {
        let list = filter_list(&[("Audio".into(), vec!["wav".into(), "aif".into()]), ("All".into(), vec![])]);
        let text = String::from_utf16(&list).unwrap();
        assert_eq!(text, "Audio (*.wav;*.aif)\0*.wav;*.aif\0All (*.*)\0*.*\0\0");
        assert_eq!(String::from_utf16(&filter_list(&[])).unwrap(), "All files (*.*)\0*.*\0\0");
    }

    #[test]
    fn answers_become_a_script() {
        assert_eq!(answer_script(3, Some("C:\\a\\b.wav")), "ferritron._dialog(3,\"C:\\\\a\\\\b.wav\");");
        assert_eq!(answer_script(4, None), "ferritron._dialog(4,null);");
    }
}
