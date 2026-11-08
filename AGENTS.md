# Ferritron: guide for AI agents

How to go from a clean checkout to a working **VST3 / CLAP / LV2 / AU / AUv3 /
AAX / ARA 2** plugin, standalone app, Android app or **WAM** (web) with Ferritron. For details, the README is the reference ([README.en.md](README.en.md) in English,
[README.md](README.md) in Japanese) and
[docs/architecture.md](docs/architecture.md) (Japanese) is the design document.

Ferritron is a Rust audio plugin framework. A plugin is two files: a **project
file** (`<name>.ferritroject`, TOML) that declares everything static, and a **Rust
`lib.rs`** that implements `process`. The macro `#[ferritron::plugin]` generates
the typed parameters, stable parameter IDs and the VST3/CLAP entry points from
the project file.

**Version: 1.3.0** (all crates share it; history in [CHANGELOG.md](CHANGELOG.md)).
When you develop against Ferritron, state the version you target and pin it:
`ferritron = { path = "...", version = "1.3.0" }`. `ferritron::VERSION` and
`cargo ferritron --version` report the version in use.

## 1. Prerequisites

- Rust 1.89+ (workspace `rust-version`), MSVC toolchain on Windows (Visual
  Studio Build Tools provide the C++17 compiler).
- **Plugin-format SDKs are never bundled and must not be committed.** The user
  gets them and accepts their licenses, by hand or with
  `cargo ferritron sdk fetch <id>` (it shows the license and asks; never pass
  `--accept-license` for the user). `cargo ferritron sdk status` shows where
  each SDK is found. Builds find SDKs by one rule (`ferritron_build::sdk`): a
  version pinned in the project file's `[sdk]`, else `source` in `sdk.toml`
  (`auto` default: the environment variable, else Ferritron's SDK folder
  `C:\Ferritron\SDK` / `~/Ferritron/SDK`, `<root>/<id>/<version>`), else the
  build stops and lists where it looked. The environment variables:

| Variable | Needed for | Folder to point at | Source |
|---|---|---|---|
| `VST3_SDK_DIR` | VST3 (default) | the one containing `pluginterfaces/` | Steinberg VST3 SDK |
| `CLAP_SDK_DIR` | CLAP (default) | the one containing `include/clap/clap.h` | github.com/free-audio/clap |
| `ARA_SDK_DIR` | ARA 2 (feature `ara`) | the one containing `ARA_API` and `ARA_Library` | `git clone --recursive https://github.com/Celemony/ARA_SDK` (Apache 2.0) |
| `AAX_SDK_DIR` | AAX (feature `aax`) | the one containing `Interfaces/AAX.h` | Avid developer site (sign up; the user gets it) |
| `ANDROID_NDK_HOME` | Android apps (`--target android`) | the NDK root (`meta/platforms.json`) | developer.android.com/ndk (also `C:\Ferritron\SDK\ndk\<version>`) |
| `XCLANG_DIR` | macOS builds from Windows/Linux | the one containing `bin/xclang` | github.com/clice-io/xclang (Apache 2.0); Apple's macOS SDK through `cargo ferritron sdk fetch macos` (Apple's terms, the user's decision) |

Set them as user environment variables, or in `.cargo/config.toml`:

```toml
[env]
VST3_SDK_DIR = "C:/SDKs/vst3sdk"
CLAP_SDK_DIR = "C:/SDKs/clap"
ARA_SDK_DIR  = "C:/SDKs/ARA_SDK"
```

If a variable is missing, the build script fails with a message that names it.
A plugin that needs only one format can drop the other:
`ferritron = { ..., default-features = false, features = ["vst3"] }`.

Note that `cargo build --workspace` builds the ARA crate and therefore needs all
three variables. A single package (`cargo build -p gain`) needs only what its
features use.

## 2. Create a plugin

Inside this repository, copy [examples/gain](examples/gain). Outside it:

```bash
cargo install --path crates/ferritron-cli      # provides `cargo ferritron`
cargo ferritron new my-plugin
```

