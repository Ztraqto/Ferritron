//! A Ferritron plugin written in C++ (`src/gain.cpp`, with `ferritron.hpp`).
//! This crate only links the C++ code with the format adapters: they find
//! the plugin through `ferritron_plugin_entry`, which the C++ code exports.

use ferritron_clap as _;
use ferritron_vst3 as _;
