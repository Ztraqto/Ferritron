//! Compiles Slang kernels for Ferritron plugins, from a build script
//! (design doc 9.11: write a kernel once, run it on the CPU and the GPU).
//!
//! A kernel is a Slang file with one function, called for every sample of
//! every channel:
//!
//! ```slang
//! float process(float x, KernelArgs args)
//! {
//!     float drive = args.values[0];
//!     return tanh(x * drive) / tanh(drive);
//! }
//! ```
//!
//! A kernel that remembers something between samples (a filter, an
//! envelope, a phase) takes a third argument, `inout KernelState state`: 16
//! values per channel, kept across blocks.
//!
//! [`compile`] turns it into WGSL for the GPU and C++ for the CPU (compiled
//! and linked into the plugin), and `ferritron_gpu::slang_kernel!` picks
//! both up (`slang_kernel!(name, stateful)` for stateful kernels). Like the plugin-format SDKs, Slang itself is not included: get
//! it from <https://github.com/shader-slang/slang/releases> and set
//! `SLANG_DIR` to its folder (or `SLANGC` to `slangc`, or put it on `PATH`).
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     ferritron_slang::compile("drive", "kernels/drive.slang").unwrap_or_else(|e| panic!("{e}"));
//! }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

/// The Slang declarations every kernel sees (`KernelArgs`).
const FERRITRON_SLANG: &str = include_str!("../slang/ferritron.slang");

/// Finds `slangc`: `SLANGC`, then `SLANG_DIR/bin`, then `PATH`.
pub fn slangc() -> Result<PathBuf, String> {
    println!("cargo::rerun-if-env-changed=SLANGC");
    println!("cargo::rerun-if-env-changed=SLANG_DIR");
    if let Some(path) = std::env::var_os("SLANGC") {
        return Ok(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("SLANG_DIR") {
        let exe = if cfg!(windows) { "slangc.exe" } else { "slangc" };
        return Ok(Path::new(&dir).join("bin").join(exe));
    }
    match Command::new("slangc").arg("-v").output() {
        Ok(_) => Ok(PathBuf::from("slangc")),
        Err(_) => Err("Slang was not found. Ferritron does not include the Slang compiler; please provide your own:\n  \
             1. Download a release from https://github.com/shader-slang/slang/releases\n  \
             2. Set SLANG_DIR to its folder (the one containing bin/slangc), e.g. in .cargo/config.toml:\n         \
             [env]\n         SLANG_DIR = \"C:/SDKs/slang\""
            .into()),
    }
}

/// Compiles the kernel in `source` under `name` (an identifier, unique in
/// the crate): `$OUT_DIR/<name>.wgsl` for the GPU, and a static library with
/// `ferritron_cpu_<name>` for the CPU, linked into the crate.
pub fn compile(name: &str, source: impl AsRef<Path>) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("kernel name `{name}` must be an identifier"));
    }
    let source = std::fs::canonicalize(source.as_ref()).map_err(|e| format!("{}: {e}", source.as_ref().display()))?;
    println!("cargo::rerun-if-changed={}", source.display());
    let slangc = slangc()?;
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set: call compile() from a build script")?);

    let module = out.join("ferritron.slang");
    std::fs::write(&module, FERRITRON_SLANG).map_err(|e| e.to_string())?;
    let harness = out.join(format!("{name}.harness.slang"));
    let text = std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let stateful = text.contains("KernelState");
    let harness_text = if stateful { stateful_harness(name, &module, &source) } else { harness_source(name, &module, &source) };
    std::fs::write(&harness, harness_text).map_err(|e| e.to_string())?;

    let wgsl = out.join(format!("{name}.wgsl"));
    run(&slangc, &[&harness, Path::new("-target"), Path::new("wgsl"), Path::new("-entry"), Path::new("ferritron_main"), Path::new("-stage"), Path::new("compute"), Path::new("-o"), &wgsl])?;
    let cpp = out.join(format!("{name}.cpp"));
    // The CPU build leaves the compute entry point out: its symbols would
    // clash between two kernels of the same crate.
    run(&slangc, &[&harness, Path::new("-target"), Path::new("cpp"), Path::new("-D"), Path::new("FERRITRON_CPU"), Path::new("-o"), &cpp])?;

    // `cargo check` for another OS: no C++ toolchain for it (see ferritron-vst3's build script).
    if std::env::var_os("FERRITRON_SKIP_NATIVE").is_some() {
        return Ok(());
    }
    let mut build = cc::Build::new();
    build.cpp(true).file(&cpp).warnings(false);
    if build.get_compiler().is_like_msvc() {
        build.flag("/std:c++17").flag("/EHsc");
    } else {
        build.flag("-std=c++17");
    }
    build.compile(&format!("ferritron_kernel_{name}"));
    Ok(())
}

