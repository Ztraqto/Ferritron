//! Per-pixel formulas shared by the CPU backend and mirrored in the GPU shader
//! (`shader.wgsl`). Keep the two in step.

/// Error function approximation (maximum error about 5e-4).
pub(crate) fn erf(x: f32) -> f32 {
    let s = x.signum();
    let a = x.abs();
    let t = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    let t = t * t;
    s - s / (t * t)
}

/// Signed distance to a rounded rectangle centered at the origin with half extents `half` and
/// corner radius `r` (negative inside).
pub(crate) fn round_rect_sdf(px: f32, py: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let r = r.min(hx).min(hy).max(0.0);
    let qx = px.abs() - (hx - r);
    let qy = py.abs() - (hy - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

fn gaussian(x: f32, sigma: f32) -> f32 {
    (-(x * x) / (2.0 * sigma * sigma)).exp() / ((std::f32::consts::TAU).sqrt() * sigma)
}

fn shadow_x(x: f32, y: f32, sigma: f32, corner: f32, hx: f32, hy: f32) -> f32 {
    let delta = (hy - corner - y.abs()).min(0.0);
    let curved = hx - corner + (corner * corner - delta * delta).max(0.0).sqrt();
    let k = std::f32::consts::FRAC_1_SQRT_2 / sigma;
    let hi = 0.5 + 0.5 * erf((x + curved) * k);
    let lo = 0.5 + 0.5 * erf((x - curved) * k);
    hi - lo
}

/// Coverage of the Gaussian-blurred rounded rectangle at point `(px, py)` relative to its
/// center (`hx`, `hy` are the half extents).
pub(crate) fn blurred_round_rect(px: f32, py: f32, hx: f32, hy: f32, corner: f32, sigma: f32) -> f32 {
    let corner = corner.min(hx).min(hy).max(0.0);
    let low = py - hy;
    let high = py + hy;
    let start = (-3.0 * sigma).clamp(low.min(high), low.max(high));
    let end = (3.0 * sigma).clamp(low.min(high), low.max(high));
    let step = (end - start) / 4.0;
    let mut y = start + step * 0.5;
    let mut v = 0.0;
    for _ in 0..4 {
        v += shadow_x(px, py - y, sigma, corner, hx, hy) * gaussian(y, sigma) * step;
        y += step;
    }
    v.clamp(0.0, 1.0)
}

/// The value [`blurred_round_rect`] gives well inside a shape (every sample row fully covered):
/// the quadrature weights summed. A shadow's flat interior uses this constant.
pub(crate) fn blurred_interior(sigma: f32) -> f32 {
    // the same four sample rows as `blurred_round_rect` uses when the shape is tall enough
    let step = 1.5 * sigma;
    let mut v = 0.0;
    for i in 0..4 {
        let y = -3.0 * sigma + step * (i as f32 + 0.5);
        v += gaussian(y, sigma) * step;
    }
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interior_constant_matches_the_formula_deep_inside() {
        for sigma in [1.0f32, 4.0, 12.0] {
            let k = blurred_interior(sigma);
            // a box 12 sigma tall and wide, sample deep inside it
            let h = 12.0 * sigma;
            let v = blurred_round_rect(0.0, 0.0, h, h, 0.0, sigma);
            assert!((v - k).abs() < 1e-3, "sigma {sigma}: formula {v}, constant {k}");
        }
    }

    #[test]
    fn erf_is_close() {
        assert!((erf(0.0)).abs() < 1e-6);
        assert!((erf(1.0) - 0.842_700_8).abs() < 1e-3);
        assert!((erf(-2.0) + 0.995_322_3).abs() < 1e-3);
    }

    #[test]
    fn blurred_box_is_half_at_the_edge_and_full_deep_inside() {
        let edge = blurred_round_rect(50.0, 0.0, 50.0, 50.0, 0.0, 4.0);
        assert!((edge - 0.5).abs() < 0.03, "{edge}");
        let inside = blurred_round_rect(0.0, 0.0, 50.0, 50.0, 0.0, 4.0);
        assert!(inside > 0.99, "{inside}");
        let outside = blurred_round_rect(80.0, 0.0, 50.0, 50.0, 0.0, 4.0);
        assert!(outside < 0.01, "{outside}");
    }

    #[test]
    fn sdf_signs() {
        assert!(round_rect_sdf(0.0, 0.0, 10.0, 5.0, 2.0) < 0.0);
        assert!(round_rect_sdf(20.0, 0.0, 10.0, 5.0, 2.0) > 0.0);
        assert!((round_rect_sdf(10.0, 0.0, 10.0, 5.0, 0.0)).abs() < 1e-6);
    }
}
