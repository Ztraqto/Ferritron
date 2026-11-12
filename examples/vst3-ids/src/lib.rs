//! A gain with fixed VST3 class IDs, an edit controller registered as a class of its own and a
//! compatibility list (see `vst3-ids.ferritroject`). Test it with
//! `cargo ferritron bundle vst3-ids` and `ferritron-testhost` (it lists the classes, creates the
//! controller class and asks `IRemapParamID`).

use ferritron::prelude::*;

#[derive(Default)]
struct Vst3Ids;

#[ferritron::plugin]
impl Process for Vst3Ids {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params().gain.value());
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_id_written_in_the_project_is_the_one_the_plugin_has() {
        let info = <Vst3Ids as ferritron::__private::PluginMeta>::INFO;
        let written = ferritron_manifest::parse(include_str!("../vst3-ids.ferritroject")).unwrap().vst3_class_id.unwrap();
        assert_eq!(written[..4], [0x3E, 0x6B, 0x2A, 0x90]);
        // On Windows the bytes are in COM order so that hosts show the GUID as written (2.0).
        let expected = if cfg!(windows) { ferritron_manifest::windows_tuid(&written) } else { written };
        assert_eq!(info.class_id, expected);
    }

    #[test]
    fn packaging_derives_the_same_class_id_as_the_plugin_does() {
        // Without `[vst3] class_id` the ID comes from `plugin.id`; moduleinfo.json is written
        // without running the plugin, so the two functions must agree.
        for id in ["dev.ferritron.examples.gain", "com.example.a", "x", ""] {
            assert_eq!(ferritron::__private::class_id(id), ferritron_manifest::class_id(id), "{id:?}");
        }
    }

    #[test]
    fn the_parameter_ids_of_1_3_0_map_to_the_fixed_ones() {
        use ferritron::__private::{param_id, reserved_id};
        // The pairs IRemapParamID answers (see `ferritron_vst3_options`).
        for key in ["ferritron.bypass", "ferritron.quality", "ferritron.mcp"] {
            assert_eq!(ferritron_manifest::legacy_reserved_id(key), param_id(key), "1.3.0 hashed the key");
            assert!(reserved_id(key) >= ferritron::__private::RESERVED_BASE);
        }
    }
}
