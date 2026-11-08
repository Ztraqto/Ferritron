# Changelog

All notable changes to Ferritron. Versions follow [Semantic Versioning](https://semver.org/):
every crate of the workspace shares one version (`[workspace.package]` in
`Cargo.toml`). A plugin can pin it with
`ferritron = { path = "...", version = "1.3.0" }`, and read it at run time as
`ferritron::VERSION`.

## 1.3.0 — 2026-11-08

Every platform: LV2, AUv3, AAX, standalone apps, iPhone and iPad, Android,
and the web (WAM 2.0), from the same plugin. Plugins built with 1.2 build
unchanged; parameter IDs and saved state stay valid. New default features
(`lv2`, `standalone`, `auv3`) add formats; `default-features = false` keeps
a plugin to the ones it lists.

### Added
- **LV2** (`ferritron-lv2`, feature `lv2`, default; Windows, macOS, Linux):
  written in Rust, no SDK. A control port per parameter (plain values,
  toggles, enumerations with their choices, units), MIDI for instruments
  (sample-accurate), state (one `atom:Chunk`), latency, freewheel (export
  quality), blocks of any length. The editor embeds through `ui:parent`
  (Windows, macOS), with `instance-access` for `ferritron.instance`. The
  Turtle files are generated from the project file. Plugin URI:
  `urn:ferritron:<plugin id>`.
- **Standalone apps** (`ferritron-standalone`, feature `standalone`,
  default): the plugin on its own with the computer's audio and MIDI
  (Windows: WASAPI and midir; macOS: Core Audio and Core MIDI; Linux: ALSA,
  no editor yet), its editor in the app's window, its state kept between
  runs. The app's executable is a launcher `cargo ferritron` compiles.
- **Android** (`--target android` / `aarch64-linux-android` /
  `x86_64-linux-android`): an APK built with the Android SDK's tools and a
  JDK (no Gradle): AAudio, `android.media.midi`, the plugin's HTML/JS editor
  in a WebView with the same `window.ferritron`, or an editor made from the
  parameters (with a keyboard for instruments). Signed with the debug key,
  or a release key (`FERRITRON_ANDROID_KEYSTORE`). Needs the Android NDK
  (new SDK id `ndk`, `ANDROID_NDK_HOME`).
- **AUv3** (`ferritron-auv3`, feature `auv3`, default; macOS, iPhone, iPad):
  an app extension inside the standalone app. It registers the AU v2 code
  in the extension and hands hosts an `AUAudioUnitV2Bridge` of it; on macOS
  the view is the plugin's editor. **iPhone / iPad** (`--target ios`): the
  app (RemoteIO, Core MIDI) with the AUv3; needs a Mac with Xcode.
- **AAX** (`ferritron-aax`, feature `aax`, off by default): a C++ shim on
  `AAX_CMonolithicParameters`, compiled with the developer's AAX SDK
  (new SDK id `aax`, `AAX_SDK_DIR`): parameters with the plugin's own text,
  Master Bypass, state as one chunk, MIDI for instruments, latency, the
  plugin's editor. Without the SDK the crate builds empty with a warning.
- **WAM 2.0** (`ferritron-wam`, `--target web`): the plugin as WebAssembly
  in an AudioWorklet, as a Web Audio Module any WAM 2.0 host loads, with
  sample-accurate automation and MIDI, state, latency, parameter info with
  keys and localized names, the plugin's web editor (or one made from the
  parameters) and a page to try it (`index.html`).
- C ABI: `ferritron.describe.v1`, the plugin as JSON (parameter keys,
  kinds, ranges, localized names and choices, editor messages).
- `cargo ferritron bundle --target web | android | ios` (and the full
  triples); format metadata keys `files` (templates with lists and
  sections), `assets`, `ui`, `launcher` / `launcher-kind`, `package` and
  `symbol-values`; templates get `{file}`, `{binary.uri}`, `{id.package}`.
