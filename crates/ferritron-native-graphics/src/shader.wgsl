// FNG: the one shader. Every primitive is an instanced quad; the fragment stage computes the
// coverage (shape, mask or full), multiplies the clip coverage and shades with the paint.
// The formulas for the shadow and the rounded-rectangle distance mirror `shade.rs` (CPU).

struct Globals {
    // x, y: target size in pixels; z: atlas size; w: gradient ramp rows
    size: vec4<f32>,
    // x: 1.0 when the target is an sRGB format (convert the output to linear)
    flags: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var ramp: texture_2d<f32>;
@group(0) @binding(3) var samp_lin: sampler;
@group(0) @binding(4) var samp_near: sampler;
@group(1) @binding(0) var img: texture_2d<f32>;

struct VIn {
    @builtin(vertex_index) vi: u32,
    @location(0) bbox: vec4<f32>,    // device quad: x0, y0, x1, y1
    @location(1) inv0: vec4<f32>,    // device -> local: a, b, c, d
    @location(2) inv1: vec4<f32>,    // e, f, device pixels per local unit, kind
    @location(3) shape: vec4<f32>,
    @location(4) shape2: vec4<f32>,
    @location(5) color: vec4<f32>,   // straight sRGB
    @location(6) paint: vec4<f32>,   // kind, ramp row, nearest sampling, 1/|delta|^2
    @location(7) pinv0: vec4<f32>,   // device -> paint space: a, b, c, d
    @location(8) pinv1: vec4<f32>,   // e, f, texture width, texture height
    @location(9) gp: vec4<f32>,
    @location(10) clip: vec4<f32>,   // device clip rectangle
    @location(11) rclip: vec4<f32>,  // rounded clip rectangle (device)
    @location(12) rclip2: vec4<f32>, // x: radius (< 0: none)
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) inv0: vec4<f32>,
    @location(1) @interpolate(flat) inv1: vec4<f32>,
    @location(2) @interpolate(flat) shape: vec4<f32>,
    @location(3) @interpolate(flat) shape2: vec4<f32>,
    @location(4) @interpolate(flat) color: vec4<f32>,
    @location(5) @interpolate(flat) paint: vec4<f32>,
    @location(6) @interpolate(flat) pinv0: vec4<f32>,
    @location(7) @interpolate(flat) pinv1: vec4<f32>,
    @location(8) @interpolate(flat) gp: vec4<f32>,
    @location(9) @interpolate(flat) clip: vec4<f32>,
    @location(10) @interpolate(flat) rclip: vec4<f32>,
    @location(11) @interpolate(flat) rclip2: vec4<f32>,
};

@vertex
fn vs(v: VIn) -> VOut {
    let corner = vec2<f32>(f32(v.vi & 1u), f32(v.vi >> 1u));
    let dev = mix(v.bbox.xy, v.bbox.zw, corner);
    var o: VOut;
    o.pos = vec4<f32>(dev.x / g.size.x * 2.0 - 1.0, 1.0 - dev.y / g.size.y * 2.0, 0.0, 1.0);
    o.inv0 = v.inv0;
    o.inv1 = v.inv1;
    o.shape = v.shape;
    o.shape2 = v.shape2;
    o.color = v.color;
    o.paint = v.paint;
    o.pinv0 = v.pinv0;
    o.pinv1 = v.pinv1;
    o.gp = v.gp;
    o.clip = v.clip;
    o.rclip = v.rclip;
    o.rclip2 = v.rclip2;
    return o;
}

fn round_rect_sdf(p: vec2<f32>, hf: vec2<f32>, r_in: f32) -> f32 {
    let r = max(min(r_in, min(hf.x, hf.y)), 0.0);
    let q = abs(p) - (hf - vec2<f32>(r, r));
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn erf4(x: f32) -> f32 {
    let s = sign(x);
    let a = abs(x);
    var t = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    t = t * t;
    return s - s / (t * t);
}

fn gaussian(x: f32, sigma: f32) -> f32 {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (2.5066282746 * sigma);
}

fn shadow_x(x: f32, y: f32, sigma: f32, corner: f32, hf: vec2<f32>) -> f32 {
    let delta = min(hf.y - corner - abs(y), 0.0);
    let curved = hf.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    let k = 0.70710678 / sigma;
    let hi = 0.5 + 0.5 * erf4((x + curved) * k);
    let lo = 0.5 + 0.5 * erf4((x - curved) * k);
    return hi - lo;
}

fn blurred_round_rect(p: vec2<f32>, hf: vec2<f32>, corner_in: f32, sigma: f32) -> f32 {
    let corner = max(min(corner_in, min(hf.x, hf.y)), 0.0);
    let low = p.y - hf.y;
    let high = p.y + hf.y;
    let lo = min(low, high);
    let hi = max(low, high);
    let start = clamp(-3.0 * sigma, lo, hi);
    let end = clamp(3.0 * sigma, lo, hi);
    let step = (end - start) / 4.0;
    var y = start + step * 0.5;
    var value = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        value = value + shadow_x(p.x, p.y - y, sigma, corner, hf) * gaussian(y, sigma) * step;
        y = y + step;
    }
    return clamp(value, 0.0, 1.0);
}

