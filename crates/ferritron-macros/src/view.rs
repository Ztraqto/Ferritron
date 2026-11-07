//! `[view]` in the project file: the plugin's editor.
//!
//! ```toml
//! [view]
//! backend = "web"        # "web" (default) or "gpu"
//! entry   = "ui"         # web: folder with index.html, embedded into the binary
//! editor  = "Editor"     # gpu: the type implementing `ferritron::gpu_view::GpuEditor`
//! size    = [480, 320]   # logical pixels at 100% scale
//! resizable = true       # optional, default false
//! min_size  = [240, 160] # optional when resizable, default half of `size`
//! max_size  = [1920, 1080] # optional when resizable, default 4096 x 4096
//! process   = true       # optional: run the editor in a separate process (Windows)
//! background = "#1e1e1e" # optional, web: painted until the page draws itself
//! preload   = false      # optional, web (default true): start the browser behind
//!                        # the editor when an instance is created, so it opens fast
//! ```

use std::path::{Path, PathBuf};
use toml::{Table, Value};

pub enum Backend {
    /// (path relative to the UI folder with `/` separators, absolute file path)
    Web(Vec<(String, PathBuf)>),
    /// The editor type's path.
    Gpu(String),
}

pub struct View {
    pub backend: Backend,
    /// The editor runs in a separate process.
    pub process: bool,
    pub width: u32,
    pub height: u32,
    pub min: (u32, u32),
    pub max: (u32, u32),
    /// Web: `background = "#rrggbb"`.
    pub background: Option<[u8; 3]>,
    /// Web: start the browser when an instance is created.
    pub preload: bool,
}

pub fn parse(root: &Table, manifest_dir: &Path) -> Result<Option<View>, String> {
    let Some(view) = root.get("view") else { return Ok(None) };
    let view = view.as_table().ok_or("[view] must be a table")?;
    const KEYS: [&str; 10] = ["backend", "entry", "editor", "size", "resizable", "min_size", "max_size", "process", "background", "preload"];
    if let Some(key) = view.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("unknown key `view.{key}` (allowed: {})", KEYS.join(", ")));
    }
    let gpu = match view.get("backend").map(|b| b.as_str()) {
        None | Some(Some("web")) => false,
        Some(Some("gpu")) => true,
        _ => return Err("view.backend must be \"web\" or \"gpu\"".into()),
    };
    if gpu && view.contains_key("entry") {
        return Err("view.entry is for web views; a gpu view names its type with view.editor".into());
    }
    if !gpu && view.contains_key("editor") {
        return Err("view.editor is for gpu views (backend = \"gpu\")".into());
    }
    if gpu && (view.contains_key("background") || view.contains_key("preload")) {
        return Err("view.background and view.preload are for web views; a gpu view draws itself from the start".into());
    }
    let background = match view.get("background") {
        None => None,
        Some(v) => Some(v.as_str().and_then(parse_color).ok_or("view.background must be a color \"#rrggbb\"")?),
    };
    let preload = match view.get("preload") {
        Some(v) => v.as_bool().ok_or("view.preload must be true or false")?,
        None => true,
    };
    let entry = match view.get("entry") {
        Some(v) => v.as_str().ok_or("view.entry must be a folder name")?,
        None => "ui",
    };
    let pair = |key: &str| -> Result<Option<(u32, u32)>, String> {
        let Some(v) = view.get(key) else { return Ok(None) };
        let dim = |v: &Value| v.as_integer().filter(|n| (100..=4096).contains(n)).map(|n| n as u32);
        let a = v.as_array().filter(|a| a.len() == 2);
        match a.map(|a| (dim(&a[0]), dim(&a[1]))) {
            Some((Some(w), Some(h))) => Ok(Some((w, h))),
            _ => Err(format!("view.{key} must be [width, height] in pixels, each between 100 and 4096")),
        }
    };
    let (width, height) = pair("size")?.ok_or("view.size = [width, height] is required")?;
    let resizable = match view.get("resizable") {
        Some(v) => v.as_bool().ok_or("view.resizable must be true or false")?,
        None => false,
    };
    let process = match view.get("process") {
        Some(v) => v.as_bool().ok_or("view.process must be true or false")?,
        None => false,
    };
    let (min, max) = if resizable {
        let min = pair("min_size")?.unwrap_or(((width / 2).max(100), (height / 2).max(100)));
        let max = pair("max_size")?.unwrap_or((4096, 4096));
        if min.0 > width || min.1 > height || max.0 < width || max.1 < height {
            return Err("view.size must lie between view.min_size and view.max_size".into());
        }
        (min, max)
    } else {
        if view.contains_key("min_size") || view.contains_key("max_size") {
            return Err("view.min_size / view.max_size need `resizable = true`".into());
        }
        ((width, height), (width, height))
    };

    if gpu {
        let editor = view.get("editor").ok_or("a gpu view needs view.editor = \"<type implementing GpuEditor>\"")?;
        let editor = editor.as_str().filter(|e| is_path(e)).ok_or("view.editor must be a Rust type path, e.g. \"Editor\" or \"ui::Editor\"")?;
        return Ok(Some(View { backend: Backend::Gpu(editor.to_string()), process, width, height, min, max, background, preload }));
    }
    let folder = manifest_dir.join(entry);
    let mut files = Vec::new();
    collect(&folder, "", &mut files).map_err(|e| format!("cannot read view folder `{entry}`: {e}"))?;
    files.sort();
    if !files.iter().any(|(p, _)| p == "index.html") {
        return Err(format!("view folder `{entry}` has no index.html"));
    }
    Ok(Some(View { backend: Backend::Web(files), process, width, height, min, max, background, preload }))
}

fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#').filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()))?;
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn is_path(s: &str) -> bool {
    let ident = |p: &str| p.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && p.chars().all(|c| c.is_alphanumeric() || c == '_');
    !s.is_empty() && s.split("::").all(ident)
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if entry.file_type()?.is_dir() {
            collect(&entry.path(), &rel, out)?;
        } else {
            out.push((rel, entry.path()));
        }
    }
    Ok(())
}

/// Items for the plugin's module, and the body of `PluginMeta::view`.
pub fn generate(view: &View, self_ty: &str) -> (String, String) {
    let mut method = "fn view() -> ::core::option::Option<&'static ::ferritron::abi::ViewApi> { ::core::option::Option::Some(&__FERRITRON_VIEW) }".to_string();
    if view.process {
        method.push_str("\n    const VIEW_PROCESS: bool = true;");
    }
    let files = match &view.backend {
        Backend::Web(files) => files,
        Backend::Gpu(editor) => {
            let items = format!(
                "::ferritron::__require_gpu_view!();
static __FERRITRON_VIEW: ::ferritron::abi::ViewApi = ::ferritron::gpu_view::view_api::<{self_ty}, {editor}>(({}, {}), {:?}, {:?});
",
                view.width, view.height, view.min, view.max,
            );
            return (items, method);
        }
    };
    let assets: Vec<String> = files
        .iter()
        .map(|(rel, path)| format!("({rel:?}, ::core::include_bytes!({:?}))", path.display().to_string()))
        .collect();
    // An editor in its own process gains nothing from a browser started in the host's.
    if view.preload && !view.process {
        method.push_str("\n    fn preload_view(created: bool) { ::ferritron::web::preload::<Self>(created) }");
    }
    let background = match view.background {
        Some([r, g, b]) => format!("\n    const BACKGROUND: ::core::option::Option<[u8; 3]> = ::core::option::Option::Some([{r}, {g}, {b}]);"),
        None => String::new(),
    };
    let items = format!(
        "::ferritron::__require_web!();
impl ::ferritron::web::WebAssets for {self_ty} {{
    const ASSETS: &'static [(&'static str, &'static [u8])] = &[{}];{background}
    const PRELOAD: bool = {};
}}
static __FERRITRON_VIEW: ::ferritron::abi::ViewApi = ::ferritron::web::view_api::<{self_ty}>(({}, {}), {:?}, {:?});
",
        assets.join(", "),
        view.preload,
        view.width,
        view.height,
        view.min,
        view.max,
    );
    (items, method)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(toml: &str) -> Result<Option<View>, String> {
        parse(&toml.parse::<Table>().unwrap(), Path::new("."))
    }

    #[test]
    fn gpu_views_name_their_editor_type() {
        let v = view("[view]\nbackend = \"gpu\"\neditor = \"ui::Editor\"\nsize = [400, 200]").unwrap().unwrap();
        assert!(matches!(v.backend, Backend::Gpu(ref e) if e == "ui::Editor"));
        let (items, _) = generate(&v, "Plugin");
        assert!(items.contains("gpu_view::view_api::<Plugin, ui::Editor>((400, 200)"), "{items}");
        assert!(view("[view]\nbackend = \"gpu\"\nsize = [400, 200]").err().unwrap().contains("view.editor"));
        assert!(view("[view]\nbackend = \"gpu\"\neditor = \"a b\"\nsize = [400, 200]").is_err());
        assert!(view("[view]\nbackend = \"gpu\"\neditor = \"E\"\nentry = \"ui\"\nsize = [400, 200]").is_err());
        assert!(view("[view]\neditor = \"E\"\nsize = [400, 200]").err().unwrap().contains("gpu views"));
        assert!(view("[view]\nbackend = \"slint\"\nsize = [400, 200]").is_err());
        let v = view("[view]\nbackend = \"gpu\"\neditor = \"E\"\nsize = [400, 200]\nprocess = true").unwrap().unwrap();
        assert!(generate(&v, "P").1.contains("const VIEW_PROCESS: bool = true;"));
        assert!(view("[view]\nbackend = \"gpu\"\neditor = \"E\"\nsize = [400, 200]\npreload = false").is_err());
    }

    #[test]
    fn web_views_take_a_background_color_and_preload() {
        assert_eq!(parse_color("#1E2f30"), Some([0x1e, 0x2f, 0x30]));
        assert_eq!(parse_color("1e2f30"), None);
        assert_eq!(parse_color("#1e2f3"), None);
        assert_eq!(parse_color("#1e2f3g"), None);
        let v = View {
            backend: Backend::Web(Vec::new()),
            process: false,
            width: 400,
            height: 200,
            min: (400, 200),
            max: (400, 200),
            background: Some([1, 2, 3]),
            preload: true,
        };
        let (items, method) = generate(&v, "P");
        assert!(items.contains("Some([1, 2, 3])") && items.contains("const PRELOAD: bool = true;"), "{items}");
        assert!(method.contains("fn preload_view"), "{method}");
        // Off, or with the editor in its own process: nothing starts early.
        let v = View { preload: false, background: None, ..v };
        assert!(!generate(&v, "P").1.contains("preload_view"));
        let v = View { preload: true, process: true, ..v };
        assert!(!generate(&v, "P").1.contains("preload_view"));
    }
}