(Inside the repository `cargo ferritron` works through a Cargo alias without
installing. An installed `cargo-ferritron` finds the framework through
`FERRITRON_HOME`, the folder containing `crates/ferritron`; new projects depend
on it by path. The Ferritron Editor, closed source and downloaded from
Releases, makes the same projects.)

A plugin package needs:

- `Cargo.toml` with `[lib] crate-type = ["cdylib"]` and a `ferritron`
  dependency.
- `<name>.ferritroject` next to `Cargo.toml` (exactly one per package;
  `<name>.frtx` from before 1.2 is still read, but not both at once).
- `src/lib.rs`.

### Effect

`gain.ferritroject`:

```toml
[plugin]
id     = "dev.ferritron.examples.gain"   # unique, stable; never change after release
name   = "Ferritron Gain"
vendor = "Ztraqto"

[params]
gain = { type = "float", range = [-60, 12], default = 0, unit = "dB" }
```

`src/lib.rs`:

```rust
use ferritron::prelude::*;

#[derive(Default)]
struct Gain;

#[ferritron::plugin]
impl Process for Gain {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
    }
}
```

The plugin type must implement `Default`. `ctx.params.<key>` exists because the
key is declared in `[params]`.

### Parameter types

| `type` | Keys | Read with |
|---|---|---|
| `float` | `range = [min, max]`, `default`, `unit` | `.value() -> f32` |
| `int` | `range = [min, max]`, `default`, `unit` | `.value() -> i32` |
| `bool` | `default` | `.value() -> bool` |
| `enum` | `choices = ["A", "B"]`, `default = "A"` | `.index() -> usize` |

Optional `name` sets the display name; `automatable = false` keeps it out of
host automation, `hidden = true` out of host parameter lists (internal state).
**The key is the parameter's identity in saved projects and automation: do not
rename it after release.**

### Instances (1.2)

Each instance has an `InstanceHandle` (`ferritron::InstanceHandle`): the
plugin gets it in `fn init(&mut self, instance: &InstanceHandle)` and as
`ctx.instance()`; GPU editors as `ui.instance()`; web editors see
`ferritron.instance`. It gives the saved `id()`, the host's `track()`
(name, color, index), parameter edits from any non-audio thread that the
host records (`begin`/`set`/`end`/`edit`), `wake()` for a sleeping
instance, a `store()` saved with the project, and JSON messages with the
web editor (`on_message`/`post` ↔ `ferritron.send`/`ferritron.onMessage`).
`ferritron::instances::<MyPlugin>()` lists the live instances in the
process. Reference: [examples/instances](examples/instances). Handles need
the plugin and editor in the host's process (not with `[isolation]` or
`[view] process = true`).

### Instrument

Add `kind = "instrument"` under `[plugin]`. The plugin then receives notes
instead of audio input: iterate `ctx.notes` (each has `offset` in samples,
`key`, `velocity`, `kind: NoteKind::On | Off`) and write to
`ctx.io.channel_mut(0)`. Reference: [examples/sine-synth](examples/sine-synth).

### Editor (optional)

