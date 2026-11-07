//! `cargo ferritron i18n check <package>`: lists untranslated strings per language.

use std::path::Path;

pub fn check(package_dir: &Path) -> Result<(), String> {
    let manifest = ferritron_manifest::locate(package_dir)?;
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", manifest.display()))?;
    let mut expected = expected_keys(&table);
    // Editor messages: every key the base language (en) defines.
    for file in std::fs::read_dir(package_dir.join("locales").join("en")).into_iter().flatten().flatten() {
        let source = std::fs::read_to_string(file.path()).unwrap_or_default();
        for key in message_ids(&source) {
            if !key.starts_with("param-") && !expected.contains(&key) {
                expected.push(key);
            }
        }
    }

    let mut languages: Vec<_> = std::fs::read_dir(package_dir.join("locales"))
        .map_err(|_| format!("{} has no locales/ folder", package_dir.display()))?
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    languages.sort_by_key(|e| e.file_name());

    let mut incomplete = 0;
    for language in languages {
        let mut present = Vec::new();
        for file in std::fs::read_dir(language.path()).into_iter().flatten().flatten() {
            if file.path().extension().is_some_and(|e| e == "ftl") {
                let source = std::fs::read_to_string(file.path()).unwrap_or_default();
                present.extend(message_ids(&source));
            }
        }
        let name = language.file_name().to_string_lossy().into_owned();
        // English is the base language: parameter names already come from the project file.
        let wanted: Vec<&String> = expected.iter().filter(|k| name != "en" || !k.starts_with("param-")).collect();
        let missing: Vec<&&String> = wanted.iter().filter(|k| !present.contains(k)).collect();
        if missing.is_empty() {
            println!("{name}: complete ({} strings)", wanted.len());
        } else {
            incomplete += 1;
            println!("{name}: {} of {} strings missing", missing.len(), wanted.len());
            for key in missing {
                println!("  {key}");
            }
        }
    }
    if incomplete > 0 { Err(format!("{incomplete} language(s) incomplete")) } else { Ok(()) }
}

/// `param-<key>` for every parameter and `param-<key>-<n>` for every enum choice.
fn expected_keys(manifest: &toml::Table) -> Vec<String> {
    let mut keys = Vec::new();
    for (key, param) in manifest.get("params").and_then(|p| p.as_table()).into_iter().flatten() {
        keys.push(format!("param-{key}"));
        if param.get("type").and_then(|t| t.as_str()) == Some("enum") {
            let count = param.get("choices").and_then(|c| c.as_array()).map_or(0, Vec::len);
            keys.extend((0..count).map(|n| format!("param-{key}-{n}")));
        }
    }
    keys
}

fn message_ids(source: &str) -> Vec<String> {
    let resource = match fluent_syntax::parser::parse(source) {
        Ok(r) => r,
        Err((r, _)) => r,
    };
    resource
        .body
        .iter()
        .filter_map(|entry| match entry {
            fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.to_string()),
            _ => None,
        })
        .collect()
}
