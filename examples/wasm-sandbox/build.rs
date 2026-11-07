// Builds examples/wasm-crusher for wasm32-wasip1 and puts it in OUT_DIR,
// where src/lib.rs embeds it. Needs `rustup target add wasm32-wasip1`.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let guest = out.join("guest.wasm");
    for path in ["../wasm-crusher/src/lib.rs", "../wasm-crusher/wasm-crusher.ferritroject", "../wasm-crusher/Cargo.toml"] {
        println!("cargo::rerun-if-changed={path}");
    }
    // A separate target directory: the outer build holds the lock on the main one.
    let target_dir = out.join("wasm-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "--package", "wasm-crusher", "--target", "wasm32-wasip1", "--target-dir"])
        .arg(&target_dir)
        // The host calls the guest's functions through its function table.
        .env("CARGO_ENCODED_RUSTFLAGS", "-Clink-arg=--export-table")
        .env_remove("CARGO_TARGET_DIR")
        .status();
    let built = target_dir.join("wasm32-wasip1/release/wasm_crusher.wasm");
    match status {
        Ok(s) if s.success() => {
            std::fs::copy(&built, &guest).expect("copy the guest module");
        }
        _ => {
            println!("cargo::warning=wasm-sandbox: building the WebAssembly guest failed (is the wasm32-wasip1 target installed?)");
            std::fs::write(&guest, b"").expect("write an empty guest");
        }
    }
}
