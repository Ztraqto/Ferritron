//! Isolation Level 2: `examples/wasm-crusher`, compiled to WebAssembly and
//! embedded here, runs in a wasmtime sandbox inside the host (see
//! `ferritron-wasm`). The guest has no access to files, the environment or
//! the network, and a crash or a runaway loop inside it only silences it.

use ferritron_clap as _;
use ferritron_vst3 as _;

ferritron_wasm::wasm_plugin!(include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm")));
