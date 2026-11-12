//! Ferritron Native Graphics (FNG): a small, general 2D drawing library.
//!
//! You record a [`Scene`] every frame (immediate mode) and draw it with a [`CpuRenderer`]
//! (reference pictures, screenshots) or a wgpu [`Renderer`]. FNG knows nothing about audio,
//! parameters or windows, and depends on no other Ferritron crate.
//!
//! Coordinates are logical pixels; the pixel scale (DPI) is given when drawing.

#[cfg(feature = "wgpu")]
mod atlas;
#[cfg(feature = "cpu")]
pub mod cpu;
pub mod font;
pub mod fonts;
pub mod geom;
// Shared with the backends; some items are unused when no backend feature is on.
#[allow(dead_code)]
mod glyph;
#[cfg(feature = "wgpu")]
pub mod gpu;
pub mod image;
pub mod path;
#[cfg(feature = "wgpu")]
mod prepare;
pub mod raster;
pub mod render;
pub mod scene;
#[allow(dead_code)]
mod shade;
pub mod stroke;
pub mod text;

#[cfg(feature = "cpu")]
pub use cpu::{CpuRenderer, MAX_SIDE, PixelFormat, RenderError, RgbaImage};
#[cfg(feature = "wgpu")]
pub use gpu::Renderer;
pub use font::{FontChain, FontData, FontError, FontId, Fonts};
pub use fonts::{FontQuery, Generic};
pub use geom::{Affine, Color, IRect, Point, Rect};
pub use image::ImageStore;
pub use path::{FillRule, Flat, Path, Verb};
pub use raster::{Mask, Rasterizer};
pub use render::RenderOptions;
pub use scene::{ImageId, LayerId, MAX_STOPS, Paint, Picture, Sampling, Scene, ShadowStyle, Stops};
pub use stroke::{Cap, Dash, Join, Stroke};
pub use text::{Align, Hit, Layout, TextStyle};

/// The CPU half of the wgpu backend on its own (no device needed): tests and benchmarks use it
/// to check how a scene is prepared. Not a stable API.
#[cfg(feature = "wgpu")]
#[doc(hidden)]
pub mod prepare_only {
    use crate::image::ImageStore;
    use crate::render::RenderOptions;
    use crate::scene::Scene;

    pub use crate::prepare::PlanStats;

    pub struct Prepare {
        planner: crate::prepare::Planner,
    }

    impl Default for Prepare {
        fn default() -> Self {
            Prepare::new()
        }
    }

    impl Prepare {
        pub fn new() -> Prepare {
            Prepare { planner: crate::prepare::Planner::new() }
        }
        /// Pretend the device's largest texture side is `max` (tests of the limit).
        pub fn with_max_texture(max: u32) -> Prepare {
            let mut p = Prepare::new();
            p.planner.max_dim = max;
            p
        }
        /// Plan one frame and give the passes back to the pool.
        pub fn plan(&mut self, scene: &Scene, images: &ImageStore, opts: &RenderOptions) -> PlanStats {
            self.planner.plan(scene, images, opts);
            let stats = self.planner.stats;
            self.planner.recycle();
            stats
        }
    }
}
