//! The VST3 bundle's moduleinfo.json, rendered from the VST3 format crate's templates and a project
//! file (what `cargo ferritron bundle` writes): it must be valid JSON, name the same classes the
//! shim's factory lists, and carry `[vst3] compatibility`.

use ferritron_manifest::{Escape, parse, render};
use std::path::PathBuf;

fn read(relative: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

fn moduleinfo(project: &str, template: &str) -> serde_json::Value {
    let manifest = parse(project).unwrap();
    let text = render(&read(template), &manifest.template_scope(), Escape::for_file("moduleinfo.json")).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{template} gives invalid JSON ({e}):\n{text}"))
}

#[test]
fn all_three_classes_and_the_compatibility_list() {
    let project = read("../../examples/vst3-ids/vst3-ids.ferritroject");
    // 2.0: an explicit GUID shows as written on every system (on Windows the bytes are stored in COM order).
    for (template, class, controller) in [
        ("../ferritron-vst3/moduleinfo.json", "3E6B2A904C174D619D5B1F7A0C2E8B11", "A1C4F7E26B384C908F2D5E7B3A91C640"),
        ("../ferritron-vst3/moduleinfo-windows.json", "3E6B2A904C174D619D5B1F7A0C2E8B11", "A1C4F7E26B384C908F2D5E7B3A91C640"),
    ] {
        let info = moduleinfo(&project, template);
        let classes = info["Classes"].as_array().unwrap();
        assert_eq!(classes.len(), 2, "{template}: component and controller");
        assert_eq!((classes[0]["CID"].as_str(), classes[0]["Category"].as_str()), (Some(class), Some("Audio Module Class")), "{template}");
        assert_eq!((classes[1]["CID"].as_str(), classes[1]["Category"].as_str()), (Some(controller), Some("Component Controller Class")), "{template}");
        assert_eq!(classes[0]["Sub Categories"], serde_json::json!(["Fx", "EQ"]));
        assert_eq!(info["Compatibility"][0]["New"].as_str(), Some(class));
        assert_eq!(info["Compatibility"][0]["Old"], serde_json::json!(["11111111222233334444555555555555", "AAAAAAAABBBBCCCCDDDDEEEEEEEEEEEE"]));
        assert_eq!(info["Factory Info"]["Vendor"], "Ztraqto");
    }
}

#[test]
fn a_plain_plugin_has_one_class_and_no_compatibility() {
    let project = "[plugin]\nid = \"dev.example.plain\"\nname = \"Plain \\\"One\\\"\"\n";
    let info = moduleinfo(project, "../ferritron-vst3/moduleinfo.json");
    assert_eq!(info["Classes"].as_array().unwrap().len(), 1);
    assert!(info.get("Compatibility").is_none());
    assert_eq!(info["Classes"][0]["Name"], "Plain \"One\"", "names are escaped");
    // 32 hex digits, derived from the plugin ID
    let cid = info["Classes"][0]["CID"].as_str().unwrap();
    assert!(cid.len() == 32 && cid.chars().all(|c| c.is_ascii_hexdigit()), "{cid}");
    assert_eq!(cid, ferritron_manifest::vst3_uid_string(&ferritron_manifest::class_id("dev.example.plain"), false));
}
