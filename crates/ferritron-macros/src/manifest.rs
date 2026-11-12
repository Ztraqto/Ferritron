//! Code generation from the project file (parsed by `ferritron-manifest`).

use std::fmt::Write;

pub use ferritron_manifest::{Kind, Manifest, parse};

fn f64_lit(v: f64) -> String {
    format!("{v:?}_f64")
}

/// The `ferritron` type of a parameter of `kind`.
fn param_type(kind: &Kind) -> &'static str {
    match kind {
        Kind::Float { .. } => "FloatParam",
        Kind::Int { .. } => "IntParam",
        Kind::Bool => "BoolParam",
        Kind::Enum { .. } => "EnumParam",
    }
}

/// A parameter key or group name as a Rust field name: Rust keywords (`type`, `mod`, ...) become raw identifiers.
fn ident(name: &str) -> String {
    const KEYWORDS: [&str; 45] = [
        "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
        "match", "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract",
        "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual",
    ];
    if KEYWORDS.contains(&name) { format!("r#{name}") } else { name.to_string() }
}

/// The `ferritron::display::Display` expression of a parameter's `display` / `format`.
fn display_expr(d: &ferritron_manifest::DisplaySpec, kind: &Kind) -> String {
    use ferritron_manifest::DisplayKind as K;
    let base = match &d.kind {
        K::Db => {
            // A range that reaches -60 dB or lower ends in "-INF dB" (a fader's bottom is off).
            let inf = match kind {
                Kind::Float { min, .. } if *min <= -60.0 => format!(".with_inf_at({})", f64_lit(*min)),
                _ => String::new(),
            };
            format!("::ferritron::display::Display::db(){inf}")
        }
        K::Hz => "::ferritron::display::Display::hz()".to_string(),
        K::Percent => "::ferritron::display::Display::percent()".to_string(),
        K::Ms => "::ferritron::display::Display::ms()".to_string(),
        K::Note => "::ferritron::display::Display::note()".to_string(),
        K::Ratio => "::ferritron::display::Display::ratio()".to_string(),
        K::Template(t) => format!("::ferritron::display::Display::template({t:?})"),
    };
    if d.digits > 0 { format!("{base}.with_digits({})", d.digits) } else { base }
}

/// `band` gives `BandParams`, `xy_pad` gives `XyPadParams`.
fn group_type(name: &str) -> String {
    let camel: String = name
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect();
    format!("{camel}Params")
}

/// What the VST3 shim needs beyond `PluginInfo`, as bytes (`ferritron_vst3_options`):
/// `version = 1`, flags (1 controller class, 2 compatibility class), controller ID, compatibility
/// class ID, the number of replaced class IDs and those (16 bytes each, in the order written),
/// then the number of parameter ID pairs and each pair `(old, new)` as little endian `u32`
/// (Ferritron 1.3.0 IDs of the built-in parameters and the IDs they have now: `IRemapParamID`).
pub fn vst3_options(m: &Manifest, windows: bool) -> Vec<u8> {
    // Explicit GUIDs are stored in COM order on Windows so hosts show them as written.
    let tuid = |b: [u8; 16]| if windows { ferritron_manifest::windows_tuid(&b) } else { b };
    let mut out = vec![1u8, 0];
    let controller = m.vst3_controller_id;
    let compat = m.vst3_compat.as_ref();
    out[1] = u8::from(controller.is_some()) | u8::from(compat.is_some()) << 1;
    out.extend_from_slice(&controller.map_or([0; 16], tuid));
    out.extend_from_slice(&compat.map_or([0; 16], |c| tuid(c.id)));
    let replaces = compat.map_or(&[][..], |c| &c.replaces[..]);
    out.push(replaces.len().min(255) as u8);
    for old in replaces.iter().take(255) {
        out.extend_from_slice(old);
    }
    let keys = ["ferritron.bypass", "ferritron.quality", "ferritron.mcp"];
    out.push(keys.len() as u8);
    for key in keys {
        out.extend_from_slice(&ferritron_manifest::legacy_reserved_id(key).to_le_bytes());
        out.extend_from_slice(&ferritron_manifest::reserved_id(key).to_le_bytes());
    }
    out
}

