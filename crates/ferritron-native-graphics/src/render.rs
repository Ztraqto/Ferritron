//! Options shared by the renderers.

use crate::geom::Color;

/// How to draw a scene.
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    /// Size of the target in device pixels.
    pub size_px: (u32, u32),
    /// Device pixels per logical pixel (DPI scale).
    pub scale: f32,
    /// Fill the target with this color first (`None`: draw over what is there; the CPU backend
    /// starts from transparent).
    pub clear: Option<Color>,
}

impl RenderOptions {
    pub fn new(size_px: (u32, u32)) -> RenderOptions {
        RenderOptions { size_px, scale: 1.0, clear: None }
    }
    pub fn with_scale(mut self, scale: f32) -> RenderOptions {
        self.scale = scale;
        self
    }
    pub fn with_clear(mut self, color: Color) -> RenderOptions {
        self.clear = Some(color);
        self
    }
}