/// What both harnesses start with: the kernel's sources, the parameter block
/// and the audio buffer.
fn harness_prelude(module: &Path, source: &Path) -> String {
    let path = |p: &Path| p.display().to_string().replace('\\', "/");
    format!(
        r#"#include "{module}"
#include "{source}"

struct FerritronParams
{{
    uint frames;
    uint channels;
    uint stride;
    uint pad;
    float4 values[2];
}};

ConstantBuffer<FerritronParams> ferritron_params;
RWStructuredBuffer<float> ferritron_data;
"#,
        module = path(module),
        source = path(source),
    )
}

/// Wraps the kernel for both targets: a compute entry point over a block of
/// channels, and an exported function for the CPU.
fn harness_source(name: &str, module: &Path, source: &Path) -> String {
    format!(
        r#"{prelude}
#ifndef FERRITRON_CPU
// One thread per sample; channel = y.
[shader("compute")]
[numthreads(256, 1, 1)]
void ferritron_main(uint3 id : SV_DispatchThreadID)
{{
    if (id.x >= ferritron_params.frames || id.y >= ferritron_params.channels)
        return;
    KernelArgs args;
    for (int k = 0; k < 8; k++)
        args.values[k] = ferritron_params.values[k / 4][k % 4];
    args.channel = id.y;
    args.frame = id.x;
    uint i = id.y * ferritron_params.stride + id.x;
    ferritron_data[i] = process(ferritron_data[i], args);
}}
#endif

#ifdef FERRITRON_CPU
export __extern_cpp float ferritron_cpu_{name}(float x, KernelArgs args)
{{
    return process(x, args);
}}
#endif
"#,
        prelude = harness_prelude(module, source),
    )
}

/// The harness of a stateful kernel: one GPU thread per channel, going
/// through the block in order with the channel's state; on the CPU, the
/// state is passed by pointer. The CPU symbol is named differently from a
/// stateless kernel's, so declaring the wrong kind fails to link.
fn stateful_harness(name: &str, module: &Path, source: &Path) -> String {
    format!(
        r#"{prelude}RWStructuredBuffer<KernelState> ferritron_state;

#ifndef FERRITRON_CPU
// One thread per channel; samples in order.
[shader("compute")]
[numthreads(64, 1, 1)]
void ferritron_main(uint3 id : SV_DispatchThreadID)
{{
    if (id.x >= ferritron_params.channels)
        return;
    KernelArgs args;
    for (int k = 0; k < 8; k++)
        args.values[k] = ferritron_params.values[k / 4][k % 4];
    args.channel = id.x;
    KernelState state = ferritron_state[id.x];
    for (uint f = 0; f < ferritron_params.frames; f++)
    {{
        args.frame = f;
        uint i = id.x * ferritron_params.stride + f;
        ferritron_data[i] = process(ferritron_data[i], args, state);
    }}
    ferritron_state[id.x] = state;
}}
#endif

#ifdef FERRITRON_CPU
export __extern_cpp float ferritron_cpu_stateful_{name}(float x, KernelArgs args, KernelState* state)
{{
    return process(x, args, *state);
}}
#endif
"#,
        prelude = harness_prelude(module, source),
    )
}

fn run(slangc: &Path, args: &[&Path]) -> Result<(), String> {
    let output = Command::new(slangc).args(args).output().map_err(|e| format!("cannot run {}: {e}", slangc.display()))?;
    if !output.status.success() {
        return Err(format!("slangc failed:\n{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}