pub fn generate(m: &Manifest, self_ty: &str, manifest_path: &str) -> String {
    let n = m.params.len();
    let mut s = String::new();
    let w = &mut s;

    // Rebuild the plugin whenever the project file changes.
    writeln!(w, "const _: &[u8] = ::core::include_bytes!({manifest_path:?});").unwrap();

    // One struct per repeated group (`[params.band] repeat = 6` gives `BandParams`, used as `band: [BandParams; 6]`).
    for g in &m.groups {
        writeln!(w, "/// One copy of the `{}` parameter group.", g.name).unwrap();
        writeln!(w, "pub struct {} {{", group_type(&g.name)).unwrap();
        for (f, field) in g.fields.iter().enumerate() {
            writeln!(w, "    pub {}: ::ferritron::{},", ident(field), param_type(&m.params[g.first + f].kind)).unwrap();
        }
        writeln!(w, "}}").unwrap();
    }

    // `[streams]`: one writer per entry (`ctx.streams().<key>`), and the hub the instance and editors read through.
    if !m.streams.is_empty() {
        writeln!(w, "/// The streams of the project file's `[streams]`: `ctx.streams().<key>`.").unwrap();
        writeln!(w, "pub struct Streams {{").unwrap();
        for st in &m.streams {
            writeln!(w, "    pub {}: ::ferritron::{},", ident(&st.key), if st.tap { "TapWriter" } else { "StreamWriter" }).unwrap();
        }
        writeln!(w, "    __hub: ::ferritron::StreamHub,").unwrap();
        writeln!(w, "}}").unwrap();
        writeln!(w, "impl ::core::default::Default for Streams {{ fn default() -> Self {{").unwrap();
        for st in &m.streams {
            let ctor = if st.tap { "TapWriter" } else { "StreamWriter" };
            writeln!(w, "    let {} = ::ferritron::{ctor}::new({:?}, {});", ident(&st.key), st.key, st.len).unwrap();
        }
        let entries: Vec<String> = m.streams.iter().map(|st| format!("{}.entry()", ident(&st.key))).collect();
        writeln!(w, "    let __hub = ::ferritron::StreamHub::new(::std::vec![{}]);", entries.join(", ")).unwrap();
        let fields: Vec<String> = m.streams.iter().map(|st| ident(&st.key)).collect();
        writeln!(w, "    Self {{ {}, __hub }}", fields.join(", ")).unwrap();
        writeln!(w, "}} }}").unwrap();
    }

    writeln!(w, "/// Parameters generated from the project file (`.ferritroject`).").unwrap();
    writeln!(w, "pub struct Params {{").unwrap();
    for p in &m.params {
        match p.group {
            None => writeln!(w, "    pub {}: ::ferritron::{},", ident(&p.key), param_type(&p.kind)).unwrap(),
            Some(r) if r.instance == 0 && r.field == 0 => {
                let g = &m.groups[r.group];
                writeln!(w, "    pub {}: [{}; {}],", ident(&g.name), group_type(&g.name), g.repeat).unwrap()
            }
            Some(_) => {}
        }
    }
    if !m.streams.is_empty() {
        writeln!(w, "    __streams: Streams,").unwrap();
    }
    writeln!(w, "}}").unwrap();

    writeln!(w, "impl ::core::default::Default for Params {{ fn default() -> Self {{ Self {{").unwrap();
    if !m.streams.is_empty() {
        writeln!(w, "    __streams: ::core::default::Default::default(),").unwrap();
    }
    // A new instance starts at `initial` when the project file has one; hosts are still told `default`.
    let start_value = |i: usize, p: &ferritron_manifest::Param| match p.initial {
        Some(v) => f64_lit(v),
        None => format!("__FERRITRON_PARAMS[{i}].default"),
    };
    for (i, p) in m.params.iter().enumerate() {
        let ty = param_type(&p.kind);
        match p.group {
            None => writeln!(w, "    {}: ::ferritron::{ty}::new({}),", ident(&p.key), start_value(i, p)).unwrap(),
            Some(r) if r.instance == 0 && r.field == 0 => {
                let g = &m.groups[r.group];
                let nf = g.fields.len();
                writeln!(w, "    {}: ::core::array::from_fn(|__i| {} {{", ident(&g.name), group_type(&g.name)).unwrap();
                for (f, field) in g.fields.iter().enumerate() {
                    let q = &m.params[g.first + f];
                    let value = match q.initial {
                        Some(v) => f64_lit(v),
                        None => format!("__FERRITRON_PARAMS[{} + __i * {nf} + {f}].default", g.first),
                    };
                    writeln!(w, "        {}: ::ferritron::{}::new({value}),", ident(field), param_type(&q.kind)).unwrap();
                }
                writeln!(w, "    }}),").unwrap();
            }
            Some(_) => {}
        }
    }
    writeln!(w, "}} }} }}").unwrap();

    for (i, p) in m.params.iter().enumerate() {
        writeln!(w, "const __FERRITRON_ID_{i}: u32 = {};", p.id).unwrap();
    }
    let mut ids: Vec<String> = (0..n).map(|i| format!("__FERRITRON_ID_{i}")).collect();
    ids.push("::ferritron::__private::BYPASS_ID".into());
    writeln!(w, "const _: () = ::ferritron::__private::assert_unique_ids(&[{}]);", ids.join(", ")).unwrap();

    // Per-parameter hooks into the plugin's own display and mapping (`text = "custom"`, `scale = "custom"`).
    let plugin = format!("<{self_ty} as ::ferritron::Process>");
    let pref = |i: usize, p: &ferritron_manifest::Param| {
        format!("::ferritron::ParamRef {{ index: {i}, id: __FERRITRON_ID_{i}, key: {:?} }}", p.key)
    };
    writeln!(w, "static __FERRITRON_PARAMS: [::ferritron::ParamInfo; {n}] = [").unwrap();
    for (i, p) in m.params.iter().enumerate() {
        let kind = match &p.kind {
            Kind::Float { min, max } => format!("Float {{ min: {}, max: {} }}", f64_lit(*min), f64_lit(*max)),
            Kind::Int { min, max } => format!("Int {{ min: {min}, max: {max} }}"),
            Kind::Bool => "Bool".to_string(),
            Kind::Enum { choices } => format!("Enum {{ choices: &{choices:?} }}"),
        };
        let scale = match p.scale {
            ferritron_manifest::Scale::Linear => "::ferritron::ParamScale::Linear".to_string(),
            ferritron_manifest::Scale::Log => "::ferritron::ParamScale::Log".to_string(),
            ferritron_manifest::Scale::Skew { center } => format!("::ferritron::ParamScale::Skew {{ center: {} }}", f64_lit(center)),
            ferritron_manifest::Scale::Custom => format!(
                "::ferritron::ParamScale::Custom {{ normalize: |v| {plugin}::param_normalize({r}, v), denormalize: |v| {plugin}::param_denormalize({r}, v) }}",
                r = pref(i, p)
            ),
        };
        // A built-in display writes the unit itself, so hosts are told an empty one.
        let display_const = p.display.as_ref().map(|d| display_expr(d, &p.kind));
        let text = if let Some(d) = &display_const {
            format!(
                "::core::option::Option::Some(::ferritron::ParamTextFns {{ format: |v, out| {d}.format(v, out), parse: |t| {d}.parse(t) }})"
            )
        } else if p.custom_text {
            format!(
                "::core::option::Option::Some(::ferritron::ParamTextFns {{ format: |v, out| {plugin}::format_param_text({r}, v, out), parse: |t| {plugin}::parse_param_text({r}, t) }})",
                r = pref(i, p)
            )
        } else {
            "::core::option::Option::None".to_string()
        };
        writeln!(
            w,
            "    ::ferritron::ParamInfo {{ id: __FERRITRON_ID_{i}, key: {:?}, name: {:?}, unit: {:?}, kind: ::ferritron::ParamKind::{kind}, default: {}, scale: {scale}, text: {text} }},",
            p.key,
            p.name,
            if p.display.is_some() { "" } else { p.unit.as_str() },
            f64_lit(p.default)
        )
        .unwrap();
    }
    writeln!(w, "];").unwrap();

    // `[presets.<id>]`: factory presets, as (parameter index, plain value) lists.
    if !m.presets.is_empty() {
        writeln!(w, "static __FERRITRON_PRESETS: [::ferritron::Preset; {}] = [", m.presets.len()).unwrap();
        for pr in &m.presets {
            let values: Vec<String> = pr.values.iter().map(|(i, v)| format!("({i}, {})", f64_lit(*v))).collect();
            writeln!(w, "    ::ferritron::Preset {{ id: {:?}, name: {:?}, values: &[{}] }},", pr.id, pr.name, values.join(", ")).unwrap();
        }
        writeln!(w, "];").unwrap();
        // The built-in "Program" parameter: which factory preset is loaded (hosts' program lists).
        let names: Vec<String> = m.presets.iter().map(|pr| format!("{:?}", pr.name)).collect();
        writeln!(
            w,
            "static __FERRITRON_PROGRAM: ::ferritron::ParamInfo = ::ferritron::ParamInfo {{ id: ::ferritron::__private::PROGRAM_ID, key: \"ferritron.program\", name: \"Program\", unit: \"\", kind: ::ferritron::ParamKind::Enum {{ choices: &[{}] }}, default: 0.0, scale: ::ferritron::ParamScale::Linear, text: ::core::option::Option::None }};",
            names.join(", ")
        )
        .unwrap();
    }

    writeln!(w, "impl ::ferritron::ParamSet for Params {{").unwrap();
    writeln!(w, "    fn infos() -> &'static [::ferritron::ParamInfo] {{ &__FERRITRON_PARAMS }}").unwrap();
    if !m.streams.is_empty() {
        writeln!(w, "    fn stream_hub(&self) -> ::core::option::Option<&::ferritron::StreamHub> {{ ::core::option::Option::Some(&self.__streams.__hub) }}").unwrap();
    }
    writeln!(w, "    fn cell(&self, index: usize) -> &::ferritron::ParamCell {{ match index {{").unwrap();
    for (i, p) in m.params.iter().enumerate() {
        match p.group {
            None => writeln!(w, "        {i} => self.{}.cell(),", ident(&p.key)).unwrap(),
            Some(r) => writeln!(w, "        {i} => self.{}[{}].{}.cell(),", ident(&m.groups[r.group].name), r.instance, ident(&m.groups[r.group].fields[r.field])).unwrap(),
        }
    }
    writeln!(w, "        _ => ::core::panic!(\"parameter index out of range\"),").unwrap();
    writeln!(w, "    }} }}").unwrap();
    if n <= 32 {
        writeln!(w, "    fn index_of(id: u32) -> ::core::option::Option<usize> {{ match id {{").unwrap();
        for i in 0..n {
            writeln!(w, "        __FERRITRON_ID_{i} => ::core::option::Option::Some({i}),").unwrap();
        }
        writeln!(w, "        _ => ::core::option::Option::None,").unwrap();
        writeln!(w, "    }} }}").unwrap();
    } else {
        // Many parameters: a sorted table and a binary search, O(log n) in every build profile.
        let mut sorted: Vec<(u32, usize)> = m.params.iter().enumerate().map(|(i, p)| (p.id, i)).collect();
        sorted.sort();
        let table: Vec<String> = sorted.iter().map(|(id, i)| format!("({id}, {i})")).collect();
        writeln!(w, "    fn index_of(id: u32) -> ::core::option::Option<usize> {{").unwrap();
        writeln!(w, "        static SORTED: [(u32, u32); {n}] = [{}];", table.join(", ")).unwrap();
        writeln!(w, "        match SORTED.binary_search_by_key(&id, |e| e.0) {{ ::core::result::Result::Ok(p) => ::core::option::Option::Some(SORTED[p].1 as usize), ::core::result::Result::Err(_) => ::core::option::Option::None }}").unwrap();
        writeln!(w, "    }}").unwrap();
    }
    // Only parameters with options other than the default need a case.
    let special: Vec<(usize, &ferritron_manifest::Param)> = m.params.iter().enumerate().filter(|(_, p)| !p.automatable || p.hidden || p.poly).collect();
    if !special.is_empty() {
        writeln!(w, "    fn options(index: usize) -> ::ferritron::ParamOptions {{ match index {{").unwrap();
        for (i, p) in special {
            writeln!(w, "        {i} => ::ferritron::ParamOptions {{ automatable: {}, hidden: {}, poly: {} }},", p.automatable, p.hidden, p.poly).unwrap();
        }
        writeln!(w, "        _ => ::ferritron::ParamOptions::DEFAULT,").unwrap();
        writeln!(w, "    }} }}").unwrap();
    }
    let smoothed: Vec<(usize, ferritron_manifest::Smooth)> = m.params.iter().enumerate().filter_map(|(i, p)| p.smooth.map(|s| (i, s))).collect();
    if !smoothed.is_empty() {
        writeln!(w, "    fn smoothing(index: usize) -> ::core::option::Option<::ferritron::Smoothing> {{ match index {{").unwrap();
        for (i, s) in smoothed {
            writeln!(w, "        {i} => ::core::option::Option::Some(::ferritron::Smoothing {{ seconds: {:?}, kind: {} }}),", s.seconds, s.kind).unwrap();
        }
        writeln!(w, "        _ => ::core::option::Option::None,").unwrap();
        writeln!(w, "    }} }}").unwrap();
    }
    writeln!(w, "}}").unwrap();

    if m.streams.is_empty() {
        writeln!(w, "impl ::ferritron::HasStreams for Params {{ type Streams = ::ferritron::NoStreams; fn streams(&self) -> &::ferritron::NoStreams {{ &::ferritron::NoStreams }} }}").unwrap();
    } else {
        writeln!(w, "impl ::ferritron::HasStreams for Params {{ type Streams = Streams; fn streams(&self) -> &Streams {{ &self.__streams }} }}").unwrap();
    }
    writeln!(w, "/// Processing context of this plugin.").unwrap();
    writeln!(w, "pub type Context<'a> = ::ferritron::Context<'a, Params>;").unwrap();

    writeln!(
        w,
        "const __FERRITRON_INFO: ::ferritron::PluginInfo = ::ferritron::PluginInfo {{ \
         id: {id:?}, name: {name:?}, vendor: {vendor:?}, version: {version:?}, url: {url:?}, email: {email:?}, \
         kind: ::ferritron::PluginKind::{kind}, class_id: {class_id}, vst3_subcategories: {vst3_sub:?}, clap_features: {clap_features:?}, vst2_id: {vst2_id}, audio: ::ferritron::AudioConfig {{ min_channels: {min_ch}, max_channels: {max_ch}, sidechain: {sidechain}, midi_input: {midi_input}, passthrough: {passthrough}, host_cc: {host_cc} }}, \
         isolation: ::ferritron::isolation::Isolation::{isolation}, \
         process: ::ferritron::isolation::ProcessOptions {{ per_instance: {per_instance}, pipeline: {pipeline} }} }};",
        id = m.id,
        vst2_id = m.vst2_id.unwrap_or(0),
        min_ch = m.audio.min_channels,
        max_ch = m.audio.max_channels,
        sidechain = match m.audio.sidechain {
            Some(n) => format!("::core::option::Option::Some({n})"),
            None => "::core::option::Option::None".to_string(),
        },
        midi_input = m.midi_input,
        passthrough = m.audio.passthrough,
        host_cc = m.audio.host_cc,
        class_id = match m.vst3_class_id {
            // An explicit GUID: as written, but in COM order on Windows so hosts show it as written.
            Some(b) => format!("{{ #[cfg(windows)] {{ {:?} }} #[cfg(not(windows))] {{ {b:?} }} }}", ferritron_manifest::windows_tuid(&b)),
            None => format!("::ferritron::__private::class_id({:?})", m.id),
        },
        vst3_sub = if m.vst3_subcategories.is_some() || !m.categories.is_empty() { m.vst3_sub_categories() } else { String::new() },
        clap_features = if m.clap_features.is_some() || !m.categories.is_empty() { m.clap_feature_list().join(",") } else { String::new() },
        name = m.name,
        vendor = m.vendor,
        version = m.version,
        url = m.url,
        email = m.email,
        kind = if m.instrument { "Instrument" } else { "Effect" },
        isolation = if m.isolate { "Process" } else { "Guarded" },
        per_instance = m.isolate_per_instance,
        pipeline = m.isolate_pipeline,
    )
    .unwrap();
    writeln!(w, "::ferritron::__export_formats!({self_ty});").unwrap();
    // The VST3 shim reads these (controller class, compatibility, parameter remapping) by name.
    for (cfg, windows) in [("windows", true), ("not(windows)", false)] {
        let blob = vst3_options(m, windows);
        let bytes: Vec<String> = blob.iter().map(u8::to_string).collect();
        writeln!(w, "#[cfg({cfg})]").unwrap();
        writeln!(w, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(w, "#[unsafe(no_mangle)]").unwrap();
        writeln!(w, "pub static ferritron_vst3_options: [u8; {}] = [{}];", blob.len(), bytes.join(", ")).unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_code_is_valid_rust_tokens() {
        let src = "[plugin]
id = \"com.example.gain\"
name = \"Gain\"
[params]
gain = { type = \"float\", range = [-60, 12], default = 0, unit = \"dB\" }";
        let code = generate(&parse(src).unwrap(), "Gain", "C:/x/gain.ferritroject");
        assert!(code.contains("pub gain: ::ferritron::FloatParam"));
        assert!(code.contains("__export_formats!(Gain)"));
    }

    #[test]
    fn streams_become_writers_in_a_streams_struct() {
        let src = "[plugin]
id = \"com.example.an\"
name = \"An\"
[streams]
spectrum = { len = 2048 }
wave = { len = 4096, kind = \"tap\" }";
        let code = generate(&parse(src).unwrap(), "An", "C:/x/an.ferritroject");
        assert!(code.contains("pub spectrum: ::ferritron::StreamWriter,"));
        assert!(code.contains("pub wave: ::ferritron::TapWriter,"));
        assert!(code.contains("::ferritron::StreamWriter::new(\"spectrum\", 2048)"));
        assert!(code.contains("type Streams = Streams;"));
        let plain = generate(&parse("[plugin]
id = \"a.b\"
name = \"X\"").unwrap(), "X", "C:/x/x.ferritroject");
        assert!(plain.contains("type Streams = ::ferritron::NoStreams;"));
        assert!(!plain.contains("pub struct Streams"));
    }

    #[test]
    fn displays_and_presets_are_generated() {
        let src = "[plugin]
id = \"com.example.d\"
name = \"D\"
[params]
cut = { type = \"float\", range = [20, 20000], unit = \"Hz\", display = \"hz\" }
lvl = { type = \"float\", range = [-90, 6], display = \"db\", digits = 2 }
plain = { type = \"float\", range = [0, 1], unit = \"x\" }
[presets.warm]
name = \"Warm\"
cut = 800";
        let code = generate(&parse(src).unwrap(), "D", "C:/x/d.ferritroject");
        assert!(code.contains("::ferritron::display::Display::hz().format(v, out)"));
        assert!(code.contains("Display::db().with_inf_at(-90.0_f64).with_digits(2)"), "{code}");
        assert!(code.contains("unit: \"\", kind: ::ferritron::ParamKind::Float { min: 20.0_f64"), "a display embeds the unit: hosts get none");
        assert!(code.contains("unit: \"x\""), "parameters without a display keep their unit");
        assert!(code.contains("::ferritron::Preset { id: \"warm\", name: \"Warm\", values: &[(0, 800.0_f64)] }"));
    }

    #[test]
    fn repeated_groups_become_arrays_of_structs() {
        let src = "[plugin]
id = \"com.example.eq\"
name = \"Eq\"
[params]
master = { type = \"float\", range = [-12, 12] }
[params.band]
repeat = 4
[params.band.fields]
freq = { type = \"float\", range = [20, 20000], initial = 500 }
on = { type = \"bool\" }";
        let code = generate(&parse(src).unwrap(), "Eq", "C:/x/eq.ferritroject");
        assert!(code.contains("pub struct BandParams {"));
        assert!(code.contains("pub freq: ::ferritron::FloatParam,"));
        assert!(code.contains("pub band: [BandParams; 4],"));
        assert!(code.contains("4 => self.band[1].on.cell(),"));
        assert!(code.contains("__FERRITRON_PARAMS[1 + __i * 2 + 1].default"));
        assert_eq!(code.matches("pub master:").count(), 1);
    }
    #[test]
    fn vst3_options_carry_the_controller_compatibility_and_remap_pairs() {
        let plain = vst3_options(&parse("[plugin]\nid = \"a.b\"\nname = \"X\"").unwrap(), false);
        assert_eq!(plain[..2], [1, 0]);
        assert_eq!(plain.len(), 2 + 16 + 16 + 1 + 1 + 3 * 8, "no replaced classes, three parameter pairs");
        let src = "[plugin]\nid = \"a.b\"\nname = \"X\"\n[vst3]\ncontroller_id = \"A1C4F7E2-6B38-4C90-8F2D-5E7B3A91C640\"\ncompatibility = { id = \"0D9E5B70-2A46-4F83-B1C7-8E4A6F2D9035\", replaces = [\"11111111-2222-3333-4444-555555555555\"] }";
        let blob = vst3_options(&parse(src).unwrap(), false);
        assert_eq!(blob[..2], [1, 3]);
        assert_eq!(blob[2..6], [0xA1, 0xC4, 0xF7, 0xE2]);
        assert_eq!(blob[18..22], [0x0D, 0x9E, 0x5B, 0x70]);
        assert_eq!(blob[34], 1);
        assert_eq!(blob[35..39], [0x11, 0x11, 0x11, 0x11]);
        let pairs = &blob[51..];
        assert_eq!(pairs[0], 3);
        let old = u32::from_le_bytes(pairs[1..5].try_into().unwrap());
        let new = u32::from_le_bytes(pairs[5..9].try_into().unwrap());
        assert_eq!((old, new), (ferritron_manifest::param_id("ferritron.bypass"), 0x4000_0000));
        // On Windows an explicit GUID is stored in COM order so that hosts show it as written.
        let win = vst3_options(&parse(src).unwrap(), true);
        assert_eq!(win[2..6], [0xE2, 0xF7, 0xC4, 0xA1], "controller: first field reversed");
        assert_eq!(win[6..8], [0x38, 0x6B], "second field reversed");
        assert_eq!(win[8..10], [0x90, 0x4C], "third field reversed");
        assert_eq!(win[10..18], blob[10..18], "the rest as written");
        assert_eq!(win[35..39], blob[35..39], "replaced classes stay as written (they are old moduleinfo strings)");
        let code = generate(&parse(src).unwrap(), "Gain", "C:/x/gain.ferritroject");
        assert!(code.contains("pub static ferritron_vst3_options: [u8; "));
        assert!(code.contains("#[cfg(windows)]"), "both layouts are emitted");
    }
}
