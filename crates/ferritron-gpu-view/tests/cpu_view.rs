//! The CPU mode of the view: an FNG editor draws into memory without a GPU (feature `fng`).
//! Needs no window and no GPU.
#![cfg(feature = "fng")]

use ferritron_core::__private::{Localized, PluginMeta};
use ferritron_core::isolation::{Isolation, ProcessOptions};
use ferritron_core::{AudioConfig, Context, ParamCell, ParamInfo, ParamSet, PluginInfo, PluginKind, Process};
use ferritron_gpu_view::{FngAdapter, FngEditor, Gpu, GpuEditor, Ui, fng};
use std::sync::OnceLock;

// ---- a plugin without parameters, written by hand (no macro) --------------------------------

#[derive(Default)]
struct NoParams;

impl ParamSet for NoParams {
    fn infos() -> &'static [ParamInfo] {
        &[]
    }
    fn cell(&self, _index: usize) -> &ParamCell {
        panic!("no parameters")
    }
    fn index_of(_id: u32) -> Option<usize> {
        None
    }
}

#[derive(Default)]
struct Plug;

impl Process for Plug {
    type Params = NoParams;
    fn process(&mut self, _ctx: &mut Context<'_, NoParams>) {}
}

static INFO: PluginInfo = PluginInfo {
    id: "test.cpu-view",
    name: "CPU view",
    vendor: "Ztraqto",
    version: "1.0.0",
    url: "",
    email: "",
    kind: PluginKind::Effect,
    class_id: [9; 16],
    isolation: Isolation::Guarded,
    process: ProcessOptions { per_instance: false, pipeline: false },
    vst3_subcategories: "",
    clap_features: "",
    vst2_id: 0,
    audio: AudioConfig::DEFAULT,
};

impl PluginMeta for Plug {
    const INFO: &'static PluginInfo = &INFO;
    fn localized() -> &'static Localized {
        static L: OnceLock<Localized> = OnceLock::new();
        L.get_or_init(|| Localized::new("test.cpu-view", &[], &[("en", "")]))
    }
}

// ---- editors -----------------------------------------------------------------------------------

/// A white rectangle at (10, 4) of 20 x 10 logical pixels on the default background.
struct Rect;

impl FngEditor for Rect {
    fn new(_ui: &Ui) -> Self {
        Rect
    }

    fn draw(&mut self, scene: &mut fng::Scene, _ui: &Ui) {
        scene.fill_rect(fng::Rect::new(10.0, 4.0, 20.0, 10.0), fng::Color::hex(0xffffff));
    }
}

/// An editor that needs the GPU: it has no CPU constructor.
struct GpuOnly;

impl GpuEditor for GpuOnly {
    fn new(_gpu: &Gpu, _ui: &Ui) -> Self {
        GpuOnly
    }

    fn render(&mut self, _gpu: &Gpu, _frame: &ferritron_gpu_view::Frame, _ui: &Ui) {}
}

/// The BGRA bytes of pixel (x, y) in a `width`-pixel-wide frame.
fn pixel(out: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * width + x) * 4;
    [out[i], out[i + 1], out[i + 2], out[i + 3]]
}

#[test]
fn a_cpu_frame_has_the_background_and_the_shapes() {
    let ui = Ui::for_tests::<Plug>(64.0, 32.0);
    let mut editor = <FngAdapter<Rect> as GpuEditor>::new_cpu(&ui).expect("an FNG editor draws on the CPU");
    let mut out = Vec::new();
    assert!(editor.render_cpu(&ui, (64, 32), 1.0, &mut out));
    assert_eq!(out.len(), 64 * 32 * 4);
    // Background 0x1b1c20 (R G B) in blue, green, red, alpha order, opaque.
    assert_eq!(pixel(&out, 64, 0, 0), [0x20, 0x1c, 0x1b, 0xff]);
    // Inside the rectangle: white.
    assert_eq!(pixel(&out, 64, 15, 8), [0xff, 0xff, 0xff, 0xff]);
}

#[test]
fn a_repeat_frame_of_the_same_size_keeps_the_buffer() {
    let ui = Ui::for_tests::<Plug>(64.0, 32.0);
    let mut editor = <FngAdapter<Rect> as GpuEditor>::new_cpu(&ui).unwrap();
    let mut out = Vec::new();
    assert!(editor.render_cpu(&ui, (64, 32), 1.0, &mut out));
    let first = out.as_ptr();
    assert!(editor.render_cpu(&ui, (64, 32), 1.0, &mut out));
    assert_eq!(out.as_ptr(), first, "a repeat frame of the same size must not allocate");
    assert_eq!(pixel(&out, 64, 0, 0), [0x20, 0x1c, 0x1b, 0xff]);
}

#[test]
fn an_editor_without_a_cpu_constructor_does_not_open_without_a_gpu() {
    let ui = Ui::for_tests::<Plug>(64.0, 32.0);
    assert!(<GpuOnly as GpuEditor>::new_cpu(&ui).is_none());
}
