// Builds examples/wasm-crusher for wasm32-wasip1 and puts it in OUT_DIR,
// where src/lib.rs embeds it. Needs `rustup target add wasm32-wasip1`.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let guest = out.join("guest.wasm");
    // The guest is built against Ferritron itself: rebuild it when the framework changes too
    // (a guest left from an older ABI makes the plugin fail to load).
    for path in [
        "../wasm-crusher/src/lib.rs",
        "../wasm-crusher/wasm-crusher.ferritroject",
        "../wasm-crusher/Cargo.toml",
        "../../crates/ferritron/src",
        "../../crates/ferritron-core/src",
        "../../crates/ferritron-core/include",
        "../../crates/ferritron-macros/src",
        "../../crates/ferritron-manifest/src",
        "../../Cargo.toml",
    ] {
        println!("cargo::rerun-if-changed={path}");
    }
    // A separate target directory: the outer build holds the lock on the main one.
    let target_dir = out.join("wasm-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let result = Command::new(cargo)
        .args(["build", "--release", "--package", "wasm-crusher", "--target", "wasm32-wasip1", "--target-dir"])
        .arg(&target_dir)
        // The host calls the guest's functions through its function table.
        .env("CARGO_ENCODED_RUSTFLAGS", "-Clink-arg=--export-table")
        .env_remove("CARGO_TARGET_DIR")
        .output();
    let built = target_dir.join("wasm32-wasip1/release/wasm_crusher.wasm");
    match result {
        Ok(output) if output.status.success() => {
            std::fs::copy(&built, &guest).expect("copy the guest module");
        }
        Ok(output) => {
            // Fail this build with the cargo error: an empty guest must never be embedded.
            let _ = std::fs::remove_file(&guest);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(60)..].join("\n");
            panic!(
                "wasm-sandbox: building the WebAssembly guest (examples/wasm-crusher, wasm32-wasip1) failed ({}). \
                 Is the wasm32-wasip1 target installed (`rustup target add wasm32-wasip1`)?\n--- cargo output (last lines) ---\n{tail}",
                output.status
            );
        }
        Err(e) => panic!("wasm-sandbox: could not start cargo for the WebAssembly guest: {e}"),
    }
}
