<p align="center"><img src="docs/images/ferritron-icon.png" width="128" alt="Ferritron"></p>

# Ferritron

A simple, fast and extensible audio plugin framework written in Rust.

**English** | [日本語](README.md)

> **Status: early development.** Ferritron builds stereo **effects and
> instruments** as **VST3 and CLAP on Windows** (macOS, with Audio Units, is
> implemented but not yet tested on a Mac), with sample-accurate automation,
> state saving, a built-in bypass, localization, **HTML/JS and GPU editors**
> and **hot reload** (rebuild while the DAW keeps running). The examples pass
> Steinberg's official VST3 validator (537/537 extensive tests) and
> clap-validator (0 failures). See [docs/architecture.md](docs/architecture.md)
> (Japanese) for the design and roadmap.

## Platforms and plugin formats

| | Windows (x64) | macOS | Linux (x64) |
|---|---|---|---|
| VST3 | ✅ | 🚧 **macOS developers wanted** | ✅ |
| CLAP | ✅ | 🚧 **macOS developers wanted** | ✅ |
| Audio Unit (AU v2) | — | 🚧 **macOS developers wanted** | — |
| ARA 2 (extension of VST3 / CLAP) | ✅ | 🚧 **macOS developers wanted** | ⚠️ |
| Editors (HTML/JS, GPU) | ✅ | 🚧 **macOS developers wanted** | — (not yet) |
| Hot reload | ✅ | 🚧 **macOS developers wanted** | ⚠️ |
| Process isolation | ✅ | 🚧 **macOS developers wanted** | ⚠️ |
| Ferritron Editor | ✅ | — | ⚠️ (build and command line only) |
| AAX / LV2 | planned | planned | planned |

