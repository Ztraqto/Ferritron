//! Repeated parameter groups (P2): 24 EQ bands, 50 slots and 6 pads declared in
//! `eq-bands.ferritroject` with `[params.<group>] repeat = N` and read as arrays.
//!
//! The plugin itself is deliberately small: the output is scaled by the master gain and by
//! the summed gain of the bands that are switched on (a stand-in for a real filter bank).

use ferritron::prelude::*;

#[derive(Default)]
struct EqBands;

#[ferritron::plugin]
impl Process for EqBands {
    fn process(&mut self, ctx: &mut Context) {
        let p = ctx.params();
        let mut db = p.master.value();
        for band in &p.band {
            if band.on.value() {
                db += band.gain.value();
            }
        }
        let gain = db_to_gain(db);
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::ParamSet;
    use ferritron::testing::Harness;

    #[test]
    fn the_groups_expand_to_951_parameters_with_the_documented_keys() {
        let infos = <Params as ParamSet>::infos();
        assert_eq!(infos.len(), 1 + 24 * 5 + 50 * 16 + 6 * 5);
        assert_eq!(infos.len(), 951);
        // declaration order: master, then band 1 .. 24 (field by field), slot 1 .. 50, pad 1 .. 6
        assert_eq!(infos[0].key, "master");
        assert_eq!(infos[1].key, "band_1_freq");
        assert_eq!(infos[5].key, "band_1_on");
        assert_eq!(infos[6].key, "band_2_freq");
        assert_eq!(infos[121].key, "slot_1_a");
        assert_eq!(infos[920].key, "slot_50_p");
        assert_eq!(infos[950].key, "pad_6_lock");
        assert_eq!(infos[1].name, "Band 1 Freq");
        assert_eq!(infos[1].unit, "Hz");
        let mut keys: Vec<&str> = infos.iter().map(|i| i.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 951, "keys are unique");
    }

    #[test]
    fn ids_follow_the_formula_and_are_stable() {
        let infos = <Params as ParamSet>::infos();
        let id = |key: &str| infos.iter().find(|i| i.key == key).unwrap().id;
        assert_eq!(id("master"), 1);
        assert_eq!(id("band_1_freq"), 2002);
        assert_eq!(id("band_1_on"), 2006);
        assert_eq!(id("band_24_gain"), 2000 + 23 * 64 + 3);
        assert_eq!(id("slot_1_a"), 4000);
        assert_eq!(id("slot_3_c"), 4000 + 2 * 16 + 2);
        // `pad` has no id_base: the ID is the hash of the key, as for a plain parameter
        assert_eq!(id("pad_2_x"), ferritron::__private::param_id("pad_2_x"));
        // IDs and indexes map both ways
        for (i, info) in infos.iter().enumerate() {
            assert_eq!(<Params as ParamSet>::index_of(info.id), Some(i), "{}", info.key);
        }
        assert_eq!(<Params as ParamSet>::index_of(3), None);
    }

    #[test]
    fn index_of_is_fast_for_951_parameters() {
        let infos = <Params as ParamSet>::infos();
        let t = std::time::Instant::now();
        let mut sum = 0usize;
        for _ in 0..1000 {
            for info in infos {
                sum += <Params as ParamSet>::index_of(std::hint::black_box(info.id)).unwrap();
            }
        }
        std::hint::black_box(sum);
        // 951 000 lookups; a linear scan would take about 450 million comparisons
        println!("index_of: {:?} per lookup", t.elapsed() / 951_000);
        assert!(t.elapsed().as_millis() < 3000, "{:?}", t.elapsed());
    }

    #[test]
    fn band_arrays_and_index_access_agree() {
        let mut h = Harness::<EqBands>::new(48_000.0, 256);
        h.set_param("band_3_gain", 6.0);
        h.set_param("band_24_freq", 8000.0);
        h.set_param("band_2_on", 0.0);
        h.set_param("pad_6_size", 5.0);
        let params = h.params();
        // `gain` and `freq` are smoothed: value() glides while blocks run, target() is the host's value
        assert_eq!(params.band[2].gain.target(), 6.0);
        assert_eq!(params.band[23].freq.target(), 8000.0);
        assert!(!params.band[1].on.value());
        assert!(params.band[0].on.value(), "default = true");
        assert_eq!(params.band[0].freq.target(), 1000.0, "default of every copy");
        assert_eq!(params.band[0].q.target(), 1.0);
        assert_eq!(params.pad[5].size.value(), 5);
        assert_eq!(params.pad[0].mode.index(), 0);
        assert_eq!(params.slot[0].a.value(), 0.0);
        // cell(index) reaches the same storage as the array
        let infos = <Params as ParamSet>::infos();
        let index = infos.iter().position(|i| i.key == "band_3_gain").unwrap();
        assert_eq!(params.cell(index).get(), 6.0);
        let index = infos.iter().position(|i| i.key == "band_24_freq").unwrap();
        assert!((params.cell(index).get() - 8000.0).abs() < 1e-6, "set_param goes through the log scale");
        assert!(std::ptr::eq(params.cell(index), params.band[23].freq.cell()));
        assert_eq!(h.param("band_3_gain"), 6.0);
    }

    #[test]
    fn the_plugin_reads_the_bands() {
        let mut h = Harness::<EqBands>::new(48_000.0, 256);
        h.set_param("band_1_gain", 6.0);
        h.set_param("band_2_gain", 6.0);
        h.set_param("band_2_on", 0.0);
        // Smoothing (gain: 20 ms) glides to the values; run long enough to arrive.
        let mut buf = [0.5f32; 256];
        for _ in 0..20 {
            buf.fill(0.5);
            assert!(h.process(&mut [&mut buf]));
        }
        let want = 0.5 * 10f32.powf(6.0 / 20.0);
        assert!((buf[255] - want).abs() < 1e-3, "{} vs {want}", buf[255]);
    }

    #[test]
    fn state_round_trips_group_values() {
        let mut h = Harness::<EqBands>::new(48_000.0, 256);
        h.set_param("slot_50_p", -0.25);
        h.set_param("band_24_type", 3.0);
        let saved = h.save_state();
        let mut other = Harness::<EqBands>::new(48_000.0, 256);
        assert!(other.load_state(&saved));
        assert_eq!(other.param("slot_50_p"), -0.25);
        assert_eq!(other.params().band[23].r#type.index(), 3);
    }

    #[test]
    fn host_sees_every_parameter_with_its_own_id_and_text() {
        let h = Harness::<EqBands>::new(48_000.0, 256);
        let hidden = (0..h.host_param_count()).filter_map(|i| h.host_param(i)).count();
        assert!(hidden >= 900, "the host lists the expanded parameters ({hidden})");
        assert_eq!(h.text("band_1_freq"), "1000.00");
    }
}
