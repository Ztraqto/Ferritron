//! ARA 2 example: plays the audio files backwards (each region plays the
//! mirrored part of its file). Only possible with random access to the whole
//! file, which ARA gives. The gain then applies like in any effect.

use ferritron::ara::{AraRender, Source};
use ferritron::prelude::*;

#[derive(Default)]
struct Reverse;

#[ferritron::plugin]
impl Process for Reverse {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params().gain.value());
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

impl AraRender for Reverse {
    // A reversed rendering cannot follow a stretched timeline.
    const TIME_STRETCH: bool = false;

    fn render_region(&mut self, params: &Params, source: &Source<'_>, start: i64, out: &mut [&mut [f32]]) {
        if !params.reverse.value() {
            return source.copy_to(start, out);
        }
        // Sample `start + i` of the region plays the sample mirrored around
        // the middle of the source.
        let last = source.len() as i64 - 1;
        for (c, out) in out.iter_mut().enumerate() {
            let input = source.channel(c.min(source.channels().saturating_sub(1)));
            for (i, s) in out.iter_mut().enumerate() {
                let at = last - (start + i as i64);
                *s = if (0..=last).contains(&at) { input[at as usize] } else { 0.0 };
            }
        }
    }
}

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<Reverse>(include_str!("../../../tests/fixtures/1.4.0/ara-reverse.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}
