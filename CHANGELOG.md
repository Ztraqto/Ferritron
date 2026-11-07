# Changelog

All notable changes to Ferritron. Versions follow [Semantic Versioning](https://semver.org/):
every crate of the workspace shares one version (`[workspace.package]` in
`Cargo.toml`). A plugin can pin it with
`ferritron = { path = "...", version = "1.1.0" }`, and read it at run time as
`ferritron::VERSION`.

## 1.1.0 — 2026-11-07

Dynamic performance and faster editors. Plugins built with 1.0.x build
unchanged; parameter IDs and saved state stay valid (state saved by 1.1.0
may carry the new Quality switch, which 1.0.x ignores).

### Added
- **Sleeping on silence**: once a plugin's input has been silent (below
  -120 dBFS) for as long as its output can go on, `process` is no longer
  called and the output is silence; input, notes and parameter changes wake
  it in the same block. `Process::tail() -> Tail` says how long the output
  goes on: `Tail::Auto` (default: input and output silent and no note held
  for two seconds), `Tail::Samples(n)` or `Tail::Infinite`.
  Hosts are told: VST3 silence flags and `getTailSamples` (it reported
  "no tail" before, which let some hosts cut reverb tails), CLAP
  `CLAP_PROCESS_SLEEP`, constant mask and the `clap.tail` extension.
  C ABI extension `ferritron.sleep.v1`.
- **Export quality**: `Quality` (`Normal`, `Best`) in `Context::quality`
  and `Setup::quality`. Best while the host renders offline (export, bounce),
  Normal otherwise; VST3 now passes the host's per-block process mode, so the
  quality follows within the block. `[quality]` in the project file adds a
  "Quality" switch (Auto / Normal / Best; 音質: 自動 / 標準 / 最高) the user
  sets by hand; saved with the project, not automatable.
- Web editors: `[view] background = "#rrggbb"` (painted until the page
  draws, no white flash) and `[view] preload` (default true).
  `ferritron.builtins` gives the page the bypass and Quality switches.
- `Harness::set_offline` and `Harness::sleeping` for tests.
- `ferritron-testhost`: `timing:` lines, `FERRITRON_TESTHOST_OPEN_AFTER`,
  `FERRITRON_TESTHOST_REOPEN`; `FERRITRON_TRACE=1` traces editor and GPU
  start-up in plugins.

### Changed
- **Open source**: Ferritron is dual-licensed under MIT or Apache-2.0
  (`LICENSE-MIT`, `LICENSE-APACHE`). The Ferritron Editor (with project
  locks) is closed source and distributed on the Releases page, alone or in
  a starter kit with Ferritron.
- New-project templates moved from the Editor to `ferritron-manifest`
  (`Template`, `new_project`), so `cargo ferritron new` no longer depends on
  the Editor. New projects find the framework through `FERRITRON_HOME`, or
  above the running program (a checkout, or a starter kit's `ferritron/`
  next to `bin/`), instead of a path fixed at build time.
- **Web editors open fast** (Windows): WebView2's browser process is started
  asynchronously 250 ms after the first instance is created (not for
  instances a host only creates to scan) and kept by a hidden
  web view while instances exist; all editors share one environment.
  Opening went from 300 ms or more (and seconds while a previous browser was
  still shutting down) to about 60–80 ms.
- WebView2 data is kept per host program (`%LOCALAPPDATA%\Ferritron\WebView2\<host>`):
  two programs with different WebView2 options sharing one folder made the
  second one fail to open any editor (`ERROR_INVALID_STATE`), for example
  two DAWs open at once.
- **Right click** in web editors no longer shows the browser's menu; pages
  still receive `contextmenu` for menus of their own
  (`ferritron.nativeContextMenu = true` restores it). Browser shortcut keys
  (reload, print, find) are off.
- **WebAssembly plugins** keep their compiled module on disk
  (`%LOCALAPPDATA%\Ferritron\wasm`, keyed by the module and wasmtime's
  settings): loading went from about 190 ms to about 2 ms.
  `FERRITRON_WASM_NO_CACHE` turns it off.
- **GPU processing** starts only Vulkan (Metal on Apple) first instead of
  every wgpu backend, falling back to the others without a hardware GPU;
  `WGPU_BACKEND` overrides. The first activation went from about 790 ms to
  about 400 ms.

## 1.0.1 — 2026-11-07

Refactoring only: no change to the plugin API, the project file format, the
C ABI, parameter IDs or saved state. Plugins built with 1.0.0 build unchanged.

### Added
- `ferritron::VERSION`: the Ferritron version a plugin is built with.
- `cargo ferritron --version`.
- New projects (`cargo ferritron new`, Ferritron Editor) record the Ferritron
  version they were made with in their `ferritron` dependency.
- `crates/ferritron-build`: helpers shared by the build scripts (finding the
  developer's SDKs, error messages, `FERRITRON_SKIP_NATIVE`).

### Changed
- GPU editors (`ferritron-gpu-view`): the drawing, input and resize logic
  that the Windows and macOS views had twice is now one `Canvas`.
- GPU processing (`ferritron-gpu`): the shared GPU threads of convolvers and
  real-time IIR banks use one generic batcher (`batch.rs`) instead of two copies.
- Isolation (`ferritron-core`): the Win32 declarations that `os.rs` and
  `shm.rs` each had are in one module (`isolation/win32.rs`), and starting a
  child through `rundll32.exe` is one function used by plugins and editors in
  their own process.
- VST3, CLAP, ARA and test host build scripts use `ferritron-build`. The test
  host now checks that `VST3_SDK_DIR` / `CLAP_SDK_DIR` contain the SDK.
- Smaller cleanups: one parameter-type mapping in the macro, one Slang
  harness prelude, a state helper in the ABI tests, `cargo_program()` in the CLI.

## 1.0.0

The first version: VST3 / CLAP / ARA 2 / Audio Unit (macOS, not yet run on a
Mac), web and GPU editors, isolation levels 1–3, hot reload, GPU processing,
node graphs, Slang kernels, C++ / C# plugins, MCP, Ferritron Editor and
project locks. See `docs/architecture.md` (design doc up to v1.36).
