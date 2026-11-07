//! Passes audio through, and crashes the process (an access violation, not a
//! panic) when its `crash` parameter is on. With `[isolation] mode = "process"`
//! the host must survive that and the plugin must come back.

use ferritron::prelude::*;

#[derive(Default)]
struct CrashFixture;

#[ferritron::plugin]
impl Process for CrashFixture {
    fn process(&mut self, ctx: &mut Context) {
        if ctx.params.crash.value() {
            // A real crash: Rust's panic guard cannot catch this.
            unsafe { std::ptr::null_mut::<u32>().write_volatile(0xDEAD) };
        }
        let level = ctx.params.level.value();
        ctx.io.for_each_sample(|s| *s *= level);
    }
}
