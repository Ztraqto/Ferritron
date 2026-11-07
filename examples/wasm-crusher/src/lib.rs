//! A bit crusher, and the guest of `examples/wasm-sandbox` (isolation
//! Level 2): built for WebAssembly, it runs in a wasmtime sandbox inside the
//! host. Its `crash` and `hang` parameters break it on purpose: in the
//! sandbox only this plugin goes silent, and the host carries on.

use ferritron::prelude::*;

#[derive(Default)]
struct Crusher;

#[ferritron::plugin]
impl Process for Crusher {
    fn process(&mut self, ctx: &mut Context) {
        if ctx.params.crash.value() {
            // A real crash, not a panic.
            std::process::abort();
        }
        if ctx.params.hang.value() {
            // Never returns (the sandbox stops it after its time limit).
            #[allow(clippy::empty_loop)]
            loop {
                std::hint::black_box(());
            }
        }
        let steps = 2f32.powf(ctx.params.bits.value() - 1.0);
        ctx.io.for_each_sample(|s| *s = (*s * steps).round() / steps);
    }
}
