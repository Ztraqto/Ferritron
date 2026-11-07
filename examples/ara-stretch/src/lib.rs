//! ARA 2 example: the framework's defaults. The host can stretch regions in
//! time (the pitch stays) and gets the notes of every audio file, found by
//! Ferritron's monophonic pitch tracker. The plugin itself only applies gain.

use ferritron::ara::AraRender;
use ferritron::prelude::*;

#[derive(Default)]
struct Stretch;

#[ferritron::plugin]
impl Process for Stretch {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
    }
}

impl AraRender for Stretch {
    // Time stretch is on by default; the notes are opt-in.
    const ANALYZES_NOTES: bool = true;
}
