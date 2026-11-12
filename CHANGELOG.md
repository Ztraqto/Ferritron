# Changelog

All notable changes to Ferritron. Versions follow [Semantic Versioning](https://semver.org/):
every crate of the workspace shares one version (`[workspace.package]` in
`Cargo.toml`). A plugin can pin it with
`ferritron = { path = "...", version = "2.0.0" }`, and read it at run time as
`ferritron::VERSION`.

## Unreleased

- Sample-accurate automation ("sample" mode) no longer drops notes or fails the block when an event sits at the very end of the block.
- `ctx.changed()` / `changed_indices()` now also report a smoothed parameter while its smoothed value moves.
- A sleeping instance now wakes up when its sidechain input has sound.
- `ctx.transport()` now moves with each piece when Ferritron splits a block; new `ctx.block_offset()`.
- Preset values (`load_values`, `presets::load`) are what hosts read and save right away, not only after the next block.
- Loading a state now gives parameters it does not hold (added later) their default instead of keeping the current value.
- LV2: program changes and channel pressure (two-byte MIDI) reach the plugin.
- LV2: the transport position moves on between the host's position updates.
- A settings file that cannot be read is kept as `<name>.json.damaged-<time>` before it is written again.
- `on_notify` handlers of VST3, VST2 and AAX plugins always run on the host's main thread, even when it is busy for a while.
- GPU convolution: a tail block is no longer lost when the GPU thread holds its lock for a moment.
- GPU editors open again after a host unloads and reloads the plugin library.
- Web editors refuse to navigate away from the plugin's own pages and to open new windows.
- A panic in `migrate_state`, custom parameter text or scale functions, or an on_notify / on_message handler no longer reaches the host.
- VST3 and CLAP: when a block carries more events than the shim holds, notes are kept first and the extra parameter changes apply after the block in order.
- New: the C ABI has `reset` (at the end of the table); CLAP hosts' reset() now clears the plugin. C++ and C# headers updated.
- Saving state retries when the state grows between the size query and the copy, instead of saving zeros.
- `ctx.midi_out` keeps keys, controller numbers and values in 0..=127.
- A panic in a plugin's custom parameter scale no longer reaches the host from the wake-up check, the editor, or the instance handle's conversions.
- After the built-in bypass is turned off, the plugin is reset before it runs again: notes held across the bypass stop and old delay contents do not come back.
- Ferritron's notification and watch threads keep their DLL loaded, so a host that unloads the plugin while they sleep does not crash.
- All Notes Off and All Sound Off (CC 123, 120) release held notes for the silence check, so an instrument can sleep again.
- CLAP: the plugin reports the end of each voice (`CLAP_EVENT_NOTE_END`) when its note-off arrives, so hosts release per-voice modulation; `Process::REPORTS_VOICE_END` and `ctx.voice_ended` let a plugin report it when its release tail ends. NOTE_CHOKE arrives as a note-off. Overlapping notes on one key are no longer claimed.
- `VoiceMods` keeps a released voice's offsets until `end()`; a full table reuses released voices. A note ID alone reaches only the voice with that note ID.
- MIDI and parameter events a plugin sends with `midi_out` and `set_param` are placed at their offset in the host's block, not in the piece.
- A data stream written from two threads at once drops the second write instead of corrupting the buffer.
- MCP: removing an instance waits for a request the hub is serving on it, instead of letting it read freed memory. The capture follows sample rate changes and old WAV files are removed.
- MCP: the hub and the plugins prove that they hold the token (HMAC challenge and response) instead of sending it, so a program waiting on the port cannot take it. The token file is readable only by its owner on Unix.
- Isolation waits up to 30 s per block while the host renders offline and 5 s for the first block after activation; pipeline mode no longer allocates when a block is longer than the latency. (The host's offline flag now reaches the isolation client.)
- WebAssembly isolation: the guest is called by one thread at a time; parameter reads come from a host-side copy and edits reach the guest at the next block.
- WebAssembly isolation: a state larger than the guest's scratch area is not saved as zeros; the shims then save an empty state and a message goes to stderr.
- VST3: the main-thread poll skips instances the host destroyed during it, and on Windows a latency change found in setActive is reported on the next poll instead of from inside setActive.
- Web editors pause their refresh while a file dialog is open.
- Parameter IDs are checked against the program parameter and the VST3 MIDI controller range too.
- Documented the limits of [vst3] controller_id, LV2 presets, the standalone app and VST3 on Linux.
- wasm-sandbox: the WebAssembly guest builds again on Windows; a failed guest build now fails the build instead of embedding an empty module.
- WebAssembly plugins: the host passes the complete plugin table (reset), so the VST3 shim accepts them.
- MCP: a request line split across the 200 ms read timeout is no longer cut in two.
- Isolation: the child process knows when the host renders offline, so ctx.quality() is Best there too.
- WebAssembly isolation: parameter changes from the host's automation and from the guest update the host-side copy, so the controller and saved state stay in sync.
- WebAssembly isolation: the guest's output events (`ctx.set_param`, `ctx.midi_out`, voice ends) reach the host, and the host-side parameter copy follows the guest's own edits.
- Process isolation: the plugin's output events (`ctx.set_param`, `ctx.midi_out`, voice ends) come back from the child process to the host.
- VST2: the plugin's MIDI output (`audioMasterProcessEvents`) and its own parameter edits (`audioMasterAutomate`) reach the host. Not tested in a DAW.
- LV2: plugins have a MIDI output port (midi_out) that carries ctx.midi_out.
- LV2: a preset chosen inside the plugin survives saving and reopening the project; the host's port values no longer overwrite the restored state.
- LV2: the recorded port values are kept across deactivate and activate, so a host that re-sends unchanged values does not overwrite the plugin's state.
- Standalone: `ctx.midi_out` is sent on Windows and Linux to the MIDI output named in `standalone/midi-out.txt` (or the `FERRITRON_MIDI_OUT` variable); the names found are listed in `standalone/midi-out-ports.txt`.
- AU: MIDI output through `kAudioUnitProperty_MIDIOutputCallback`, and parameter-change notifications for the plugin's own edits. Type-checked only; not run on a Mac.
- AAX: the plugin's own parameter edits reach Pro Tools from the host timer. Compiled and linked against the AAX SDK 2.10; not run in Pro Tools.
- VST3 `[vst3] controller_id`: the controller's own instance is hidden from instances() and MCP, and the editor attaches to the processor's instance when the host connects them in one process.
- Windows GPU editors survive a lost GPU device: the shared device is replaced after a loss or an uncaptured wgpu error, and the editor is opened again on it (not tested on real device loss).
- A panic in an editor's window code stops that view from drawing instead of unwinding out of the host's window procedure.
- Input for a GPU editor no longer draws inside the input message; the frame is drawn by the next check, within one frame.
- Japanese and other IME text reaches GPU editors that take text, and the composition window sits at the editor's caret (`Ui::set_text_input`; not tested with a real IME).
- Losing the mouse capture while a button is held releases the button for the editor and frees a pointer lock.
- GPU editors read the refresh rate of the display their window is on, and surfaces are limited to the device's maximum texture size.
- The GPU convolver restarts its input stream when the GPU falls behind, instead of leaving the tail off for good; `Convolver::restarts()` counts the restarts. Not tested on a real stall.
- A GPU round that does not finish within two seconds, or a device call that panics, marks the convolver as lost: it then passes its input through (`Convolver::device_lost()`). Not tested on a real device loss.
- `Convolver::process` and `Graph::process` process blocks longer than `max_block` in pieces, so no frame is left unprocessed.
- The GPU thread sleeps until work arrives instead of waking every millisecond, and a thread that ended by panic is replaced by the next convolver or bank.
- `choose_keyed` reuses a placement by configuration key and accepts a fixed placement; the GPU nodes do not use it yet.
- Vulkan records a lost device and fence waits are bounded to two seconds; mixers and persistent modal banks then output silence. Not tested on a real device loss.
- The FNG mask atlas evicts its least recently used shelves when full instead of starting the frame over; masks past a frame's budget are kept for that frame only. GPU behaviour was not tested on a device that runs out of atlas space (not tested on real device loss either).
- The FNG CPU renderer keeps its buffers between frames, has `render_into` for caller-owned output in `Rgba8` or `Bgra8Premultiplied`, and refuses pictures larger than 16384 pixels a side instead of truncating them.
- Opacity groups in FNG composite only the part they drew; blurred shadows compute the flat interior once and cache their coverage per shape.
- FNG glyph masks (upright and outlined) share a 32 MB budget and are evicted oldest first; outlined glyphs are cached too.
- Layers in FNG are drawn at the zoom of the transform they are placed with (capped at 4096 pixels a side), so a zoomed layer is sharp on both backends.
- `Scene::push_clip_rect` documents that rectangle clips round to whole device pixels.
- The FNG editors (`FngAdapter`) are drawn on the CPU in the window when no GPU device can present to it or the device stays lost after three attempts (Windows only); the view tries the GPU again every five seconds. Setting `FERRITRON_GPU_VIEW=cpu` takes the CPU mode at once. Editors that need the GPU still do not open without one. The GPU path on device loss is not yet verified on a real device loss.
- The GPU convolver is built again on a new device after the GPU is lost (simulated by hand; not tested on a real device loss); `Convolver::rebuilds()` and `device_generation()` report it. Its input stream starts again from the current position.
- The shared Vulkan device is replaced after it is lost, at most once per five seconds while that fails; `PersistentModalBank::revive` and `TensorMixer::revive` build the bank or mixer again. Not tested on a real device loss.
- `Graph::placement` reports where a node runs and `Graph::set_placement` fixes it (rebuilding the node and preparing the graph again). GPU nodes remember their placement per configuration.
- A GPU node whose device is lost (`IirNode`, `KernelNode`, `MatrixMixerNode`) runs its CPU candidate from the next control period; the candidate starts empty, so filter and delay contents are not carried over. `Graph::take_latency_changed()` reports a latency change. Tested with hand-set loss flags; a real device loss was not tested.
- `Graph::prepare` calls each node's `revive`: a lost persistent kernel or Tensor-core mixer is built again (`PersistentModalBank::revive`, `TensorMixer::revive`) and the node returns to the GPU when that works. `ConvolverNode` has no CPU candidate and builds itself again on its own GPU thread. No retry every five seconds for the first two. Not tested on a real device loss.
- After the GPU is lost, the shared device is reopened on the next use, and every later call gets the same device; devices it replaced are kept until the process ends. Once the wgpu side has handed a device over, the Vulkan side does not open a device of its own after a loss; before any hand-over it still does. Not tested on a real device loss.
- A convolution called with a block longer than its `max_block` still processes the whole block. The first such call prints a warning from the GPU thread; the tail blocks the GPU cannot deliver in real time are counted by `missed_blocks()`. Build the convolver with a `max_block` at least as large as the host's block to avoid this. (Measured at real-time pace: 1024-sample calls with `max_block` 256 missed 5 of 300 calls in 6.4 s; with `max_block` 1024 none in a 2 s run.)
- FNG keys each path and glyph mask without its whole-pixel position or the target size, so one mask serves every whole-pixel position of a shape and survives a resized target; the offset is rounded to a quarter pixel (up to an eighth of a pixel shift), and shapes wider than 4000 pixels keep the old key.
- FNG keeps layer textures within the device's largest texture side by drawing the layer smaller and stretching it back when composited, so it is blurred rather than rejected; group textures are sized to their parent and are not limited separately.
- FNG images get a mip chain when they are stored, and both backends sample it by the same level of detail, so a large image drawn small no longer shimmers (nearest sampling still uses the full-size image).
- The FNG stroker keeps its buffers in the renderer, so a dashed or round-capped stroke no longer allocates each time it is drawn on the CPU backend.
- FNG CPU opacity groups store only the part they draw and give back unused buffers after two pictures; in a 2560 by 1440 test picture with nested groups the peak working set fell from about 140 MB to about 76 MB (measured, Windows).
- The GPU layer zoom test leaves out pixels whose alpha is 2/255 or less from its count of color differences, because straight color is noise there (the coverage rounding of the two backends still differs at those pixels; not fixed at its source).
- AAX: MIDI output through an output MIDI node. Compiled and linked against the AAX SDK 2.10; not run in Pro Tools.
- `cargo ferritron bundle --features` also packages formats that are off by default (VST 2, AAX) for plugins outside the Ferritron workspace; they were built but not packaged (`docs/external-fixes.md` X1).
- New: `[bundle]` in the project file (`install`, `shortcuts`, `shortcut_prefix`) and `bundle --shortcuts` / `--no-shortcuts` / `--no-install`: install after every bundle, and shortcuts to each format's install folder next to the bundles (`_VST3 Plugins.lnk`, ...; symbolic links outside Windows). VST 2 has an install folder (`{VSTPLUGINS}`: the registry's `VSTPluginsPath`, else `%COMMONPROGRAMFILES%\VST2`). `--install` skips formats without an install folder (the standalone app) instead of failing (`docs/external-fixes.md` X2).

## 2.0.0 — 2026-11-10

**Ferritron 1.4.x and older are deprecated.** New plugins and fixes target 2.0.0. The plan and the
item-by-item results are in `release/plans/` (not exported); measurements are in
`docs/performance.md`. Everything marked as run was run on Windows (i7-10700KF, Windows 10);
formats and platforms that were not run are named below.

### Moving up from 1.4.x

Details and a conversion table: `docs/migration-2.0.md`.

1. **Automation is applied per control period** (32 samples, `[process] automation = "control"`).
   `"sample"` restores 1.x timing; `"events"` hands the changes to the plugin
   (`ctx.param_events()`). Float parameters can glide with `smooth = "20ms"`.
2. **`Context` fields are methods**: `ctx.params()`, `ctx.io()`, `ctx.notes()`, `ctx.midi()`,
   `ctx.sample_rate()`, `ctx.quality()`, `ctx.transport()`. `Context` is `#[non_exhaustive]`.
3. **C ABI v2**: `process` takes a `ferritron_process_data*` (C++ and C# bindings moved).
4. **The built-in bypass** crossfades over 10 ms and delays the dry signal by the plugin's
   latency; while fully bypassed `process` is not called.
5. **GPU editors**: `gpu_view::Input` is `#[non_exhaustive]`; pointer events carry `modifiers`,
   `Wheel` has `delta_x`.
6. `FloatParam::value()` is the smoothed value; `target()` is the host's.
7. **`.frtx` project files are no longer read.** `cargo ferritron migrate-frtx <package>` renames
   them to `.ferritroject`.
8. **Windows VST3: explicit GUIDs show as written.** `[vst3] class_id`, `controller_id` and
   `compatibility.id` are stored in COM order on Windows, so hosts show them exactly as written
   (before, the first three fields appeared byte-swapped). Plugins that shipped with an explicit
   `class_id` list the old ID, as hosts showed it, under `compatibility.replaces`
   (`examples/multiband-test` does). IDs derived from `plugin.id` do not change.

9. **New fields** in public structs that code may build by hand: `TrackInfo` (`kind`, `location`,
   `channels`), `voice::Expression` (`tuning`, `volume`, `pan`, `vibrato`, `expression`; its
   default is now neutral: volume 1, pan 0.5), `ParamOptions` (`poly`); C: `ferritron_track_info`
   grew three fields and `ferritron_event` has a `voice` member (the event's size is unchanged).
   Add `..Default::default()` where you build them.
10. **Voice ends**: Ferritron reports the end of each CLAP voice (`NOTE_END`) when its note-off arrives.
    A plugin with a release tail sets `Process::REPORTS_VOICE_END` and reports the end itself with
    `ctx.voice_ended`. `VoiceMods` keeps a released voice's modulation until `end(id)` is called.
11. **`ctx.midi_out` and `ctx.set_param` events** are placed at their offset in the host's block, not in the
    piece Ferritron split the block into.

Parameter keys, parameter IDs, class IDs derived from `plugin.id` and saved states stay compatible:
states saved by 1.4.0 for 13 examples are kept in `tests/fixtures/1.4.0/` and loaded by tests
(`ferritron::testing::check_fixture`).

### Added

- **Parameter flow**: `[process]` (`automation`, `control_block`), `smooth` (linear, log, exp),
  `ctx.ramp`, `ctx.changed` / `any_changed` / `changed_indices`, `ctx.set_param` and `ctx.midi_out`
  (output events; VST3 run in a test host, CLAP built and not run), repeated parameters and groups
  (`[params.<group>] repeat = N`, `ctx.params().band[i].freq`), display formats (`display`,
  `format`, `digits`), factory presets (`[presets.<name>]`, `ferritron::presets`), polyphonic
  modulation in the core (`VoiceMods`, `ctx.value_for`), `Process::STATE_VERSION` and `migrate_state`.
- **Threads and memory**: `Process::scratch` and `ctx.scratch` (lent without allocating), workers
  (`#[ferritron::plugin(worker = ...)]`), `[streams]` and analysis taps (`examples/analyzer`),
  `InstanceHandle::attach` / `shared` / `is_editor_open`, `util::Swap`, `util::CachePadded`.
- **Settings across projects**: `ferritron::settings::<P>()` / `settings_for(id)` — one JSON file per
  plugin ID, safe for several processes, change callbacks. The web editor's `ferritron.settings`
  uses the same file inside a plugin.
- **`ferritron-dsp`** (`ferritron::dsp`, feature `dsp`, on by default): SIMD with run-time ISA choice,
  biquads (4 and 8 at once), delay lines, smoothing, sliding min/max, FFT, partitioned convolution,
  oversampling, dither and more. AVX2 and SSE2 run here; AVX-512 and NEON not run.
- **Ferritron Native Graphics** (`crates/ferritron-native-graphics`) and `FngEditor` / `FngAdapter`
  (feature `fng` of `ferritron-gpu-view`): drawn in a real VST3 window on Windows. macOS, iOS, Linux,
  Android and Web: type-checked or not built; not run.
- **Editors**: GPU editor frame pacing (no redraw while idle, DWM refresh), pointer lock, theme
  changes, `request_resize` and `scale_factor` (VST3); web editor theme, settings, a context menu of
  its own and file dialogs (Windows). The VST2 adapter gets editors (test host; not in a DAW).
- **VST3**: `[vst3] controller_id`, `compatibility` (`IRemapParamID` for the 1.3.0 built-in
  parameter IDs) and `moduleinfo.json`.
- **Notifications to the main thread**: `InstanceHandle::notify(code)` / `ctx.notify(code)` from any
  thread, the audio thread included (lock-free, no allocation; 64 codes wait at most, a code already
  waiting is not queued twice, overflow is counted), and `on_notify(|code, handle| ..)` on the main
  thread: delivered by the shims' main-thread poll (VST3, VST2, AAX every 30 ms; CLAP through
  `request_callback`), by a Ferritron thread for formats without one (LV2, standalone, AU, WAM),
  inside the plugin's process under isolation.
- **Presets in the formats**: plugins with `[presets]` get a hidden built-in "Program" parameter that
  becomes the VST3 program list (`IUnitInfo`, `kIsProgramChange`), CLAP `preset-load` and preset
  discovery, and VST2 programs; choosing one loads it on the main thread. User presets:
  `presets::save_user` / `user_list` / `load_user` / `delete_user` (JSON by parameter key in
  Ferritron's data folder), `presets::current`.
- **Per-note modulation from the host**: `poly = true` on a float parameter makes it modulatable per
  note in CLAP (`PARAM_MOD`, per note ID / key / channel, and for every note); CLAP note expressions and
  VST3 Note Expression (volume, pan, tuning, vibrato, expression, brightness) for instruments that
  set `Process::VOICES`, which CLAP `voice-info` reports. The plugin reads `ctx.voice_events()`;
  `VoiceMods::feed_note` / `feed_voice` turn them into per-voice offsets and `Expression`.
- **Watching instances and editors**: `instances_watch(|event| ..)` (added, removed, changed, parameters
  moved, editor opened / closed / focused) on one Ferritron thread for the whole process,
  `RedrawHandle::wake_on`, `editors::<P>()` in opening order, `SingletonView` (one editor does the
  shared work and hands it on), `InstanceHandle::params_revision`.
- **More of the mixer**: `TrackInfo::kind` (track, return, bus, master; CLAP), `location` (before or
  after the fader, panner; VST3) and `channels` (CLAP). No format tells mute, solo or the fader
  (`docs/host-mixer-info.md`).
- **Testing and measuring**: `tools/bench-fx` (`--json`, `--compare`, `--golden`), `AllocGuard`
  (feature `alloc-check`), and on `Harness`: `assert_no_alloc`, `measure_latency`, parameter access
  by index and ID, `flags`, `host_latency`, `load_values`, `set_automation_mode`, `process_events`,
  `param_event`, `memory_report`; `testing::check_latency`, `check_fixture`, `time_blocks`.
  `cargo ferritron check` (reported vs measured latency), `bundle --size`, `bundle --pgo` (steps
  written; not run: `llvm-tools` not at hand). `ferritron-testhost --bench N` times blocks through
  the VST3 / VST2 shim (S9) and, with `FERRITRON_ISOLATION=process`, through isolation (S7).

### Changed

- Dense automation: `process` runs once per control period instead of once per point, and only
  the last value of each parameter in a period is looked up and converted
  (8 parameters automated every sample, 512-sample blocks: 512 → 16 calls, 142.3 → 67.2 µs).
- The per-block cost without automation is the same as 1.4.0 (0.25 µs vs 0.29 µs at 512 samples).
- Process isolation maps shared memory by the real channel count and block length
  (about 41 KB per stereo instance at 512 samples, 2.1 MB before).
- Every adapter sorts events the same way: stable, O(n log n), no allocation (the LV2, AU, AAX and
  VST2 adapters allocated or sorted in O(n²) before; the LV2 adapter also allocated every block).
- Framework memory per instance is about 1.6 KB (1.4.0: about 1 KB). The bypass crossfade's copy of
  the input comes from a pool of four buffers shared by the process; with more than four instances
  crossfading in the same moment, the others switch without a fade.

### Verified and not verified

- Run on Windows: `cargo test --workspace`, the VST3 test host (`ferritron-testhost`), LV2 through
  its test host, the standalone app, isolation (including recovery from a real crash).
- Official checks: Steinberg's VST3 `validator` passes all 537 tests for every example (24); `clap-validator`
  0.4.1 reports no failure for 20 of 23 examples. The other three do what they demonstrate: `midi-out` moves
  its own parameter every block (3 `param-set-*` tests), `multiband-test` starts at an `initial` value that
  differs from its default (`param-default-values`), and `wasm-crusher`, the guest meant to run inside the
  WebAssembly sandbox (`wasm-sandbox`, no failure), really crashes or hangs when its `crash` / `hang`
  parameters are set (6 tests; the same before the changes of this release). clap-validator's preset
  discovery `crawl` / `load` tests were left out: 0.4.1 deadlocks on any plugin with two or more presets. Many examples get a warning that denormal input is
  2.7–3× slower; `Harness` measures 1.22× for the same input.
- C# (NativeAOT) and C++ examples built and run in the VST3 test host and both validators.
- AAX: built against AAX SDK 2.10.0, not run (no Pro Tools). Type-checked only: macOS, iOS, AU, AUv3.
- DAW: FL Studio 2026's plugin scan loads the VST2 build and reports it "ok". The VST3 and CLAP builds
  were not inserted and played in a DAW on 2.0.0 (only the official validators above).

### Fixed while validating

- The worker thread could outlive the plugin library: it is now joined when the last worker goes.
- CLAP: `clap_host_latency::changed` is called only inside `activate`; ports whose channel count has no
  speaker layout are no longer typed `surround`.
- C# binding: the plugin info struct had not been extended in 1.3.1 (CLAP read past it); NativeAOT finds the
  linker through `vswhere.exe` even when the Visual Studio installer is not on `PATH`.
- WebAssembly isolation (Level 2) had been left on C ABI v1 and did not load; it now speaks v2.
- `[companion]` in project files (`examples/csharp-gain`) was rejected by the project file check.

- Not run in a DAW: notifications, the formats' preset lists, per-note modulation and Note Expression,
  watching instances and editors (all checked with `Harness` tests and the official validators).

### Left for a later version

Host context menus, full screen, resizing in formats other than VST3, AudioSuite,
a 64-bit float audio path, the web editor's preset menu (`ferritron.presets`) and
focus report, LV2 presets, `poly` parameters as VST3 custom note expressions, mute / solo / fader of the
track (no format offers them). Reasons are in the results table.

## 1.4.0 — 2026-11-09

**Ferritron 1.3.0 and older are deprecated.** They get no fixes; `cargo ferritron bundle`
warns when a plugin is built on one. New work starts on 1.4.0.

This release was planned as 1.3.1 (`release/plans/ferritron-1.3.1-plan.md`); the change of the
reserved parameter IDs and of the built-in bypass makes it a minor version. The plan's items
that are not done are listed at the end of this section and move to 1.4.x.

### Moving up from 1.3.0 and older

1. Set `version = "1.4.0"` of `ferritron` and install the matching `cargo-ferritron`
   (`cargo install --path crates/ferritron-cli`): the plugin and the tool
   should be the same version.
2. **Reserved parameter IDs moved** to `0x40000000` and up (Bypass `0x40000000`, Quality
   +1, AI Access +2, instance and editor-size state +3..+6). Saved state keeps working
   (values are stored by key); **host automation and MIDI mappings of the built-in
   Bypass / Quality / AI Access written by 1.3.0 no longer connect.** Your own parameters
   keep their IDs. Pin one with `id = <n>` if you need a fixed value.
3. `PluginInfo` and `Setup` (public structs) have new fields, and `Context` has new ones:
   code that builds them by hand needs `..` or the new fields; plugins that use
   `#[ferritron::plugin]` are not affected.
4. Everything else is added behind new project-file keys: a plugin that does not use them
   keeps its parameter IDs, class IDs, bus layout and LV2 ports.
5. The built-in bypass is **not** yet the delay-line / crossfade version the plan wanted;
   it behaves as in 1.3.0 (P8 is not done).

### Added (verified on Windows with unit tests; formats as noted)

- **P1 explicit parameter IDs** — `id = <n>` (0 to 0x3FFFFFFF); omitted
  keeps the 1.3.0 hash. Duplicates and clashes with Ferritron's own IDs are
  compile errors. **Reserved IDs moved** to fixed values from `0x40000000`
  (Bypass `0x40000000`, Quality +1, AI Access +2, instance and editor-size
  state +3..+6). *Effect on existing projects:* host automation and MIDI
  mappings of the built-in Bypass / Quality / AI Access written by 1.3.0 no
  longer connect (their values are restored from the saved state by key). No
  VST3 `IRemapParamID` remap yet.
- **P3 custom text** — `text = "custom"` plus `Process::format_param_text` /
  `parse_param_text` (`ParsedText::{Value, Reject, Default}`); every host path
  uses them. **P4 scales** — `scale = "log" | { skew_center = v } | "custom"`
  (`Process::param_normalize` / `param_denormalize`); stored values stay plain.
  Ranges stay finite: show `-INF` through custom text. (Differs from the plan:
  the hooks are methods of `Process`, not a separate trait.)
- **P5** `[vst3] class_id` (GUID). `controller_id` / `compatibility` not yet.
- **P6** `[plugin] categories`, `[vst3] subcategories`, `[clap] features`.
- **P7** `initial` (new instance value; the host default stays `default`).
- **P9** `ctx.request_latency(n)`; VST3 `restartComponent(kLatencyChanged)`,
  CLAP `request_restart` + `latency.changed`. `Setup::latency`.
- **P10** `InstanceHandle::load_values` / `load_values_by_key`: applied
  together at the start of a block, no automation; VST3
  `kParamValuesChanged` + `setDirty`, CLAP `rescan(VALUES)`.
- **B4** `ctx.transport` (tempo, time signature, play state, beats, bar start,
  samples, loop) from VST3 and CLAP. Not carried across `[isolation]` yet.
- **B1 sidechain** — `[audio] sidechain = { channels = "main" | n }`,
  `ctx.sidechain()` (mono reads as L = R; `None` when not connected). VST3
  `kAux` bus (inactive until the host enables it), CLAP second input port.
- **B2 channels** — `[audio] channels = { min, max }` (up to 32) and `layouts`
  (names restrict the range), `Setup::layout` (`Speaker` per channel; VST3
  arrangements, CLAP default layouts by channel count). `AudioIo` and the ABI
  carry 32 channels. `unsupported = "passthrough"` is parsed but shims still
  refuse out-of-range layouts. CLAP does not use the `surround` extension yet,
  so its speakers come from the channel count.
- **B3 MIDI input** — `[plugin] midi_input = true` for effects;
  `ctx.midi` (`MidiKind`: notes, CC, pitch bend, channel and poly pressure,
  program change; `ctx.notes` stays). VST3: `IMidiMapping` with hidden
  parameters (ids from `0x40001000`, 16 channels x 131), legacy CC and poly
  pressure events; CLAP: all `CLAP_EVENT_MIDI` messages and pressure
  expressions. Only effects with `midi_input` get the hidden parameters.
- **Isolation** — sidechain, MIDI, speaker layout, transport and main-thread
  requests (latency) cross `[isolation] mode = "process"` through the shared
  memory (32 channels; about 2 MB per instance). Checked with the test host:
  the isolated plugin gives the same output as the in-process one.
- **LV2** — `lv2_port_plan` (one list for the Turtle file and the adapter):
  one port per channel, sidechain inputs, a MIDI atom port, a transport port
  (`time:Position`) for plugins that use the 1.3.0-new audio features. Plugins
  that use none keep exactly the 1.3.0 ports. Checked with a small LV2 host in
  `examples/multiband-test`.
- **VST 2** (`ferritron-vst2`, feature `vst2`, off by default, Windows tested)
  — `VST2_SDK_DIR`; **never fetched or shipped** (Steinberg no longer licenses
  it). Parameters, text, chunks, MIDI, transport, sidechain (the inputs after the
  main ones), up to 32 channels (`effSetSpeakerArrangement`), `effSetBypass`,
  latency changes (`audioMasterNeedIdle` -> idle -> `ioChanged`), `[vst2] unique_id`.
  **No editor yet.** Checked with a small VST 2 host in `examples/multiband-test`
  (`--features vst2-test`, needs your SDK); not tried in a DAW.
- **AAX** (compiled and linked with the real AAX SDK in this release; the shim built
  without changes; the ACF entry points are exported) — all MIDI messages,
  the transport (tempo, meter, play state, ticks, samples, loop), one
  registration per stem format inside the channel range (the default 1..2
  range still registers only stereo), latency and bulk value changes from the
  host timer. **Not run in Pro Tools** (no validator or Pro Tools here; PACE
  signing is needed anyway). **Sidechain**: AAX has one mono sidechain input,
  read as L = R; registered by repeating `StaticDescribe`'s work in the shim
  (it has no sidechain field) on an unused slot of the render info. Needs
  `AAX_SDK_DIR`; `Extensions/Topology` is now compiled in. AudioSuite is not covered.
- `cargo ferritron bundle <package> --features <list>` turns on formats that are
  off by default (VST 2 and AAX), through a feature of the plugin.
- **AU** (written, type-checked with `cargo check --target aarch64-apple-darwin`,
  never run on a Mac) — all MIDI messages and the host transport. No more than
  two channels, no sidechain, no latency notification yet.
- `unsupported = "passthrough"` (VST3, CLAP), CLAP `surround` extension,
  `[midi] vst3_cc = "host"`.
- **Ferritron Editor**: parameters keep and edit `id`,
  `initial`, `scale`, `text`, `automatable`, `hidden` (an update no longer drops
  them); `SetOption` for `plugin.midi_input` / `categories`, `[audio]`, `[midi]`,
  `[vst3]`, `[vst2]`, `[clap]`; `NewGuid`; `Describe` returns the new fields and
  `options`. The CLI has `option` and `guid` and `param --param-id/--initial/--scale/--text`.
  GUI: an "Audio & Formats" view and an Advanced section in the parameter sheet.
  **New look** — the Editor's own: a dark rack sidebar with a lit copper bar, warm
  paper and graphite surfaces over a faint measurement grid, square-cornered
  hardware-style controls, a monospaced voice for labels and keys, new accent
  colours (Copper, Verdigris, Indigo, Magenta, Moss, Steel; older choices fall back
  to Copper). Behaviour stays as the Human Interface Guidelines describe it
  (keyboard use, focus rings, Reduce Motion, light and dark, contrast).
  Checked in a browser with a stand-in for the Tauri bridge, and the Tauri app builds;
  not run as a window here.
- **R1** `AudioIo::channels_mut()` and `split_mut::<N>()`.
- **Testing** `Harness`: `process_with_sidechain`, `midi`, `set_layout`,
  `reactivate`, `host_param`, `normalized`, `set_normalized`, `text`,
  `parse_text`, `set_transport`, `take_main_requests`.

Not covered by the above: AUv3 does not use sidechain, MIDI, layouts or transport yet;
AU has no sidechain, no more than two channels and no latency notification yet. 

**Not done yet (moved to 1.4.x):** P2, P8, B5, R2–R5, E1–E13 (including the VST 2
editor), FNG (E3), F2, T1, N1–N6, and the docs for the finished items.

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
