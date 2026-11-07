// Publishes the directory of `ferritron_abi.h` to format adapters as
// `DEP_FERRITRON_ABI_INCLUDE`.
fn main() {
    let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("include");
    println!("cargo::metadata=include={}", dir.display());
    println!("cargo::rerun-if-changed=include/ferritron_abi.h");
}
