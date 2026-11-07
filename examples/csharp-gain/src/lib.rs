//! A Ferritron plugin written in C# (`plugin/Gain.cs`, with
//! `bindings/csharp/Ferritron.cs`), built with NativeAOT into a native
//! library. This crate is the plugin binary: the format adapters, and an
//! entry point that loads the C# library from the plugin's folder.

use ferritron_clap as _;
use ferritron_vst3 as _;

#[cfg(windows)]
ferritron_core::companion_plugin!("CSharpGain.dll");
#[cfg(target_os = "linux")]
ferritron_core::companion_plugin!("CSharpGain.so");
#[cfg(target_os = "macos")]
ferritron_core::companion_plugin!("CSharpGain.dylib");
