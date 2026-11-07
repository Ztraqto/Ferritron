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

pub fn generate(m: &Manifest, self_ty: &str, manifest_path: &str) -> String {
    let n = m.params.len();
    let mut s = String::new();
    let w = &mut s;

    // Rebuild the plugin whenever the project file changes.
    writeln!(w, "const _: &[u8] = ::core::include_bytes!({manifest_path:?});").unwrap();

    writeln!(w, "/// Parameters generated from the project file (`.ferritroject`).").unwrap();
    writeln!(w, "pub struct Params {{").unwrap();
    for p in &m.params {
        let ty = param_type(&p.kind);
        writeln!(w, "    pub {}: ::ferritron::{ty},", p.key).unwrap();
    }
    writeln!(w, "}}").unwrap();

    writeln!(w, "impl ::core::default::Default for Params {{ fn default() -> Self {{ Self {{").unwrap();
    for (i, p) in m.params.iter().enumerate() {
        let ty = param_type(&p.kind);
        writeln!(w, "    {}: ::ferritron::{ty}::new(__FERRITRON_PARAMS[{i}].default),", p.key).unwrap();
    }
    writeln!(w, "}} }} }}").unwrap();

    for (i, p) in m.params.iter().enumerate() {
        writeln!(w, "const __FERRITRON_ID_{i}: u32 = ::ferritron::__private::param_id({:?});", p.key).unwrap();
    }
    let mut ids: Vec<String> = (0..n).map(|i| format!("__FERRITRON_ID_{i}")).collect();
    ids.push("::ferritron::__private::BYPASS_ID".into());
    writeln!(w, "const _: () = ::ferritron::__private::assert_unique_ids(&[{}]);", ids.join(", ")).unwrap();

    writeln!(w, "static __FERRITRON_PARAMS: [::ferritron::ParamInfo; {n}] = [").unwrap();
    for (i, p) in m.params.iter().enumerate() {
        let kind = match &p.kind {
            Kind::Float { min, max } => format!("Float {{ min: {}, max: {} }}", f64_lit(*min), f64_lit(*max)),
            Kind::Int { min, max } => format!("Int {{ min: {min}, max: {max} }}"),
            Kind::Bool => "Bool".to_string(),
            Kind::Enum { choices } => format!("Enum {{ choices: &{choices:?} }}"),
        };
        writeln!(
            w,
            "    ::ferritron::ParamInfo {{ id: __FERRITRON_ID_{i}, key: {:?}, name: {:?}, unit: {:?}, kind: ::ferritron::ParamKind::{kind}, default: {} }},",
            p.key,
            p.name,
            p.unit,
            f64_lit(p.default)
        )
        .unwrap();
    }
    writeln!(w, "];").unwrap();

    writeln!(w, "impl ::ferritron::ParamSet for Params {{").unwrap();
    writeln!(w, "    fn infos() -> &'static [::ferritron::ParamInfo] {{ &__FERRITRON_PARAMS }}").unwrap();
    writeln!(w, "    fn cell(&self, index: usize) -> &::ferritron::ParamCell {{ match index {{").unwrap();
    for (i, p) in m.params.iter().enumerate() {
        writeln!(w, "        {i} => self.{}.cell(),", p.key).unwrap();
    }
    writeln!(w, "        _ => ::core::panic!(\"parameter index out of range\"),").unwrap();
    writeln!(w, "    }} }}").unwrap();
    writeln!(w, "    fn index_of(id: u32) -> ::core::option::Option<usize> {{ match id {{").unwrap();
    for i in 0..n {
        writeln!(w, "        __FERRITRON_ID_{i} => ::core::option::Option::Some({i}),").unwrap();
    }
    writeln!(w, "        _ => ::core::option::Option::None,").unwrap();
    writeln!(w, "    }} }}").unwrap();
    // Only parameters with options other than the default need a case.
    let special: Vec<(usize, &ferritron_manifest::Param)> = m.params.iter().enumerate().filter(|(_, p)| !p.automatable || p.hidden).collect();
    if !special.is_empty() {
        writeln!(w, "    fn options(index: usize) -> ::ferritron::ParamOptions {{ match index {{").unwrap();
        for (i, p) in special {
            writeln!(w, "        {i} => ::ferritron::ParamOptions {{ automatable: {}, hidden: {} }},", p.automatable, p.hidden).unwrap();
        }
        writeln!(w, "        _ => ::ferritron::ParamOptions::DEFAULT,").unwrap();
        writeln!(w, "    }} }}").unwrap();
    }
    writeln!(w, "}}").unwrap();

    writeln!(w, "/// Processing context of this plugin.").unwrap();
    writeln!(w, "pub type Context<'a> = ::ferritron::Context<'a, Params>;").unwrap();

    writeln!(
        w,
        "const __FERRITRON_INFO: ::ferritron::PluginInfo = ::ferritron::PluginInfo {{ \
         id: {id:?}, name: {name:?}, vendor: {vendor:?}, version: {version:?}, url: {url:?}, email: {email:?}, \
         kind: ::ferritron::PluginKind::{kind}, class_id: ::ferritron::__private::class_id({id:?}), \
         isolation: ::ferritron::isolation::Isolation::{isolation}, \
         process: ::ferritron::isolation::ProcessOptions {{ per_instance: {per_instance}, pipeline: {pipeline} }} }};",
        id = m.id,
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
}
