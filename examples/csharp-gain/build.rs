// Builds the C# plugin with NativeAOT into OUT_DIR/managed/CSharpGain.dll.
// Like the plugin SDKs, the .NET SDK is the developer's: without `dotnet`
// the crate still builds, but the plugin finds no companion and stays empty.
use std::process::Command;

fn main() {
    for file in ["plugin/Gain.cs", "plugin/CSharpGain.csproj", "../../bindings/csharp/Ferritron.cs"] {
        println!("cargo::rerun-if-changed={file}");
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("managed");
    let rid = match (std::env::var("CARGO_CFG_TARGET_OS").as_deref(), std::env::var("CARGO_CFG_TARGET_ARCH").as_deref()) {
        (Ok("windows"), Ok("aarch64")) => "win-arm64",
        (Ok("windows"), _) => "win-x64",
        (Ok("linux"), Ok("aarch64")) => "linux-arm64",
        (Ok("linux"), _) => "linux-x64",
        (Ok("macos"), Ok("aarch64")) => "osx-arm64",
        _ => "osx-x64",
    };
    let mut dotnet = Command::new("dotnet");
    dotnet.args(["publish", "plugin/CSharpGain.csproj", "-c", "Release", "-r", rid, "--nologo", "-v", "quiet", "-o"]).arg(&out);
    // NativeAOT finds the linker through `vswhere.exe`, which the Visual Studio installer keeps
    // outside PATH: without it the link step fails even with the C++ build tools installed.
    if rid.starts_with("win-")
        && let Some(installer) = std::env::var_os("ProgramFiles(x86)")
            .map(|p| std::path::PathBuf::from(p).join("Microsoft Visual Studio").join("Installer"))
            .filter(|d| d.join("vswhere.exe").is_file())
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(installer).chain(std::env::split_paths(&path))).unwrap_or(path);
        dotnet.env("PATH", joined);
    }
    let status = dotnet.status();
    match status {
        Ok(s) if s.success() => {}
        // Bundling then reports the missing companion; the workspace still builds.
        Ok(s) => println!("cargo::warning=csharp-gain: dotnet publish failed ({s}); NativeAOT needs the C++ build tools"),
        Err(e) => println!("cargo::warning=csharp-gain: .NET SDK not found ({e}); the plugin will be empty"),
    }
}
