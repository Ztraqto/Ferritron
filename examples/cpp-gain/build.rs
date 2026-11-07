// Compiles the C++ plugin; ferritron-core publishes the directory of
// ferritron_abi.h and ferritron.hpp.
fn main() {
    println!("cargo::rerun-if-env-changed=FERRITRON_SKIP_NATIVE");
    let include = std::env::var("DEP_FERRITRON_ABI_INCLUDE").expect("ferritron-core publishes its include directory");
    // `cargo check` for another OS (e.g. macOS from Windows) has no C/C++
    // toolchain for it: skip the native part, the Rust code is still checked.
    if std::env::var_os("FERRITRON_SKIP_NATIVE").is_some() {
        return;
    }
    let mut build = cc::Build::new();
    build.cpp(true).file("src/gain.cpp").include(&include);
    if build.get_compiler().is_like_msvc() {
        build.flag("/std:c++20").flag("/EHsc");
    } else {
        build.flag("-std=c++20");
    }
    build.compile("cpp_gain");
    println!("cargo::rerun-if-changed=src/gain.cpp");
    println!("cargo::rerun-if-changed={include}/ferritron.hpp");
    println!("cargo::rerun-if-changed={include}/ferritron_abi.h");
}