fn srgb_to_linear(c: f32) -> f32 {
    if (c <= 0.04045) {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy;
    if (p.x < in.clip.x || p.x >= in.clip.z || p.y < in.clip.y || p.y >= in.clip.w) {
        discard;
    }
    let kind = i32(in.inv1.w + 0.5);
    let dscale = in.inv1.z;
    let local = vec2<f32>(
        in.inv0.x * p.x + in.inv0.z * p.y + in.inv1.x,
        in.inv0.y * p.x + in.inv0.w * p.y + in.inv1.y,
    );

    var cov = 1.0;
    if (kind == 0) {
        let hf = in.shape.zw * 0.5;
        let d = round_rect_sdf(local - (in.shape.xy + hf), hf, in.shape2.x);
        cov = clamp(0.5 - d * dscale, 0.0, 1.0);
    } else if (kind == 1) {
        let r = in.shape.zw;
        let d = (length((local - in.shape.xy) / r) - 1.0) * min(r.x, r.y);
        cov = clamp(0.5 - d * dscale, 0.0, 1.0);
    } else if (kind == 2) {
        let dir = in.shape2.xy;
        let q = local - in.shape.xy;
        let u = dot(q, dir);
        let v = dot(q, vec2<f32>(-dir.y, dir.x));
        let len = in.shape.z;
        let hw = in.shape.w;
        var cap = in.shape2.z;
        if (u > 0.0) {
            cap = in.shape2.w;
        }
        var d: f32;
        if (abs(u) <= len) {
            d = abs(v) - hw;
        } else if (cap < 0.5) {
            d = max(abs(u) - len, abs(v) - hw);
        } else if (cap < 1.5) {
            d = length(vec2<f32>(abs(u) - len, v)) - hw;
        } else {
            d = max(abs(u) - len - hw, abs(v) - hw);
        }
        cov = clamp(0.5 - d * dscale, 0.0, 1.0);
    } else if (kind == 3) {
        let hf = in.shape.zw * 0.5;
        let c = local - (in.shape.xy + hf);
        let sigma = in.shape2.y;
        if (sigma > 0.001) {
            cov = blurred_round_rect(c, hf, in.shape2.x, sigma);
        } else {
            let d = round_rect_sdf(c, hf, in.shape2.x);
            cov = clamp(0.5 - d * dscale, 0.0, 1.0);
        }
    } else if (kind == 4) {
        let ip = vec2<i32>(floor(p)) - vec2<i32>(in.shape2.xy);
        if (ip.x < 0 || ip.y < 0 || ip.x >= i32(in.shape.z) || ip.y >= i32(in.shape.w)) {
            cov = 0.0;
        } else {
            cov = textureLoad(atlas, vec2<i32>(in.shape.xy) + ip, 0).r;
        }
    }

    if (in.rclip2.x >= 0.0) {
        let rh = (in.rclip.zw - in.rclip.xy) * 0.5;
        let d = round_rect_sdf(p - (in.rclip.xy + rh), rh, in.rclip2.x);
        cov = cov * clamp(0.5 - d, 0.0, 1.0);
    }

    // paint (premultiplied result)
    var col: vec4<f32>;
    let pk = i32(in.paint.x + 0.5);
    if (pk == 0) {
        col = vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    } else if (pk == 1 || pk == 2) {
        let pp = vec2<f32>(
            in.pinv0.x * p.x + in.pinv0.z * p.y + in.pinv1.x,
            in.pinv0.y * p.x + in.pinv0.w * p.y + in.pinv1.y,
        );
        var t: f32;
        if (pk == 1) {
            t = clamp(dot(pp - in.gp.xy, in.gp.zw) * in.paint.w, 0.0, 1.0);
        } else {
            t = clamp(length(pp - in.gp.xy) / in.gp.z, 0.0, 1.0);
        }
        let uv = vec2<f32>((t * 255.0 + 0.5) / 256.0, (in.paint.y + 0.5) / g.size.w);
        let s = textureSampleLevel(ramp, samp_lin, uv, 0.0);
        col = vec4<f32>(s.rgb * s.a, s.a);
    } else {
        let pp = vec2<f32>(
            in.pinv0.x * p.x + in.pinv0.z * p.y + in.pinv1.x,
            in.pinv0.y * p.x + in.pinv0.w * p.y + in.pinv1.y,
        );
        let uv = pp / in.pinv1.zw;
        var s: vec4<f32>;
        if (in.paint.z > 0.5) {
            s = textureSampleLevel(img, samp_near, uv, 0.0);
        } else {
            // level of detail: log2 of the texels one device pixel covers along the longer axis
            // (layers and groups have one level, so they stay at level 0)
            let rho = max(length(vec2<f32>(in.pinv0.x, in.pinv0.y)), length(vec2<f32>(in.pinv0.z, in.pinv0.w)));
            s = textureSampleLevel(img, samp_lin, uv, max(log2(max(rho, 1.0)), 0.0));
        }
        col = s * in.color.a;
    }

    col = col * cov;

    if (g.flags.x > 0.5 && col.a > 0.0) {
        let c = col.rgb / col.a;
        col = vec4<f32>(
            vec3<f32>(srgb_to_linear(c.r), srgb_to_linear(c.g), srgb_to_linear(c.b)) * col.a,
            col.a,
        );
    }
    return col;
}