- `ferritron_manifest`: `Manifest::host_params`, `template_scope`,
  `render`, `param_id` (the same as the core's, pinned by a test),
  `java_package`; `Manifest::view`.
- `ferritron_au::factory` (and `editor_view` on macOS) for adapters built on
  the AU v2; `ferritron-au` also builds for iOS (without the Cocoa view).

### Fixed
- WebAssembly: creating an instance no longer panics (no process ids there),
  which also broke isolation Level 2 guests built with 1.2.
- Android builds of the core (no POSIX shared memory: process isolation is
  not offered there).
- Mach-O bundles: nested code (frameworks, app extensions with their
  entitlements, the helper) is signed before the bundle.

### Not verified
- macOS and iOS (standalone, AUv3, and the AU v2 changes) are type-checked
  only; AAX was written without the SDK and is not compiled; LV2 was checked
  with a test host, not yet in a DAW; Android on the emulator, not on a phone.

## 1.2.0 — 2026-11-07

Instances that know themselves, their track and each other. Plugins built
with 1.1.x build unchanged; parameter IDs stay valid. State saved by 1.2
carries the instance ID and the store after the parameter entries; 1.1
reads such state and ignores both. 1.2 reads state of every earlier version.

### Added
- **Instance handles** (`ferritron::InstanceHandle`, `Send + Sync`, cheap to
  clone): the plugin gets its own in `Process::init` (new, called once
  after creation) and `Context::instance`, editors through `Ui::instance`
  (GPU) and `window.ferritron.instance` (web), any code through
  `ferritron::instances::<P>()` (the live instances of a plugin in the
  process) and `instances_revision()`. A handle:
  - `id()`: an `InstanceId` saved with the project, new for a duplicated
    instance (same state loaded while the original lives);
  - `track()`: `TrackInfo { name, color, index }` from the host (VST3
    `IInfoListener`, CLAP `track-info`), now for every plugin, not only
    with `[mcp]`;
  - `value` / `normalized` and `begin` / `set` / `end` / `edit`: change a
    parameter from any thread (not the audio thread) and tell the host as a
    gesture, so DAWs record automation on the right track;
  - `wake()`: a sleeping instance processes its next block (CLAP:
    `request_process`); edits through a handle and loaded state wake too;
  - `store()`: a key-value store (text or bytes) saved with the project;
  - `on_message` / `post`: JSON messages with the web editor.
- Web editors: `ferritron.send(obj)`, `ferritron.onMessage(fn)`,
  `ferritron.instance` (`{ id, track }`) and `ferritron.onInstanceChange(fn)`.
- GPU editors: `Input::Leave`; `Ui::set_cursor(Cursor::…)`;
  `Ui::redraw_handle()` (`RedrawHandle::request()` from any thread) and
  `Ui::request_redraw()`; editors also redraw when their instance (track,
  store) or the list of instances changes; `Ui::track()`, `Ui::track_name()`,
  `Ui::instance_id()`; `Ui::for_tests::<P>(w, h)` for unit tests of editor
  logic without host or GPU.
- Parameters: `automatable = false` and `hidden = true` in the project file
  (`ParamOptions`, `ParamSet::options`). VST3: no `kCanAutomate` /
  `kIsHidden`; CLAP: no `CLAP_PARAM_IS_AUTOMATABLE` / `CLAP_PARAM_IS_HIDDEN`.
  The Ferritron Editor's CLI takes `--hidden` and `--not-automatable`.
- Tests: `Harness::params()` (typed), `Harness::instance()`,
  `Harness::set_track()`, `Harness::send_message()`.
- `ferritron-testhost`: screenshots as PNG (`--bmp` for BMP);
  `--instances N` also with `--gui` (each instance told its own track);
  mouse scripts `--drag X0 Y0 X1 Y1`, `--move X Y`, `--wheel X Y N`, `--leave`.
- `cargo ferritron bundle` warns when the tool and the plugin's Ferritron
  versions differ.
- C ABI: `ferritron.host.v2` (`ferritron_host_callbacks2`: edits as
  gestures, `request_process`), `ferritron.track.v1` (`ferritron_track_info`),
  `FERRITRON_PARAM_HIDDEN`. The callbacks' `ctx` is the same pointer as the
  editor's `ferritron_view_host::ctx`, which is how editors find their instance.
- Example `examples/instances`: every instance with its track, levels set
  from any instance's editor, a note per track in the store.
- **SDK management**: one rule finds every SDK (`ferritron_build::sdk`): a
  version pinned in the project file (`[sdk] vst3 = "3.8.1"`), else `source`
  in `sdk.toml` (`auto`, `env`, `managed`, `path`), else the build stops and
  lists where it looked. Ferritron's SDK folder holds several versions side
  by side (`C:\Ferritron\SDK` or `~/Ferritron/SDK`, `FERRITRON_SDK_ROOT`).
  `cargo ferritron sdk status | list | available | fetch | use | source |
  remove`: `fetch` shows the license and asks before downloading from the
  publisher, checks each part (SHA-256, or the git commit of a tag archive)
  and leaves nothing behind on failure. New crate `ferritron-sdk` (catalog
  and fetching). Without `sdk.toml`, builds behave as in 1.1.
- **macOS targets**: `cargo ferritron bundle --target aarch64-apple-darwin |
  x86_64-apple-darwin | universal-apple-darwin` (bundle layout chosen by
  the target, not the computer; universal joined with lipo). From Windows or
  Linux through xclang (`cargo ferritron sdk fetch xclang`, then `fetch
  macos` for Apple's SDK under Apple's terms). macOS 13.0 minimum
  (`MACOSX_DEPLOYMENT_TARGET`). `cargo ferritron notarize <bundle>` (Mac,
  `xcrun notarytool`). Off a Mac, bundles are signed with `rcodesign` if
  installed.
- CI: `.github/workflows/macos.yml` bundles the examples on macOS and runs
  the VST3 validator, clap-validator and auval; optionally validates bundles
  cross-built on Windows.
- Ferritron Editor (closed source): SDK settings (source, version, fetch
  after the license sheet, delete), checks for newer SDKs and for a newer
  Editor; the Windows starter kit is also an installer
  (`Ferritron-StarterKit-<v>-windows-x64-setup.exe`, Inno Setup, per user),
  and an Editor installed with it updates itself: the new setup is
  downloaded in the background, checked against the release's SHA-256 and
  run without a window when the Editor quits. The Editor's program is
  `Ferritron Editor.exe` in every package.

### Changed
- **Project files are `<name>.ferritroject`**. `.frtx` is still read
  everywhere (build, CLI, Editor); a package with both is an error. New
  projects, the examples and locked projects (`.ferritroject-locked`;
  `.frtx-locked` still opens) use the new name.
- `ParamSet` gained `options` (default provided); `Context` and the
  `ferritron-core` state format gained the fields above. `Input` (GPU) has a
  new variant, `Leave`: an exhaustive `match` on it needs a new arm.

### Not in this release
- Instance handles, track info and editor messages need the plugin and its
  editor in the host's process: with `[isolation]` or `[view] process =
  true`, and during hot reload, `Ui::instance` is `None` and handles see no host.
- Audio Unit: edits through handles use `ferritron.host.v1` (one-shot
  edits); no track info.
- macOS: still not built or run on a Mac by the maintainers; the CI
  workflow is written but has not run yet. Cross-built bundles are "builds",
  not "runs", until a Mac validates them.
- From the PanStudio requests, still open: a 2D drawing helper with Japanese
  font fallback for GPU editors, native tooltips and menus, multichannel
  buses, transport, sidechain, smoothing and meter helpers, presets, undo,
  DirectLink discovery, groups, floating editor windows; the CLAP
  validator's denormal warning was not investigated (no validator here).

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
