//! `cargo ferritron new <name>`: creates a plugin project from a template
//! (the same as the Ferritron Editor's, from `ferritron-manifest`).

use ferritron_manifest::{Template, new_project};
use std::path::Path;

pub fn run(name: &str, template: Template) -> Result<(), String> {
    new_project(Path::new("."), name, template, None, None)?;
    println!("Created {name}/. Next:");
    println!("  1. Set VST3_SDK_DIR / CLAP_SDK_DIR (see {name}/.cargo/config.toml)");
    println!("  2. cd {name} && cargo ferritron bundle {name}");
    Ok(())
}
