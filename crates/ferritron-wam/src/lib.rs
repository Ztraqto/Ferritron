//! Web Audio Module (WAM 2.0) format.
//!
//! A plugin built for WebAssembly (`cargo ferritron bundle <package>
//! --target web`) becomes `<Name>.wam/`, a folder any web server can host:
//!
//! | File | |
//! |---|---|
//! | `index.js` | the WAM: `export default` class hosts load with `import()` |
//! | `processor.js`, `guest.js` | the AudioWorklet that runs the plugin |
//! | `plugin.wasm` | the plugin (`wasm32-wasip1`, functions in its exported table) |
//! | `descriptor.json` | the WAM descriptor, from the project file |
//! | `gui/` | the plugin's web editor (`[view] backend = "web"`), if it has one |
//! | `index.html` | a page to try the plugin in a browser |
//!
//! The plugin keeps every parameter key, its state format and its
//! translations: the browser side calls the same C ABI as the native
//! formats (through `ferritron.describe.v1` for keys and names).
//!
//! This crate has no code: the WebAssembly build of every Ferritron plugin
//! already exports what the browser side calls (`ferritron_plugin_entry`,
//! `ferritron_wasm_alloc`). It only carries the files above.
