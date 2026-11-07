fn main() {
    println!("cargo::rustc-check-cfg=cfg(no_slang)");
    // A plugin would simply fail here without Slang; this example falls back
    // to the same formula in Rust so the workspace builds without it.
    // `smooth` (stateful) is used by the tests.
    if let Err(e) = ferritron_slang::compile("drive", "kernels/drive.slang").and_then(|_| ferritron_slang::compile("smooth", "kernels/smooth.slang")) {
        println!("cargo::warning=slang-drive builds without its Slang kernel: {}", e.lines().next().unwrap_or(""));
        println!("cargo::rustc-cfg=no_slang");
    }
}