- ✅ verified (VST3: Steinberg's official validator; CLAP: clap-validator; ARA:
  the ARA SDK's `ARATestHost`; Linux in Docker on Debian)
- 🚧 implemented and type-checked, but not yet built or run on a Mac
  ([macOS developers wanted](#macos-developers-wanted))
- ⚠️ the code exists but has not been checked
- — not supported

## macOS developers wanted

The macOS port (VST3 / CLAP / Audio Unit, editors, ARA, process isolation, hot
reload) is written and type-checked but has not been checked on a Mac yet.
**We are looking for developers with a Mac to build it, test it and fix what
they find.**

- Run `cargo ferritron bundle` on a Mac to make `.vst3` / `.clap` / `.component` bundles
- Check them with the VST3 validator, clap-validator and `auval` (the CI
  workflow `.github/workflows/macos.yml` does the same)
- Load them in DAWs such as Logic Pro, Ableton Live or REAPER; try the editors and ARA

See [Contributing](#contributing) for how to join.

## Contributing

**Joining the Discord server (<https://discord.gg/HbvnNMpFdV>) is required to take part in
Ferritron's development.** Please introduce yourself there before reporting
bugs, proposing features or opening pull requests. Help with the macOS port is
coordinated there too.

## A complete plugin

`gain.ferritroject`, the project file (TOML inside; one per package, next to
`Cargo.toml`):

```toml
[plugin]
id     = "dev.ferritron.examples.gain"
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

`Cargo.toml` needs `crate-type = ["cdylib"]` under `[lib]`. Parameters are
declared once in the project file; `#[ferritron::plugin]` generates the typed
`ctx.params` struct, stable parameter IDs and the format entry points from it.
See [examples/gain](examples/gain).

### Editors (HTML / JS)

Put a web UI in a folder and declare it. It is embedded into the plugin and
shown with the system WebView (WebView2 on Windows). There is no local
server, and it works in VST3 and CLAP.

```toml
# gain.ferritroject
[view]
backend   = "web"
entry     = "ui"          # must contain index.html
size      = [480, 240]    # logical pixels at 100% scale
resizable = true          # optional; then min_size / max_size (defaults: half size / 4096)
```

```toml
# Cargo.toml
ferritron = { ..., features = ["web"] }
```

```js
// ui/app.js: the page talks to the plugin through window.ferritron
const p = ferritron.params.gain;      // { name, unit, min, max, value, text, ... } (localized)
slider.addEventListener("pointerdown", () => ferritron.begin("gain"));
slider.addEventListener("input", () => ferritron.set("gain", slider.value));   // plain value (dB)
slider.addEventListener("pointerup", () => ferritron.end("gain"));
ferritron.onChange((key, param) => show(param));   // automation, host or UI changes
title.textContent = ferritron.t("ui-title");       // from locales/<lang>/*.ftl
```

Editor texts use the same translation files as parameters. `ferritron.t(key, args)`
returns any message in the user's language, and `{ $name }` variables are filled
from `args`. `cargo ferritron i18n check` also reports editor messages that
English defines but another language lacks.

Edits are reported to the host as gestures, so DAWs record them as automation.
See [examples/gain/ui](examples/gain/ui).

- **Fast to open**: on Windows the browser behind editors (WebView2) starts
  shortly after the first plugin instance is created, without blocking the host, and
  keeps running while instances exist. An editor then opens in about 60–80 ms
  instead of 300 ms or more (measured with `ferritron-testhost`). Every
  editor of a process shares it. `preload = false` under `[view]` starts it
  only when an editor opens (and lets it end with the last editor).
- **No white flash**: `background = "#16181d"` under `[view]` is painted from
  the first frame until the page draws itself; use the page's background.
- **Right click**: no browser menu (reload, inspect, ...). The page still
  gets `contextmenu` events, so an editor can show a menu of its own; set
  `ferritron.nativeContextMenu = true` to bring the browser's back (for
  example while developing). Browser keys (F5, Ctrl+P, Ctrl+F) are off too.
- **Built-in switches**: `ferritron.builtins` holds `"ferritron.bypass"` and,
  with `[quality]`, `"ferritron.quality"`, in the same shape as
  `ferritron.params`; `ferritron.set` / `edit` / `onChange` work with their keys.

- **HiDPI**: sizes are logical, and the editor is scaled to the host's scale
  factor (VST3 `IPlugViewContentScaleSupport`, CLAP `set_scale`) or else the
  window's DPI. Pages lay out at the logical size and render sharply.
- **Resizing**: with `resizable = true`, hosts can resize the editor within the
  limits. Lay the page out with relative CSS units. The size the user chose
  is saved with the project (extension `ferritron.editor-size.v1`), so the
  editor reopens at that size, also after reloading the project.

Check an editor without a DAW with
`ferritron-testhost <bundle> --gui <screenshot dir> [--click X Y] [--scale 1.5] [--resize W H]`.
It prints `timing:` lines (load, instance, editor open). With
`FERRITRON_TESTHOST_OPEN_AFTER=S` the editor opens S seconds after the
instance (as a user would), `FERRITRON_TESTHOST_REOPEN=N` closes and reopens
it N times, and `FERRITRON_TRACE=1` makes the plugin print where the time goes.
The click moves the real mouse; with `FERRITRON_TESTHOST_POST_CLICK=1` it is
posted to the window under the point instead (not for web editors, whose
input goes through WebView2). After `--resize`, a second instance loads the
saved state and reports the size its editor reopens at.

### Editors in a separate process

Any editor (web or GPU) can run in a process of its own, embedded in the
host's window as usual:

```toml
[view]
process = true
```

Parameter values and the editor's edits travel through shared memory; edits
still reach the host as gestures. If the editor process crashes, the host
and the sound are unaffected and a new editor process takes its place within
about a second. It also suits UI frameworks that want their own event loop.
The editor process is the plugin library itself under `rundll32` (nothing
extra to ship). Plugins with `[isolation] mode = "process"` always open their
editor this way. Windows only: macOS cannot embed another process's window,
so there editors open in the host process (isolated plugins too; their
editor then talks to the isolated instance through the same mirror).

### Editors drawn on the GPU (wgpu)

For editors that draw everything themselves (custom knobs, spectrum
displays, 3D), `backend = "gpu"` gives the plugin a wgpu surface in the host's
window instead of a web page. The project file names the type that draws:

```toml
[view]
backend = "gpu"
editor  = "Editor"        # a type implementing ferritron::gpu_view::GpuEditor
size    = [480, 240]
resizable = true
```

```rust
use ferritron::gpu_view::{Frame, Gpu, GpuEditor, Input, Ui};

impl GpuEditor for Editor {
    fn new(gpu: &Gpu, ui: &Ui) -> Self { /* pipelines for gpu.format */ }
    fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
        let gain = ui.normalized("gain");   // also ui.value / ui.text (localized) / ui.t
        /* draw into frame.target, submit to gpu.queue */
    }
    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        if let Input::Press { x, .. } = *event { ui.edit_normalized("gain", x / ui.width()); }
        true   // redraw
    }
}
```

With `ferritron = { ..., features = ["gpu-view"] }`. The editor is redrawn
when a parameter changes (automation, host or editor), after input that asks
for it, on resize, and on every tick (about 60 per second) while
`animating()` returns true. Edits reach the host as gestures. Input comes in
logical pixels; `frame` has the physical size and the scale factor. All
editors of a process share one GPU device, and presenting never waits for
vertical sync, so the host's UI thread is not held up. Text drawing is left to
the plugin (e.g. a glyph library on top of wgpu).

Keyboard: a click gives the editor the keyboard focus. `wants_key(key,
modifiers)` says which keys the editor uses; those arrive as `Input::Key`
(down, up, repeat) and typed text as `Input::Char` (after the keyboard layout
and input method), plus `Input::Focus`. Every other key goes to the host's
window, so the DAW keeps its shortcuts (space for transport) while the editor
has the focus. This works the same in an editor in a separate process
(`[view] process = true`). See
[examples/gpu-view](examples/gpu-view): a level bar drawn by one shader.

### Silence and export quality

**Idle plugins cost nothing.** When a plugin's input has been silent (below
-120 dBFS) for as long as its output can go on, Ferritron stops calling
`process` and outputs silence; input, a note or a parameter change wakes it
in the same block. Hosts are told (VST3 silence flags and tail samples, CLAP
`CLAP_PROCESS_SLEEP` and the `tail` extension), so they can skip work too.
How long the output goes on is `Process::tail`:

```rust
fn tail(&self) -> Tail {
    Tail::Samples((self.decay_seconds * self.rate) as u32)   // a reverb
}
```

- `Tail::Auto` (default): sleep after input and output were both silent, and
  no note was held, for two seconds. Right for most effects and instruments.
- `Tail::Samples(n)`: the output dies away `n` samples after the input stops
  (latency is added). Use it for delays whose echoes can pause longer than
  two seconds. It never cuts off sound that is still there.
- `Tail::Infinite`: never sleep (generators, test tones).

**Best quality on export, automatically.** `ctx.quality` is `Quality::Best`
while the host renders offline (export, bounce, freeze) and
`Quality::Normal` while the user plays and edits; it follows within the
block the host says so. What a level means is the plugin's choice:

```rust
let taps = if ctx.quality.is_best() { 512 } else { 64 };   // oversampling, filter length, ...
```

With `[quality]` in the project file, the plugin also gets a "Quality"
switch (音質: 自動 / 標準 / 最高 — Auto / Normal / Best) that the user can set
by hand; Auto is the rule above. It is saved with the project and not
automatable. `Setup::quality` gives the value at `activate`; allocate there
for `Quality::Best` if the levels need different buffers.

```toml
[quality]
```

In tests, `Harness::set_offline(true)` starts an export and
`Harness::sleeping()` tells whether the last block slept.

### Using several cores

`ctx.parallel_for` spreads independent work, such as voices, over cores:

```rust
ctx.parallel_for(&mut self.voices, |voice| voice.render(frames));   // then mix serially
```

It runs on the host's thread pool when the format offers one (CLAP
`thread-pool`). Otherwise it uses Ferritron's workers, raised to MMCSS
"Pro Audio" priority, if `parallel = true` is set under `[plugin]`. Otherwise
it runs on the calling thread. The output is identical in every case. It
never allocates or blocks: if another instance is using the workers, the
work runs on the calling thread instead of waiting. Without
`parallel = true`, no threads are started.

All Ferritron plugins in a process share **one** set of workers, even when
they come from different DLLs or vendors. This avoids every plugin bringing
its own threads and competing for the same cores. Check it with
`ferritron-testhost --shared-runtime A.dll B.dll`.

`cargo run --release -p ferritron-bench` measures it. On a 16-thread PC,
32 heavy voices went from 3106 to 449 us per 256-sample block (6.9x), and the
output matched the serial run exactly.

### GPU processing

`ferritron-gpu` runs heavy DSP on the GPU (wgpu: Vulkan / DX12 / Metal). The
first building block is `Convolver`, a long convolution split by latency. The
first taps (two to three blocks' worth) are convolved on the CPU, so there is
no latency. The long tail is convolved on the GPU with partitioned FFT
convolution, at least one block ahead, so its cost grows only slowly with IR
length. Hosts may change the block size every block. The audio thread never
calls the GPU API, and one GPU device is shared by the whole process.

```rust
// in activate():
self.reverb = ferritron_gpu::Convolver::new(&impulse_response, setup.max_block_size)?;
// in process():
self.reverb.process(ctx.io.channel_mut(0));
```

See [examples/gpu-reverb](examples/gpu-reverb), a 2-second convolution reverb.
On an RTX 3070 with 256-sample blocks (`cargo run --release -p ferritron-bench`):

| Impulse response | Direct CPU convolution, 1 core | Ferritron audio thread | GPU, in parallel | Missed deadlines |
|---|---|---|---|---|
| 2 s (96,000 taps) | 21.1 ms (396% of budget) | 115 us (2.2%) | 256 us | 0 of 400 |
| 10 s (480,000 taps) | 108.7 ms (2037%) | 115 us (2.2%) | 313 us | 0 of 400 |

The CPU baseline is direct convolution, not an FFT-based CPU implementation.
GPU time is mostly the fixed cost of one submission and readback per block.
If the GPU misses a block, that block gets the head only and
`missed_blocks()` counts it.

#### IIR filter banks

IIR filters look serial, but their state follows a linear recurrence, so
`IirBank` computes a block with a parallel scan. It runs many filter chains
(lanes of biquads) at once, and lanes with the same output are summed on the
GPU. Use it for modal synthesis, filter banks, or EQ on many channels. Each
section is realized in a numerically well-conditioned form, so results are
at least as accurate as a plain f32 biquad on the CPU. Coefficient changes
behave exactly like transposed direct form II.

```rust
use ferritron_gpu::{Biquad, Lane, RealtimeIirBank};
let lanes: Vec<Lane> = modes.iter()
    .map(|&f| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(f, 300.0, rate)] })
    .collect();
let mut bank = RealtimeIirBank::new(&lanes, max_block)?; // in activate()
bank.process(&[input], &mut [output]);                   // in process(): never waits
```

An IIR cannot be split by latency the way a convolution can. So
`RealtimeIirBank` delays its output by `latency()` frames (2 × `max_block`),
and the GPU work runs on its own thread. If the GPU is late anyway, a CPU
copy of the bank computes the missing frames, starting from the state the
GPU sent back with its last frames, so the output does not change; if the
GPU fails for good, the CPU carries on. `IirNode` turns this on when the CPU
can afford a block (at most half the budget).

All real-time banks of a process share one GPU thread, which batches the
pending blocks of every instance into one submission (`IirBank::process_many`
does the same for offline use). The same plugin on 32 tracks, 256 filters
each: 5,151 us per block with a submission per instance (97% of the budget)
against 1,695 us batched; in real time, 484 us of GPU time per block for all
32, with no missed blocks. `IirBank` waits for the GPU, which
suits offline rendering. With 2,000 resonators summed (`ferritron-bench`):

| | CPU, 1 core | Ferritron |
|---|---|---|
| Real time, 256-sample blocks | 1,795 us per block (34% of budget) | 6.6 us on the audio thread; GPU 270 us in parallel; 0 of 400 missed |
| Offline render of 10 s | 3.36 s | 0.061 s (55x faster) |

See [examples/gpu-modal](examples/gpu-modal), a modal synthesizer: 16 voices
of 128 resonators each (2,048 in all), excited by a noise mallet. Releasing a
note shortens its decay without a click, because the state is kept across the
coefficient change. Without a GPU, the same filters run on the CPU.

#### Reporting latency

A plugin that delays its output reports it, and the host compensates:

```rust
impl Process for MySynth {
    fn latency(&self) -> u32 {
        self.bank.latency() as u32 // read after every activate()
    }
    // ...
}
```

VST3 (`getLatencySamples`) and CLAP (`latency` extension) pass it to the host.

#### Offline rendering

When the host bounces or exports, it calls `process` as fast as it returns.
Real-time GPU components would then drop work they cannot wait for. So
`Setup::offline` tells the plugin, from VST3's `processMode` or CLAP's
`render` extension, and GPU components can wait instead:

```rust
fn activate(&mut self, setup: &Setup) {
    let mode = ferritron_gpu::Mode::from_offline(setup.offline);
    self.reverb = ferritron_gpu::Convolver::with_mode(&ir, setup.max_block_size, mode).ok();
    // IIR banks: IirBank (waits, no latency) offline, RealtimeIirBank otherwise.
}
```

A 60 s bounce through the 2 s reverb (`ferritron-bench`): real-time mode
finishes in 1.25 s but loses the tail in 44 of 11,250 blocks; offline mode
takes 2.09 s (29x real time) and loses nothing. Both examples switch modes
this way. Tests render with `Harness::offline`.

#### Node graph and automatic placement

`ferritron-graph` (re-exported as `ferritron_gpu::graph`) connects nodes,
channel to channel, and runs them in order every block. Signals into the
same input are summed. When a node adds latency (a GPU pipeline, a
look-ahead), the graph delays the other paths so they line up, and reports
the total as `graph.latency()`.

```rust
use ferritron_gpu::graph::{Gain, Graph, NodeSetup, Policy};
let mut graph = Graph::new(1, 1);
let conv = graph.add(ferritron_gpu::ConvolverNode::new(&ir, &setup)?);
let wet = graph.add(Gain::new(1, 0.3));
graph.connect(Graph::INPUT, 0, conv, 0)?;
graph.connect(conv, 0, wet, 0)?;
graph.connect(wet, 0, Graph::OUTPUT, 0)?;
graph.connect(Graph::INPUT, 0, Graph::OUTPUT, 0)?; // dry, summed
graph.prepare(max_block)?;
graph.process(&[input], &mut [output]);            // audio thread
graph.node_mut::<Gain>(wet).unwrap().set(0.5);     // change a node
```

Nodes are placed by measurement. `IirNode` builds a CPU bank and a GPU bank,
runs both, and keeps the faster one that the GPU policy allows:

| Policy (`FERRITRON_GPU_POLICY` overrides the plugin's choice) | Allowed |
|---|---|
| `off` | CPU only |
| `balanced` (default) | GPU only without added latency |
| `max` | GPU with latency too, if the CPU would need more than 1/8 of the block |

With 256-sample blocks (`ferritron-bench`), real time with `max` keeps up
to 256 filters on the CPU and moves 1,024 and more to the GPU; offline, the
GPU wins from 256 filters. `examples/gpu-reverb` is built as a graph, and
`examples/gpu-modal` uses an `IirNode`.

#### Fusing GPU stages

Every GPU component pays a fixed cost per block: upload, submit, wait,
read back. Banks in series can instead be one chain, `IirBank::chain(&[&a,
&b, &c], max_block)` (also `RealtimeIirBank::chain`, `CpuIirBank::chain`
and `IirNode::chain`). Stage `k` reads the outputs of stage `k - 1`, the
signals stay on the GPU, and the whole chain costs one round trip:

| 1,024 resonators → 16-channel EQ → stereo, 256-sample blocks | Separate banks | Fused chain |
|---|---|---|
| GPU time per block (offline) | 559 us (3 round trips) | 224 us (1 round trip) |
| Real-time latency | 1,536 samples | 512 samples |

A graph does this by itself: when a node feeds only the next node, channel
for channel, `prepare()` asks the two to fuse (`Node::fuse`). IIR nodes in
series become one chain (placed again as a whole); gains in series become
one multiplication. Ids of the merged nodes keep working:
`graph.node_mut::<IirNode>(eq).set_lane(2, ...)` still reaches lane 2 of
that node. `graph.set_fusion(false)` turns it off. Three heavy IIR nodes in
series (1,024 filters each): 725 us per block and 1,536 samples of latency
unfused, 315 us and 512 samples fused.

#### Slang kernels: write once, run on the CPU or the GPU

A kernel is one Slang function, called for every sample of every channel:

```slang
// kernels/drive.slang
float process(float x, KernelArgs args)
{
    float drive = max(args.values[0], 1e-6);
    return tanh(x * drive) / tanh(drive) * args.values[1];
}
```

`ferritron-slang` compiles it in the build script, to WGSL for the GPU and
to C++ for the CPU (compiled and linked into the plugin):

```rust
// build.rs
fn main() {
    ferritron_slang::compile("drive", "kernels/drive.slang").unwrap_or_else(|e| panic!("{e}"));
}
// lib.rs
let node = ferritron_gpu::KernelNode::new(ferritron_gpu::slang_kernel!(drive), 2, &setup);
```

`KernelNode` measures both and runs the faster one (the GPU is offered for
offline rendering). Like the plugin SDKs, Slang is not included: download a
release from https://github.com/shader-slang/slang/releases and set
`SLANG_DIR`. See [examples/slang-drive](examples/slang-drive).

#### Ferritron Chain: modules in one graph

Plugins in a DAW pass audio to each other on the CPU, so each GPU plugin
pays its own round trip. Modules built into one plugin share one graph
instead: a `Module` adds its nodes, `Graph::chain` connects modules in
series, and fusion works across their borders. See
[examples/gpu-chain](examples/gpu-chain): a resonant body (1,024 filters),
a tone EQ and a level, where body and tone run as one GPU chain.

#### Offline batches, stateful kernels, many instances

- `ferritron_graph::Batched::new(node, 8192)` runs a node on batches of
  8192 frames whatever the host's block size, and reports the batch as
  latency: one GPU round trip per batch when rendering offline (4000
  resonators: 4.8x faster than host blocks of 256).
- A Slang kernel may keep state per channel across blocks:
  `float process(float x, KernelArgs args, inout KernelState state)`, bound
  with `slang_kernel!(name, stateful)`. Kernel nodes in series fuse into
  one chain (one round trip per block).
- All convolvers of a process share one GPU thread, which submits the due
  work of every instance at once.

#### Persistent kernels, Tensor cores, RT cores

The `ferritron-vulkan` crate adds what wgpu cannot do, on the same Vulkan
device as wgpu (a second queue, so both run side by side). Its shaders are
written in Slang; their SPIR-V is kept in the repository, so building needs
no Slang.

```rust
// A persistent kernel: the GPU in real time at 64-sample blocks, no latency.
let modes = IirNode::new_low_latency(&lanes, &setup);       // opt-in, see below
// Tensor cores: hundreds of channels mixed to hundreds of channels.
let mut mixer = MatrixMixerNode::new(256, 256, &setup);      // CPU or Tensor cores, measured
mixer.set_gain(out, input, 0.5);
// RT cores: a room's impulse response from its geometry.
let mut room = ferritron_gpu::room::Room::shoebox([10.0, 8.0, 4.0], 0.15);
room.add_box([2.0, 2.0, 0.0], [3.0, 3.0, 1.0], 0.3);
let (energy, tracer) = ferritron_gpu::room::energy_response(&room, &trace);
let ir = ferritron_gpu::room::impulse_response(&energy, 64, 1);
```

- **Persistent kernel** (`IirNode::new_low_latency`): the kernel stays on
  the GPU and is handed each block through memory. RTX 3070: 64 samples ×
  2048 resonators in 46 µs (median, 49 µs at the 99th percentile). Opt-in,
  because it keeps part of the GPU busy waiting while audio runs, which
  slows other GPU work on the computer; one per computer.
- **Tensor cores** (`MatrixMixerNode`, `TensorMixer`): cooperative
  matrices, f16 products with f32 sums (about three decimal digits). 256×256
  channels × 512 frames: 0.35 ms on the GPU, 4 ms on the CPU.
- **RT cores** (`room`): ray queries against the room's acceleration
  structure, or the same algorithm on the CPU. A furnished room of 1,116
  triangles: 2 ms on the GPU, 5.8 s on the CPU; the reverberation time
  matches Sabine's formula.

### Writing plugins in C++

`ferritron.hpp` (C++20, header only, next to `ferritron_abi.h`) turns a
class into a Ferritron plugin; the format adapters treat it like any other:

```cpp
#include "ferritron.hpp"

struct Gain {
    static constexpr ferritron::Info info{"com.example.gain", "Gain", "Example", "1.0.0"};
    // key, name, unit, min, max, default, steps (0 = continuous)
    static constexpr ferritron::Param params[]{{"gain", "Gain", "dB", -60.0, 12.0, 0.0}};

    void process(ferritron::Block& block) {
        float g = ferritron::db_to_gain(block.param(0));
        for (auto channel : block.channels())
            for (float& s : channel) s *= g;
    }
};
FERRITRON_PLUGIN(Gain)
```

As for Rust plugins, parameter changes take effect at their exact sample, a
bypass parameter is added, state is saved, denormals are flushed and
exceptions never cross the ABI. Parameter and class IDs are computed the
same way as in Rust. A small Cargo crate compiles the C++ and links the
format adapters: see [examples/cpp-gain](examples/cpp-gain). (MSVC 14.50
crashes on designated initializers in the parameter table: use positional
ones.)

### Writing plugins in C#

`bindings/csharp/Ferritron.cs` turns a C# class into a Ferritron plugin,
built with NativeAOT into a native library (no .NET install needed by
users):

```csharp
public sealed class Gain : Plugin
{
    static readonly PluginInfo Info = new("com.example.gain", "Gain", "Example");
    static readonly Param[] Params = { new("gain", "Gain", "dB", -60, 12, 0) };

    public override void Process(in Block block)
    {
        float g = Dsp.DbToGain(block.Param(0));
        for (int c = 0; c < block.Channels; c++)
            foreach (ref float s in block.Channel(c)) s *= g;
    }

    [ModuleInitializer]
    internal static void Register() => Host.Register<Gain>(Info, Params);
}
```

The library is a *companion*: the plugin binary is a small Rust crate with
the format adapters and `ferritron_core::companion_plugin!("Gain.dll")`,
which loads it from the plugin's folder; `cargo ferritron bundle` copies it
there (`[companion] files` in the project file). `Process` must not allocate
(the garbage collector must not run on the audio thread). See
[examples/csharp-gain](examples/csharp-gain); it needs the .NET SDK (8 or
later) and, for NativeAOT on Windows, the Visual Studio C++ tools.

Other languages with their own runtime (Python, for example) plug in the
same way: a companion library that exports `ferritron_plugin_entry`.

### ARA 2 (Audio Random Access)

ARA hosts (Studio One, Cubase, Logic, REAPER, ...) hand the plugin whole
audio files and tell it where their regions play, so it can read any part
of a file at any time: what pitch and time editors, restoration and
analysis tools need.

```toml
[plugin]
url = "https://example.com/my-plugin"   # required by ARA

[ara]
```

```rust
use ferritron::ara::{AraRender, Source};

impl AraRender for MyPlugin {
    // Called for each part of a playback region inside the block.
    fn render_region(&mut self, params: &Params, source: &Source, start: i64, out: &mut [&mut [f32]]) {
        source.copy_to(start, out);   // or anything: source.channel(c) is the whole file
    }
}
```

With `ferritron = { ..., features = ["ara"] }` and your own ARA SDK in
`ARA_SDK_DIR` (`git clone --recursive https://github.com/Celemony/ARA_SDK`,
Apache 2.0; Ferritron does not ship it). With it, the VST3 adapter offers
the "ARA Main Factory Class" and `IPlugInEntryPoint2`, and the CLAP adapter
the ARA factory and plug-in extension. Ferritron keeps each audio source the
host enables in memory; the regions' audio then runs through the plugin's
`process` like any input. Without `[ara]`, nothing ARA is linked.
Both adapters also accept mono buses now (ARA hosts render mono files that
way). [examples/ara-reverse](examples/ara-reverse) plays files backwards.

**Time stretch and analysis.** Hosts can stretch regions in time (the pitch
stays): `render_stretched` has a default, a granular stretch whose grains line
up with each other (waveform similarity), and `const TIME_STRETCH: bool = false`
opts out for a plugin whose rendering cannot follow a stretched timeline (like
the reverse example). A plugin that sets `const ANALYZES_NOTES: bool = true`
gives the host the notes of every audio file (pitch, level, start and length;
what a pitch editor shows): `analyze_notes(source)` has a default for
monophonic sources (`ferritron::ara::detect_notes`, YIN) and runs on a
background thread. Stretching is linear over the region; tempo-following
stretch (`kARAPlaybackTransformationTimestretchReflectingTempo`) is not
offered. [examples/ara-stretch](examples/ara-stretch) uses both defaults.
Both examples pass every case of the ARA SDK's `ARATestHost` for VST3 and
CLAP.

### Crash isolation

By default, a panic in plugin code is caught and the plugin outputs silence.
For protection against real crashes (access violations, native libraries),
run the plugin in its own process:

```toml
[isolation]
mode = "process"      # default: "guarded" (in-process)
share = "plugin"      # default: instances of the plugin share one process; "instance": one each
pipeline = false      # true: blocks run one block late, in parallel with the host
```

If the plugin process dies, the host keeps running and the plugin outputs
silence. A new process starts automatically, and processing resumes with the
same parameters, typically within half a second. The child is the same
plugin library started through Windows' `rundll32`, so there is nothing extra
to ship. `FERRITRON_ISOLATION=process|guarded` overrides the setting. The
editor of an isolated plugin opens in a process of its own (see "Editors in
a separate process" below), so an editor crash does not even interrupt the
sound.

- **Sharing**: by default all instances of the plugin in one host run in one
  process (one thread each), which saves memory; a crash then silences them
  all until the process restarts. `share = "instance"` gives each instance
  its own process.
- **Pipeline**: by default the host waits for each block (no added latency,
  but one cross-process round trip per block). With `pipeline = true` the
  child processes a block while the host goes on; the output is one block
  (the host's maximum block size) late, reported to the host as latency.
- **Latency** the plugin reports in the child reaches the host (plus the
  pipeline's block).
- **Hot reload** works inside the isolated process: development bundles
  reload the rebuilt code there.
- `FERRITRON_ISOLATION_SHARE=plugin|instance` and
  `FERRITRON_ISOLATION_PIPELINE=0|1` override the project file.

Windows and macOS. On Windows the child process is the plugin library under
`rundll32`; on macOS a tiny helper, `ferritron-host`, which
`cargo ferritron bundle` compiles into each bundle's `Contents/Helpers`
(signals go through named FIFOs there). Sandboxed hosts cannot start it, so
isolated Audio Units say they are not sandbox-safe.
[tools/crash-fixture](tools/crash-fixture) is a plugin that really crashes,
for trying it:

```bash
ferritron-testhost "target/bundled/Ferritron Crash Fixture.vst3" --seconds 3 --pulse-param-at 1
```

#### WebAssembly sandbox (isolation Level 2)

A plugin built for `wasm32-wasip1` can run in a wasmtime sandbox inside the
host: `ferritron-wasm` implements the C ABI on top of the guest module.
The guest gets no files, environment or network; a crash inside it, or a
call that runs longer than 500 ms, only silences that plugin. The plugin
binary is a small crate that embeds the module:

```rust
ferritron_wasm::wasm_plugin!(include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm")));
```

See [examples/wasm-sandbox](examples/wasm-sandbox), which runs
[examples/wasm-crusher](examples/wasm-crusher) (a bit crusher with `crash`
and `hang` switches to try it). Needs `rustup target add wasm32-wasip1`.

### AI control through MCP

Plugins can be reached by AI clients (Claude Code, Codex, ...) through the
Model Context Protocol, also in DAWs that know nothing about MCP. One hub
serves the whole computer: configure it once in the AI client,

```bash
claude mcp add ferritron -- ferritron-mcp
```

and every Ferritron plugin that allows it, in any DAW, is reachable. A plugin
opts in with `[mcp]` in its project file; it then has an **AI Access** switch,
off for every new instance. Only instances whose switch the user turns on
connect to the hub (and they report which DAW process and track they are
on). The AI client can list them, read and set parameters (the DAW is told,
as for edits in the editor) and capture the audio before and after the
plugin as WAV files with their levels. The hub listens on 127.0.0.1 only and
accepts only processes that present the token in the user's Ferritron
folder. A second `ferritron-mcp` started by another AI client relays to the
first, so there is always one hub.

Tools: `list_plugins`, `get_parameters`, `set_parameter`, `capture_audio`.

### Sharing resources

The same plugin on many tracks, or several plugins of a vendor, often load
the same impulse responses, wavetables or samples. `ferritron::share` builds
them once:

- `share::cached(key, build)`: a value shared by the instances of this
  plugin library while any holds it (`Arc`).
- `share::bytes(name, build)`: read-only bytes in named shared memory,
  built once for the whole session: other plugin libraries and other
  processes (a second DAW, a DAW's plugin sandboxes) map the same physical
  memory. The OS frees it with its last user.

On the GPU, convolvers with the same impulse response share its spectra
automatically.

### DirectLink: plugins talking to each other

Ferritron plugins can exchange values through **DirectLink** channels, e.g.
a gain-match pair where the plugin after an effect needs the level before
it:

```rust
let link = ferritron::directlink::DirectLink::open("com.example.gainmatch", "group 1")?; // in activate
let level = link.port("level").unwrap();
level.publish(&[ms]);                       // audio thread: never blocks
if let Some(r) = level.read(&mut v) { /* r.age tells how fresh */ }
```

A channel is a small region of named shared memory: it works between plugin
libraries and processes, the OS frees it with its last user (nothing leaks,
nothing dangles after a crash), publishing and reading are lock-free and
allocation-free, and several writers never tear a value. Keep the channel
name in a parameter or the state, and reopening the project reconnects the
peers by themselves. See [examples/gain-match](examples/gain-match).

### Instruments

Set `kind = "instrument"` under `[plugin]`. The plugin then gets notes instead
of audio input. `ctx.notes` holds the block's note on/off events with sample
offsets. See [examples/sine-synth](examples/sine-synth): a 16-voice sine synth.

```rust
for note in ctx.notes {
    match note.kind {
        NoteKind::On => { /* start a voice at note.offset: note.key, note.velocity */ }
        NoteKind::Off => { /* release it */ }
    }
}
```

### Testing without a DAW

`ferritron::testing::Harness` runs a plugin through the same path hosts use
(sample-accurate automation, bypass, notes, state):

```rust
let mut h = ferritron::testing::Harness::<Gain>::new(48_000.0, 512);
h.set_param("gain", -6.0);
h.automate(32, "gain", 0.0);     // change at sample 32
let (mut l, mut r) = ([1.0; 64], [1.0; 64]);
assert!(h.process(&mut [&mut l, &mut r]));
```

### Instances, tracks and the store

Every instance has an `InstanceHandle`: the plugin gets it in
`Process::init` (and as `ctx.instance()`), GPU editors as `ui.instance()`,
web editors see `ferritron.instance`, and `ferritron::instances::<P>()`
lists the live instances of a plugin in the process:

```rust
fn init(&mut self, instance: &InstanceHandle) {
    instance.on_message(|json, me| me.post(format!("{{\"track\":{:?}}}", me.track().name)));
}
```

- `id()`: saved with the project; a duplicated track gets a new one.
- `track()`: name, color and position from the host.
- `begin` / `set` / `end` / `edit`: parameter edits from any thread but
  the audio thread, recorded by the host as automation; they wake the
  instance. `wake()` alone does too.
- `store()`: text or bytes by key, saved with the project.
- `on_message` / `post` with `ferritron.send` / `ferritron.onMessage` in
  web editors.

See [examples/instances](examples/instances). Handles need the plugin and
its editor in the host's process (not with `[isolation]` or a `[view]` in
its own process).

### Parameter types

| `type`  | Keys                                   | Read with      |
|---------|----------------------------------------|----------------|
| `float` | `range = [min, max]`, `default`, `unit` | `.value() -> f32` |
| `int`   | `range = [min, max]`, `default`, `unit` | `.value() -> i32` |
| `bool`  | `default = true/false`                 | `.value() -> bool` |
| `enum`  | `choices = ["A", "B"]`, `default = "A"` | `.index() -> usize` |

Every parameter also accepts an optional display `name` (defaults to the key in
title case), `automatable = false` (not offered for automation) and
`hidden = true` (not listed by hosts; for internal state). The key is the parameter's identity in saved projects and
automation, so don't rename it after release.

### Localization

Put [Fluent](https://projectfluent.org/) files in `locales/<language>/`:

```ftl
# locales/ja/main.ftl
param-gain = ゲイン
```

- `param-<key>` translates a parameter name; `param-<key>-<index>` translates
  a choice of an `enum` parameter. Untranslated strings fall back along
  `ja-JP` → `ja` → `en` → the name in the project file.
- The files are checked at compile time: Fluent syntax errors and keys that
  match no parameter are build errors with file and line.
- The display language is the first of: the `FERRITRON_LANGUAGE` environment
  variable, the setting in `%APPDATA%\Ferritron\settings.toml`, or the OS
  language. One setting covers every Ferritron plugin:

  ```toml
  language = "ja"
  [plugins."dev.ferritron.examples.gain"]   # optional per-plugin override
  language = "en"
  ```

- Anyone can add or override a language without rebuilding by placing
  `%APPDATA%\Ferritron\locales\<plugin id>\<language>.ftl`.
- `cargo ferritron i18n check <package>` lists missing strings per language
  and exits non-zero if any are missing, so it can run in CI.
- `cargo ferritron bundle` notices newly added `.ftl` files. With plain
  `cargo build`, edit any tracked file (or run `cargo clean -p <package>`)
  after adding a new one.

## Building

### 1. Get the official SDKs yourself

**Ferritron does not ship any plugin-format SDK.** It provides only the adapter
code that builds against the SDKs you supply. Both VST3 and CLAP are enabled by
default; to build only one, use
`ferritron = { ..., default-features = false, features = ["vst3"] }`.

The easiest way: let Ferritron fetch them, from their publishers, after you
read and accept each license (the Ferritron Editor does the same from its
settings):

```bash
cargo ferritron sdk available        # versions Ferritron knows and checked
cargo ferritron sdk fetch vst3       # shows the license; asks you to accept
cargo ferritron sdk fetch clap
cargo ferritron sdk status           # what each build will use, and why
```

They go to Ferritron's SDK folder, `C:\Ferritron\SDK` (Windows) or
`~/Ferritron/SDK`, one folder per version (`<root>/<id>/<version>`; you may
also put an SDK there by hand). Builds find each SDK by one rule:

1. A version pinned by the project file (`[sdk] vst3 = "3.8.1"`), used by
   `cargo ferritron bundle`;
2. its `source` in `sdk.toml` (`%APPDATA%\Ferritron\sdk.toml`, or the same
   folder as the Editor's settings elsewhere): `auto` (default: the
   environment variable if set, else the managed version), `env`, `managed`
   or `path`;
3. else the build stops and lists every place it looked.

```toml
# sdk.toml (written by `cargo ferritron sdk use / source` and the Editor)
root = "C:/Ferritron/SDK"       # FERRITRON_SDK_ROOT overrides it
[vst3]
source  = "managed"
version = "3.8.1"
[ara]
source = "path"
path   = "D:/work/ARA_SDK"
```

Without `sdk.toml` nothing changes from Ferritron 1.1: environment variables
still work. By hand: download the VST3 SDK from Steinberg
(<https://github.com/steinbergmedia/vst3sdk>, with its submodules) and CLAP from
<https://github.com/free-audio/clap>, accept their licenses, and point
`VST3_SDK_DIR` (the folder containing `pluginterfaces/`) and `CLAP_SDK_DIR`
(the folder containing `include/clap/clap.h`) at them, for example in your
project's `.cargo/config.toml`:

```toml
[env]
VST3_SDK_DIR = "C:/SDKs/vst3sdk"
CLAP_SDK_DIR = "C:/SDKs/clap"
```

### 2. Requirements

- Rust 1.89 or newer
- A C++17 compiler: on Windows, the Visual Studio Build Tools that Rust's MSVC toolchain already uses; on Linux, GCC or Clang

**macOS** (Apple Silicon and Intel, macOS 13 or newer; `MACOSX_DEPLOYMENT_TARGET` changes it) needs the Xcode
command line tools (`xcode-select --install`): `cargo ferritron bundle gain`
writes `<name>.vst3`, `<name>.clap` and `<name>.component` (Audio Unit)
bundles with an Info.plist, signs them (ad hoc, or with the identity in
`FERRITRON_CODESIGN_IDENTITY`; `cargo ferritron notarize <bundle>` then
notarizes it with `xcrun notarytool`, credentials in `FERRITRON_NOTARY_PROFILE`
or `FERRITRON_NOTARY_KEY` / `_KEY_ID` / `_ISSUER`: downloaded plugins that are
not notarized are stopped by Gatekeeper), and `--install` copies them to
`~/Library/Audio/Plug-Ins/{VST3,CLAP,Components}`. Editors (web: WKWebView;
GPU: Metal), hot reload, process isolation, ARA and the Audio Workgroup all
have macOS implementations. **They are written and type-checked
(`cargo check --target aarch64-apple-darwin` / `x86_64-apple-darwin`) but
not yet built or run on a Mac**; see the status in
[docs/architecture.md](docs/architecture.md).

**Linux** works the same way (`cargo ferritron bundle gain` writes
`<name>.vst3/Contents/x86_64-linux/<name>.so` and `<name>.clap`; `--install`
copies to `~/.vst3` and `~/.clap`). There is no editor on Linux yet. Hot
reload and process isolation use the same POSIX code as macOS but are not
tested on Linux (isolation needs the `ferritron-host` helper, which only
directory bundles such as VST3 get).
All examples pass both validators on Linux (checked in Docker, Debian with
GCC). A software GPU (llvmpipe, lavapipe, WARP) counts as no GPU: GPU nodes
then run on the CPU.

### 3. Create a plugin (optional)

```bash
cargo install --path crates/ferritron-cli   # provides `cargo ferritron` everywhere
cargo ferritron new my-plugin
```

This creates a ready-to-build project (gain example, Japanese translation, SDK
settings template) that depends on this Ferritron folder by path. Inside this
repository the `cargo ferritron` alias works without installing; an installed
`cargo-ferritron` finds Ferritron through `FERRITRON_HOME` (set it to this
folder, the one containing `crates/ferritron`).

#### Ferritron Editor

The **Ferritron Editor** sets up and edits projects, like JUCE's Projucer,
from a window, from the command line or from an AI assistant. It is not part
of this repository: it is closed source and comes as a download on the
[Releases](https://github.com/Ztraqto/Ferritron/releases) page, alone or in the
starter kit together with Ferritron.

```bash
ferritron-editor new my-synth --template instrument --vendor "Acme"
cd my-synth
ferritron-editor param add cutoff --type float --min 20 --max 20000 --default 1000 --unit Hz
ferritron-editor feature mcp on
ferritron-editor info
ferritron-editor bundle
```

- It makes the same projects as `cargo ferritron new` and checks every edit
  with the same rules as the build (`ferritron-manifest`) before writing:
  an invalid edit changes nothing, and comments in the project file are kept.
- **GUI** (Tauri 2, following Apple's Human Interface Guidelines): several
  projects at once, parameters, features, building, English and Japanese.
- **AI:** `ferritron-editor mcp` is an MCP server over stdio
  (`claude mcp add ferritron-editor -- ferritron-editor mcp`).
- **Project lock:** a project can be encrypted into one file that opens only
  with a passkey (Windows Hello, a security key) or a recovery code; it is
  unlocked only while the Editor has it open.
- New projects depend on the Ferritron framework found through
  `FERRITRON_HOME`, or next to the Editor (the starter kit's layout).

### 4. Build and bundle

```bash
cargo ferritron bundle gain
```

This builds the `gain` package in release mode and writes
`target/bundled/Ferritron Gain.vst3` and `target/bundled/Ferritron Gain.clap`
(and `Ferritron Gain.component` on macOS).
Copy them to `C:\Program Files\Common Files\VST3\` and `...\Common Files\CLAP\`,
or pass `--install` from an administrator shell. Then rescan plugins in your DAW.

#### Audio Units (macOS)

The `au` feature (on by default; it builds nothing on other systems) adds an
AU v2 adapter. It needs no SDK: AU v2 is a C interface of macOS. The codes
the component is registered with default to `aufx` / `aumu` (effect,
instrument), a subtype derived from `plugin.id` and a manufacturer code from
`plugin.vendor`; set them to keep them stable:

```toml
[au]
type = "aufx"          # aufx (effect), aumu (instrument), aumf (effect taking MIDI)
subtype = "Gain"       # 4 characters, unique among your plugins
manufacturer = "Ztrq"  # 4 characters, at least one not lowercase
```

Check a bundle with `auval -v aufx Gain Ztrq` after `--install`. Hosts that
support it hand over their Audio Workgroup (render context observer), and
Ferritron's worker threads join it.

#### Building for macOS from Windows or Linux (xclang)

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo ferritron sdk fetch xclang     # the cross toolchain (Apache 2.0)
cargo ferritron sdk fetch macos      # Apple's macOS SDK, through xclang
cargo ferritron bundle gain --target universal-apple-darwin
```

`--target` takes `aarch64-apple-darwin`, `x86_64-apple-darwin` or
`universal-apple-darwin` (both in one binary); the bundles go to
`target/<target>/bundled/`. Off a Mac the build runs through
[xclang](https://github.com/clice-io/xclang) (`xclang cargo build`), found like
an SDK (`XCLANG_DIR`, `sdk.toml`, the SDK folder). The macOS SDK is Apple's and
used under Apple's terms, which allow it on Apple hardware: building for macOS
on Windows or Linux with it is your decision. Such bundles carry only the
linker's ad-hoc signature (or `rcodesign`'s, if installed); to distribute them,
sign and notarize on a Mac. This path builds; that the result runs is checked
on a Mac (the CI workflow `.github/workflows/macos.yml` validates cross-built
bundles there when the repository variable `FERRITRON_CROSS_MACOS` is `true`).

#### Checking macOS code from Windows or Linux

```bash
rustup target add aarch64-apple-darwin
FERRITRON_SKIP_NATIVE=1 cargo check --workspace --target aarch64-apple-darwin
```

`FERRITRON_SKIP_NATIVE` skips the C, C++ and Objective-C parts (there is no
Apple toolchain to compile them) and type-checks all Rust code for macOS.
A dependency that compiles C (wasmtime) still wants a compiler for the
target: point `CC_aarch64_apple_darwin` at a clang.

### Hot reload (development)

Update a plugin inside a running DAW without restarting it:

```bash
cargo ferritron bundle gain --dev --install
```

Install the dev bundle once and load it in your DAW. After that, every

```bash
cargo build -p gain
```

swaps the new code into the running plugin between two audio blocks.
Parameter values and state carry over. Reload events are logged to
`%TEMP%\ferritron-hot-reload\<package>.log` (`$TMPDIR/ferritron-hot-reload/`
on macOS).

- Hot reload is compiled only into debug builds; release bundles call the
  plugin directly.
- If the parameter list changes, re-insert the plugin to see the new parameters.
- If plugin code panics, the plugin outputs silence. Fix the code and rebuild,
  and the reloaded code resumes processing.

To try it without a DAW, run the bundle in the minimal test host and rebuild
while it runs:

```bash
cargo run -p ferritron-testhost -- "target/bundled/Ferritron Gain.vst3" --seconds 60
```

### Validating

- VST3: build the `validator` target of the VST3 SDK with CMake, then run
  `validator -e "target/bundled/Ferritron Gain.vst3"`.
- CLAP: build [clap-validator](https://github.com/free-audio/clap-validator) and run
  `clap-validator validate "target/bundled/Ferritron Gain.clap"`. For very light
  plugins, its `process-audio-denormals` check can warn even though Ferritron
  enables flush-to-zero during processing: its timing includes the validator's
  own generation of denormal test data.

## Adding a plugin format

A format is one crate; neither Ferritron's core nor the CLI changes:

1. **Entry point**: export the symbol the host looks for and get the plugin
   from `ferritron_plugin_entry()` (a `ferritron_plugin_api` table, see
   `crates/ferritron-core/include/ferritron_abi.h`). Typically a small shim
   compiled against the format's SDK, which the developer provides.
2. **Packaging**: declare in the crate's `Cargo.toml`:

   ```toml
   [package.metadata.ferritron.format]
   name = "VST3"
   entry-symbol = "GetPluginFactory"                 # how bundle detects the format
   root = "{name}.vst3"                              # packaged and installed item
   binary = "{name}.vst3/Contents/{arch}/{name}.vst3"
   arch = { windows-x86_64 = "x86_64-win" }          # values for {arch}
   install = { windows = "{COMMONPROGRAMFILES}/VST3" }
   ```

3. **Linking**: built-in formats are features of the `ferritron` crate. A
   third-party format is linked from the plugin with `use ferritron_xyz as _;`.

`crates/ferritron-vst3` and `crates/ferritron-clap` are written exactly this way.

## Repository layout

| Path | Purpose |
|---|---|
| `crates/ferritron` | The crate plugins depend on (re-exports, prelude) |
| `crates/ferritron-core` | Format-independent API and the C ABI (`include/ferritron_abi.h`) |
| `crates/ferritron-macros` | `#[ferritron::plugin]`: reads the project file (`.ferritroject`), generates code |
| `crates/ferritron-vst3` | VST3 adapter: a C++ shim compiled against your VST3 SDK |
| `crates/ferritron-clap` | CLAP adapter: a C shim compiled against your CLAP headers |
| `crates/ferritron-au` | Audio Unit v2 adapter (macOS), in Rust |
| `crates/ferritron-web` | HTML/JS editor backend (WebView2 / WKWebView via wry) |
| `crates/ferritron-ara` | ARA 2: document controller on your ARA SDK, `AraRender` |
| `crates/ferritron-gpu-view` | Editors drawn with wgpu (`[view] backend = "gpu"`) |
| `crates/ferritron-cli` | `cargo ferritron bundle` |
| `crates/ferritron-build` | Helpers for build scripts: finding your SDKs, `FERRITRON_SKIP_NATIVE` |
| `tools/testhost` | Minimal VST3 host for testing without a DAW |
| `crates/ferritron-gpu` | GPU compute backend (`Convolver`, IIR banks, graph nodes; wgpu) |
| `crates/ferritron-slang` | Build-time Slang kernel compiler (WGSL + C++) |
| `bindings/csharp` | C# binding (`Ferritron.cs`) |
| `crates/ferritron-wasm` | Isolation Level 2: plugins in a wasmtime sandbox |
| `crates/ferritron-vulkan` | Native Vulkan: persistent kernels, Tensor cores (Slang shaders, SPIR-V kept) |
| `crates/ferritron-graph` | Node graph: latency compensation, automatic CPU/GPU placement |
| `examples/gpu-reverb` | GPU convolution reverb example |
| `examples/ara-reverse` | ARA 2 plugin that plays audio files backwards |
| `examples/gpu-view` | A gain whose editor is drawn on the GPU (wgpu) |
| `examples/slang-drive` | Saturator with a Slang kernel (CPU and GPU from one source) |
| `examples/gpu-chain` | Ferritron Chain: three modules in one graph, fused across their borders |
| `examples/cpp-gain` | A plugin written in C++ with `ferritron.hpp` |
| `examples/csharp-gain` | A plugin written in C# (NativeAOT companion library) |
| `examples/wasm-sandbox` | Isolation Level 2: `examples/wasm-crusher` in a WebAssembly sandbox |
| `examples/gain-match` | DirectLink: a gain-match pair around an effect |
| `examples/gpu-modal` | GPU modal synthesizer example (instrument, reports latency) |
| `tools/ferritron-mcp` | The MCP hub for AI clients (one per computer) |
| `crates/ferritron-manifest` | Project file (`.ferritroject`): location, parsing and new-project templates, shared by the macro, the CLI and the Ferritron Editor |
| `tools/bench` | Runtime benchmarks (parallel speedup, GPU convolution) |
| `tools/crash-fixture` | Plugin that crashes on demand, for testing process isolation |
| `examples/gain` | Minimal effect example |
| `examples/sine-synth` | Instrument example (polyphonic sine synth) |
| `docs/architecture.md` | Design document (Japanese) |

## Versions

All Ferritron crates share one version (currently **1.2.0**); see
[CHANGELOG.md](CHANGELOG.md). Pin it in a plugin with
`ferritron = { path = "...", version = "1.2.0" }`; `ferritron::VERSION` and
`cargo ferritron --version` report it.

## License

Ferritron is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in Ferritron, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

Plugin format SDKs (VST3, CLAP, ARA, Audio Unit) are not part of Ferritron:
developers download them and stay bound by their vendors' licenses.

VST is a trademark of Steinberg Media Technologies GmbH, registered in Europe
and other countries. Audio Unit is a trademark of Apple Inc. ARA is a
trademark of Celemony Software GmbH. Ferritron is not affiliated with or
endorsed by these companies.
