//! Embedding and compile-time checking of `locales/<lang>/*.ftl`.

use crate::manifest::{Kind, Manifest};
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub struct Locale {
    pub language: String,
    pub files: Vec<PathBuf>,
}

/// Finds every locale, checks its Fluent syntax and that each `param-*` key
/// names a real parameter (or enum choice).
pub fn collect(manifest_dir: &Path, manifest: &Manifest) -> Result<Vec<Locale>, String> {
    let root = manifest_dir.join("locales");
    let Ok(entries) = std::fs::read_dir(&root) else { return Ok(Vec::new()) };
    let mut locales = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let language = entry.file_name().to_string_lossy().into_owned();
        if language.is_empty() || !language.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("locales/{language}: folder names must be language tags such as `ja` or `pt-BR`"));
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(entry.path())
            .map_err(|e| format!("cannot read locales/{language}: {e}"))?
            .flatten()
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e == "ftl"))
            .collect();
        files.sort();
        for file in &files {
            let shown = format!("locales/{language}/{}", file.file_name().unwrap().to_string_lossy());
            let source = std::fs::read_to_string(file).map_err(|e| format!("cannot read {shown}: {e}"))?;
            check(&source, manifest).map_err(|(offset, msg)| format!("{shown}:{}: {msg}", line_of(&source, offset)))?;
        }
        if !files.is_empty() {
            locales.push(Locale { language, files });
        }
    }
    locales.sort_by(|a, b| a.language.cmp(&b.language));
    Ok(locales)
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].matches('\n').count() + 1
}

fn check(source: &str, manifest: &Manifest) -> Result<(), (usize, String)> {
    let resource = fluent_syntax::parser::parse(source).map_err(|(_, errors)| {
        let e = &errors[0];
        (e.pos.start, format!("invalid Fluent syntax: {}", e.kind))
    })?;
    for entry in &resource.body {
        let fluent_syntax::ast::Entry::Message(message) = entry else { continue };
        let id = message.id.name;
        let Some(rest) = id.strip_prefix("param-") else { continue };
        let offset = id.as_ptr() as usize - source.as_ptr() as usize;
        let known = manifest.params.iter().any(|p| p.key == rest)
            || rest.rsplit_once('-').is_some_and(|(key, index)| {
                manifest.params.iter().any(|p| {
                    p.key == key
                        && matches!(&p.kind, Kind::Enum { choices } if index.parse::<usize>().is_ok_and(|i| i < choices.len()))
                })
            });
        if !known {
            return Err((offset, format!("`{id}` does not match any parameter in the project file (use `param-<key>` or `param-<key>-<choice index>`)")));
        }
    }
    Ok(())
}

pub fn generate(locales: &[Locale], self_ty: &str, extra_methods: &str) -> String {
    let mut s = String::new();
    let w = &mut s;
    // Proc macros cannot watch folders, so new .ftl or UI files would go
    // unnoticed. `cargo ferritron bundle` sets this variable to the file list;
    // reading it makes cargo rebuild when the list changes.
    writeln!(w, "const _: ::core::option::Option<&str> = ::core::option_env!(\"FERRITRON_ASSETS_STAMP\");").unwrap();
    writeln!(w, "static __FERRITRON_LOCALES: [(&str, &str); {}] = [", locales.len()).unwrap();
    for locale in locales {
        let parts: Vec<String> =
            locale.files.iter().map(|f| format!("::core::include_str!({:?}), \"\n\"", f.display().to_string())).collect();
        writeln!(w, "    ({:?}, ::core::concat!({})),", locale.language, parts.join(", ")).unwrap();
    }
    writeln!(w, "];").unwrap();
    writeln!(
        w,
        "impl ::ferritron::__private::PluginMeta for {self_ty} {{
    const INFO: &'static ::ferritron::PluginInfo = &__FERRITRON_INFO;
    fn localized() -> &'static ::ferritron::__private::Localized {{
        static LOCALIZED: ::std::sync::OnceLock<::ferritron::__private::Localized> = ::std::sync::OnceLock::new();
        LOCALIZED.get_or_init(|| ::ferritron::__private::Localized::new(
            __FERRITRON_INFO.id,
            <Params as ::ferritron::ParamSet>::infos(),
            &__FERRITRON_LOCALES,
        ))
    }}
    {extra_methods}
}}"
    )
    .unwrap();
    s
}