- **HTML/JS**: `[view] backend = "web"`, `entry = "ui"` (folder with
  `index.html`), `size = [w, h]`, and the `web` feature on `ferritron`. The
  page talks to the plugin through `window.ferritron` (`params`, `set`,
  `begin`/`end` for gestures, `onChange`, `t` for translations).
  See [examples/gain/ui](examples/gain/ui). Set `background = "#rrggbb"`
  (the page's background) to avoid a white flash. The browser behind the
  editor starts when an instance is created (`preload`, default true), so
  editors open in well under 100 ms. Right click shows no browser menu; a
  page that wants a menu listens to `contextmenu` and draws its own.
- **GPU (wgpu)**: `backend = "gpu"`, `editor = "<Type>"` implementing
  `ferritron::gpu_view::GpuEditor`. See [examples/gpu-view](examples/gpu-view).
- `[view] process = true` runs any editor in its own process.

### Silence and quality

- Ferritron stops calling `process` while the input is silent and the
  plugin's tail is over (`fn tail(&self) -> Tail`, default `Tail::Auto`).
  A plugin that makes sound from silence or whose output pauses for more
  than two seconds (long delays, loopers) returns `Tail::Infinite` or
  `Tail::Samples(n)`.
- `ctx.quality` is `Quality::Best` while the host exports and
  `Quality::Normal` otherwise. `[quality]` in the project file adds a
  user switch (Auto / Normal / Best). Allocate for `Best` in `activate`.

### Localization

Fluent files in `locales/<lang>/*.ftl`: `param-<key> = ...`,
`param-<key>-<index>` for enum choices. They are validated at compile time.
`cargo ferritron i18n check <package>` lists missing strings. UI-facing text for
the project's user is **Japanese**; keep English as the fallback.

## 3. Build, bundle, validate

```bash
cargo ferritron bundle gain
```

Builds in release mode and writes `target/bundled/<Name>.vst3` and
`<Name>.clap` (and `.component` on macOS). `--target aarch64-apple-darwin`
(or `x86_64-`, `universal-apple-darwin`) builds for macOS into
`target/<target>/bundled/`, through xclang when not on a Mac (macOS 13.0
minimum). `cargo ferritron notarize <bundle>` notarizes on a Mac.

For quick iteration inside a running host, build a dev bundle once and then use
plain `cargo build`; the new code is swapped in between audio blocks (debug
builds only):

```bash
cargo ferritron bundle gain --dev --install
cargo build -p gain
```

Test without a DAW:

```bash
cargo run -p ferritron-testhost -- "target/bundled/Ferritron Gain.vst3" --seconds 60
```

For unit tests, `ferritron::testing::Harness::<MyPlugin>::new(48_000.0, 512)` runs
the plugin through the same path as hosts (`set_param`, `automate`, `process`,
notes, state, `params()`, `instance()`, `set_track`, `send_message`). GPU
editor logic can be tested with `gpu_view::Ui::for_tests::<MyPlugin>(w, h)`.
`ferritron-testhost` takes `--instances N`, `--gui DIR` (PNG screenshots),
`--drag`, `--move`, `--wheel`, `--leave`.

Official validators:

- VST3: build the SDK's `validator` target, run
  `validator -e "target/bundled/<Name>.vst3"`.
- CLAP: `clap-validator validate "target/bundled/<Name>.clap"`.

## 3b. Other outputs (1.3)

```bash
cargo ferritron bundle gain                     # + LV2 folder, standalone app (Windows .exe; macOS .app with AUv3)
cargo ferritron bundle gain --target web        # WAM 2.0 folder (rustup target add wasm32-wasip1); serve over HTTP
cargo ferritron bundle gain --target android    # APK (NDK, Android SDK, JDK 17-21); x86_64-linux-android for the emulator
cargo ferritron bundle gain --target ios        # app + AUv3; needs a Mac with Xcode
```

- LV2, standalone and AUv3 are default features; AAX is `features = ["aax"]`
  and needs the user's AAX SDK (the crate builds empty with a warning without it).
- Phone and web editors are the plugin's HTML/JS editor with the same
  `window.ferritron`; plugins without one get an editor made from their parameters.
- Verified on this machine: LV2 (test host), standalone on Windows, WAM in a
  browser, Android on the emulator. Not verified: macOS/iOS (type-checked
  only), AAX (written without the SDK). Say so; do not claim otherwise.
- Test LV2 bundles from `target/bundled`, never install them into `%APPDATA%\LV2`
  while testing (same rule as VST3/CLAP).

## 4. ARA 2 plugin

ARA hands the plugin whole audio files and their regions. Required changes to
an ordinary plugin:

1. `ferritron = { ..., features = ["ara"] }` and `ARA_SDK_DIR` set.
2. In the project file, add a plugin `url` (ARA requires one) and an empty
   `[ara]` table:

   ```toml
   [plugin]
   url = "https://example.com/my-plugin"

   [ara]
   ```

3. Implement `AraRender`:

   ```rust
   use ferritron::ara::{AraRender, Source};

   impl AraRender for MyPlugin {
       fn render_region(&mut self, params: &Params, source: &Source<'_>,
                        start: i64, out: &mut [&mut [f32]]) {
           source.copy_to(start, out);   // or read source.channel(c) freely
       }
   }
   ```

   The rendered region audio then runs through `process` like any input.

Optional switches on the trait: `const TIME_STRETCH: bool = false` to opt out of
host time stretching; `const ANALYZES_NOTES: bool = true` (with
`analyze_notes`, default YIN for monophonic sources) to give the host the notes
of each file. Both buses may be mono. Without `[ara]` nothing ARA is linked.
References: [examples/ara-reverse](examples/ara-reverse),
[examples/ara-stretch](examples/ara-stretch).

## 5. Optional project-file features

| Table | Effect |
|---|---|
| `[quality]` | A "Quality" switch (Auto / Normal / Best) for `ctx.quality`; Auto is Best while the host exports |
| `[mcp]` | Lets AI clients reach the running plugin through the `ferritron-mcp` hub, behind a user-controlled "AI Access" switch (off by default) |
| `[isolation] mode = "process"` | Plugin runs in its own process; a crash silences it instead of taking down the host. Also `share`, `pipeline` |
| `[view]` | Editor (see above) |

By default a panic in plugin code is caught and the plugin outputs silence.
GPU processing, node graphs, Slang kernels, C++ and C# plugins and WebAssembly
sandboxing exist too; see the README sections of the same names.

## 6. Rules for agents working in this repository

- **Never `--install` into the system plugin folders while testing**
  (`C:\Program Files\Common Files\VST3` / `CLAP`). Test from `target/bundled`
  with `ferritron-testhost` or the validators.
- Do not download or commit SDKs into the repository, and do not assume the
  user accepted a license: ask them to download, or ask before you do it.
- Do not rename a parameter key or a plugin `id` that has shipped.
- Design principle: a **simple core with open extension points**; do not trade
  flexibility away for convenience.
- Real-time code (`process`) must not allocate, lock or block.
- Windows is the verified platform. **macOS / Audio Unit code is written and
  type-checked (`cargo check --target aarch64-apple-darwin`) but has never been
  built or run on a Mac**; say so rather than claim it works. Linux has no
  editor yet.
- The framework is named **Ferritron** (`Rvst` is only a codename); the
  vendor/author is **Ztraqto**; keep the `dev.ferritron.*` IDs.

## 7. Crate map

| Path | Role |
|---|---|
| `crates/ferritron` | The crate plugins depend on (prelude, features) |
| `crates/ferritron-core` / `-macros` / `-manifest` | Runtime, `#[plugin]` macro, `.ferritroject` parsing |
| `crates/ferritron-vst3` / `-clap` / `-au` / `-lv2` / `-auv3` / `-aax` | Plugin format adapters |
| `crates/ferritron-standalone` | Standalone apps (Windows, macOS, Linux, iOS, Android) |
| `crates/ferritron-wam` | Web Audio Module (WAM 2.0) files for `--target web` |
| `crates/ferritron-ara` | ARA 2 |
| `crates/ferritron-web` / `-gpu-view` | HTML/JS and wgpu editors |
| `crates/ferritron-gpu` / `-graph` / `-slang` / `-vulkan` | GPU processing |
| `crates/ferritron-wasm` | Isolation level 2 (WebAssembly) |
| `crates/ferritron-cli` | `cargo ferritron` (new, bundle, i18n) |
| `crates/ferritron-build` | Shared build-script helpers (SDK lookup, `FERRITRON_SKIP_NATIVE`) |
| `tools/testhost` | Minimal VST3 host for testing |
| `tools/ferritron-mcp` | MCP hub for AI clients |
| (Releases) | Ferritron Editor: closed source, not in this repository; downloaded alone or in the starter kit |
| `examples/*` | Working plugins: start from `gain`, `sine-synth`, `ara-reverse` |
