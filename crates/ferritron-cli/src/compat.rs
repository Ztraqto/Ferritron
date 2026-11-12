//! T1: what `cargo ferritron` 2.x says about a plugin that still depends on Ferritron 1.x.

use std::path::PathBuf;

fn major(version: &str) -> u32 {
    version.split(['.', '-', '+']).next().and_then(|m| m.parse().ok()).unwrap_or(0)
}

/// Where the migration guide is: in the Ferritron folder (`FERRITRON_HOME`, when set).
fn guide() -> String {
    match std::env::var_os("FERRITRON_HOME").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home).join("docs").join("migration-2.0.md").display().to_string(),
        None => "docs/migration-2.0.md".into(),
    }
}

/// The message to show when this tool (`ours`) is 2.0 or newer and the plugin's `ferritron`
/// dependency (`plugin`) is 1.x; `None` when they fit. 1.x plugins are not handled as they are:
/// the project file keys, `Context` and the C ABI changed (see the guide).
pub fn legacy_plugin_notice(ours: &str, plugin: &str) -> Option<String> {
    (major(ours) >= 2 && major(plugin) < 2).then(|| {
        let guide = guide();
        if crate::lang::japanese() {
            format!("このプラグインは Ferritron {plugin} を使っています。cargo-ferritron {ours} は 1.x のプラグインをそのままでは扱えません。移行の手順は {guide} を見てください（`ferritron` の依存を {ours} に上げ、Context のフィールドをメソッドに置き換えます）。")
        } else {
            format!("This plugin uses Ferritron {plugin}. cargo-ferritron {ours} cannot handle 1.x plugins as they are. Move it to 2.0 with the guide in {guide} (raise the `ferritron` dependency to {ours} and replace the Context fields by methods).")
        }
    })
}

/// The version of the `ferritron` package in `cargo metadata`.
pub fn plugin_framework_version(metadata: &serde_json::Value) -> Option<String> {
    metadata["packages"].as_array()?.iter().find(|p| p["name"] == "ferritron")?["version"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_two_point_tool_with_a_one_point_plugin_gets_the_notice() {
        let note = legacy_plugin_notice("2.0.0", "1.4.0").unwrap();
        assert!(note.contains("1.4.0") && note.contains("2.0.0") && note.contains("migration-2.0.md"), "{note}");
        assert!(legacy_plugin_notice("2.0.0", "2.0.0").is_none());
        assert!(legacy_plugin_notice("2.1.3", "2.0.0-rc1").is_none());
        assert!(legacy_plugin_notice("1.4.0", "1.4.0").is_none(), "a 1.x tool handles 1.x");
        assert!(legacy_plugin_notice("1.4.0", "1.3.0").is_none(), "that is the deprecation warning's job");
    }

    #[test]
    fn finds_the_framework_version() {
        let m = serde_json::json!({ "packages": [ { "name": "serde", "version": "1.0.0" }, { "name": "ferritron", "version": "1.4.0" } ] });
        assert_eq!(plugin_framework_version(&m).as_deref(), Some("1.4.0"));
        assert_eq!(plugin_framework_version(&serde_json::json!({ "packages": [] })), None);
    }
}
