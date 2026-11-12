//! Ferritron DSP building blocks (X5 and X7 of the 2.0 plan).
//!
//! No dependencies besides `std` and `core::arch`; independent of `ferritron-core`.
//! Rules for every part: `new` / `prepare` allocate, the per-sample and per-block
//! methods never allocate and never lock. Recursive state is flushed to zero below
//! [`DENORMAL_FLOOR`] so the result does not depend on the CPU's FTZ/DAZ state.

#[cfg(test)]
mod bench;
pub mod biquad;
pub mod crossfade;
pub mod delay;
pub mod design;
pub mod dither;
pub mod fastmath;
pub mod fft;
pub mod isa;
pub mod lookahead;
pub mod lut;
pub mod multiversion;
pub mod oversampler;
pub mod shaper;
pub mod shared;
pub mod sliding;
pub mod smoother;
pub mod simd;

pub use biquad::{Biquad, BiquadX4, BiquadX8, Coefs};
pub use crossfade::{Crossfade, FadeLaw};
pub use delay::DelayLine;
pub use dither::Dither;
pub use isa::{Isa, isa};
pub use fft::{Fft, PartitionedConvolver};
pub use lookahead::Lookahead;
pub use lut::Lut;
pub use oversampler::Oversampler;
pub use shaper::{Shaper, ShaperKind};
pub use shared::{Cached, shared};
pub use sliding::{PeakHold, SlidingMax, SlidingMin};
pub use smoother::{SmoothKind, Smoother};
pub use simd::{F32x4, F32x8, F64x2, Mask2, Mask4, Mask8};

/// Values of recursive state below this magnitude are set to zero (the same on every ISA).
pub const DENORMAL_FLOOR: f32 = 1e-20;

/// Sets `x` to zero when its magnitude is below [`DENORMAL_FLOOR`].
#[inline(always)]
pub fn flush_denormal(x: f32) -> f32 {
    if x.abs() < DENORMAL_FLOOR { 0.0 } else { x }
}
