//! Linear IIR filters on the GPU with parallel scans (design doc 9.4).
//!
//! An IIR filter looks serial: every output needs the previous one. But its
//! state follows a linear recurrence, `s[n] = A·s[n-1] + B·x[n]`, and such
//! recurrences compose associatively. So a block is computed in parallel:
//! 256 threads each run their own stretch of the block from a zero state,
//! a Hillis–Steele scan with powers of `A` hands every stretch its incoming
//! state in 8 steps, and a second pass adds it in. Block length is not
//! limited by workgroup memory, so offline rendering can use huge blocks.
//!
//! [`IirBank`] runs many filter chains ("lanes") at once, one workgroup per
//! lane: modal synthesis (thousands of resonators summed into one output),
//! filter banks (one output per band), or EQ for many channels.
//! [`RealtimeIirBank`] puts it on a GPU thread for use on the audio thread,
//! with a fixed latency (the "max" GPU policy, 9.10): an IIR cannot be split
//! by latency the way a convolution can.

use crate::batch::{Batcher, Registry};
use crate::{Context, context, entry, f32_bytes, u32_bytes};
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Lanes per partial sum when mixing lanes into outputs (`CHUNK` in the shader).
const CHUNK: usize = 32;
/// A dispatch has at most this many workgroups per dimension.
const MAX_DISPATCH: usize = 65_535;

pub(crate) const SHADER: &str = r#"
struct Params {
    len: u32,       // frames in this block
    lanes: u32,
    outputs: u32,
    chunks: u32,    // lane chunks summed in parallel
    max_block: u32, // stride of per-channel input and output signals
    stride: u32,    // stride of lane signals: max_block rounded up to 256
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> lanes: array<vec4<u32>>; // first section, sections, input, output
@group(0) @binding(2) var<storage, read> coefs: array<f32>;       // A (4), B (2), C (2), D, M (4) per section
@group(0) @binding(3) var<storage, read_write> state: array<vec2<f32>>;
@group(0) @binding(4) var<storage, read> input: array<f32>;
// Lane signals, transposed so that neighbouring threads touch neighbouring
// words: frame start(t) + k of thread t is at k·256 + t.
@group(0) @binding(5) var<storage, read_write> work: array<f32>;
@group(0) @binding(6) var<storage, read_write> partial: array<f32>;
@group(0) @binding(7) var<storage, read_write> out: array<f32>;

const CHUNK: u32 = 32u;

var<workgroup> carry: array<vec2<f32>, 256>;

// 2x2 matrices as vec4: (m00, m01, m10, m11).
fn mul(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(a.x * b.x + a.y * b.z, a.x * b.y + a.y * b.w, a.z * b.x + a.w * b.z, a.z * b.y + a.w * b.w);
}

fn apply(m: vec4<f32>, v: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(m.x * v.x + m.y * v.y, m.z * v.x + m.w * v.y);
}

fn power(m: vec4<f32>, e: u32) -> vec4<f32> {
    var r = vec4<f32>(1.0, 0.0, 0.0, 1.0);
    var b = m;
    var k = e;
    while (k != 0u) {
        if ((k & 1u) == 1u) {
            r = mul(r, b);
        }
        b = mul(b, b);
        k >>= 1u;
    }
    return r;
}

// One workgroup per lane; its sections run one after another over the block.
// Each section is a state space (realized on the CPU so that A's powers
// stay bounded):
//   s[n] = A·s[n-1] + B·x[n]
//   y[n] = C·s[n-1] + D·x[n]
@compute @workgroup_size(256)
fn scan(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let lane = lanes[wg.x];
    let per = (p.len + 255u) / 256u;
    let start = min(lid * per, p.len);
    let end = min(start + per, p.len);
    let count = end - start;
    let base = wg.x * p.stride + lid;
    for (var k = 0u; k < count; k++) {
        work[base + k * 256u] = input[lane.z * p.max_block + start + k];
    }
    for (var k = 0u; k < lane.y; k++) {
        let sec = lane.x + k;
        let c = sec * 13u;
        let a = vec4<f32>(coefs[c], coefs[c + 1u], coefs[c + 2u], coefs[c + 3u]);
        let bv = vec2<f32>(coefs[c + 4u], coefs[c + 5u]);
        let cv = vec2<f32>(coefs[c + 6u], coefs[c + 7u]);
        let dv = coefs[c + 8u];

        // 1. This thread's stretch from a zero state.
        var s = vec2<f32>(0.0, 0.0);
        for (var k = 0u; k < count; k++) {
            s = apply(a, s) + bv * work[base + k * 256u];
        }
        carry[lid] = s;
        // M carries the state over a coefficient change (identity otherwise).
        let s0 = apply(vec4<f32>(coefs[c + 9u], coefs[c + 10u], coefs[c + 11u], coefs[c + 12u]), state[sec]);
        workgroupBarrier();

        // 2. Inclusive scan over stretches: carry[i] += A^(per·d)·carry[i-d].
        //    Meanwhile collect A^(per·lid), which carries the block's
        //    initial state to this stretch.
        var q = power(a, per);
        var into = vec4<f32>(1.0, 0.0, 0.0, 1.0);
        for (var j = 0u; j < 8u; j++) {
            let d = 1u << j;
            var v = carry[lid];
            if (lid >= d) {
                v += apply(q, carry[lid - d]);
            }
            if (((lid >> j) & 1u) == 1u) {
                into = mul(q, into);
            }
            workgroupBarrier();
            carry[lid] = v;
            workgroupBarrier();
            q = mul(q, q);
        }

        // 3. Run the stretch again from its true incoming state.
        s = apply(into, s0);
        if (lid > 0u) {
            s += carry[lid - 1u];
        }
        for (var k = 0u; k < count; k++) {
            let x = work[base + k * 256u];
            work[base + k * 256u] = dot(cv, s) + dv * x;
            s = apply(a, s) + bv * x;
        }
        workgroupBarrier();
        if (start < end && end == p.len) {
            state[sec] = s;
        }
        workgroupBarrier();
    }
}

// Partial sums of lane outputs: chunk c of lanes, output o. Threads walk the
// transposed lane signals in memory order.
@compute @workgroup_size(256)
fn mix(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    let c = id.y;
    let o = id.z;
    let per = (p.len + 255u) / 256u;
    let n = (i % 256u) * per + i / 256u;
    if (i / 256u >= per || n >= p.len) {
        return;
    }
    var acc = 0.0;
    let first = c * CHUNK;
    for (var l = first; l < min(first + CHUNK, p.lanes); l++) {
        if (lanes[l].w == o) {
            acc += work[l * p.stride + i];
        }
    }
    partial[(c * p.outputs + o) * p.max_block + n] = acc;
}

@compute @workgroup_size(256)
fn sum(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = id.x;
    let o = id.y;
    if (n >= p.len) {
        return;
    }
    var acc = 0.0;
    for (var c = 0u; c < p.chunks; c++) {
        acc += partial[(c * p.outputs + o) * p.max_block + n];
    }
    out[o * p.max_block + n] = acc;
}
"#;

/// A second-order section, normalized so that `a0 = 1`.
///
/// `y[n] = b0·x[n] + b1·x[n-1] + b2·x[n-2] - a1·y[n-1] - a2·y[n-2]`.
/// The constructors follow the RBJ Audio EQ Cookbook.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    /// The section as a state space `s[n] = A·s[n-1] + B·x[n]`,
    /// `y[n] = C·s[n-1] + D·x[n]`, as `[A (row-major), B, C, D]`, plus the
    /// matrix `T` with `u = T·s`, where `u` is the state of transposed direct
    /// form II.
    ///
    /// Direct form's state matrix `A_c = [[-a1, 1], [-a2, 0]]` has powers
    /// that grow to about `1/θ` for poles at low angles θ, and the parallel
    /// scan multiplies by such powers: in f32 that costs digits. So the
    /// section is transformed, `A = T⁻¹·A_c·T`, into a scaled rotation for
    /// complex poles `r·e^(±iθ)` and into two first-order stages in series
    /// for real poles; every power of either stays bounded for a stable
    /// filter. `T` depends only on the poles, so it is always invertible and
    /// the state can be carried over any coefficient change.
    fn realization(&self) -> ([f64; 9], [f64; 4]) {
        let (a1, a2) = (self.a1 as f64, self.a2 as f64);
        let b0 = self.b0 as f64;
        // Direct form: B_c = (c1, c2), C_c = (1, 0), D = b0.
        let (c1, c2) = (self.b1 as f64 - a1 * b0, self.b2 as f64 - a2 * b0);
        let re = -a1 / 2.0;
        let im2 = a2 - re * re;
        if im2 > 1e-10 {
            let im = im2.sqrt();
            let t = [1.0, 0.0, -re, -im];
            ([re, -im, im, re, c1, -(re * c1 + c2) / im, 1.0, 0.0, b0], t)
        } else {
            let d = (-im2).max(0.0).sqrt();
            let (p1, p2) = (re + d, re - d);
            let t = [0.0, 1.0, 1.0, -p1];
            ([p1, 0.0, 1.0, p2, p1 * c1 + c2, c1, 0.0, 1.0, b0], t)
        }
    }

    /// The matrix that carries a state of `from` over to `self` so that the
    /// filter continues as transposed direct form II would: `T⁻¹·T_from`.
    fn transition_from(&self, from: &Biquad) -> [f64; 4] {
        let t = self.realization().1;
        let det = t[0] * t[3] - t[1] * t[2];
        let t_inv = [t[3] / det, -t[1] / det, -t[2] / det, t[0] / det];
        mat_mul(t_inv, from.realization().1)
    }

    /// Passes the signal through unchanged.
    pub const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn normalized(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b0: (b[0] / a[0]) as f32,
            b1: (b[1] / a[0]) as f32,
            b2: (b[2] / a[0]) as f32,
            a1: (a[1] / a[0]) as f32,
            a2: (a[2] / a[0]) as f32,
        }
    }

    fn prepare(freq: f64, q: f64, sample_rate: f64) -> (f64, f64) {
        let w = std::f64::consts::TAU * freq / sample_rate;
        (w.cos(), w.sin() / (2.0 * q))
    }

    pub fn lowpass(freq: f64, q: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::prepare(freq, q, sample_rate);
        Self::normalized([(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    pub fn highpass(freq: f64, q: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::prepare(freq, q, sample_rate);
        Self::normalized([(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    /// Band-pass with 0 dB gain at `freq`; with a high `q`, a resonator.
    pub fn bandpass(freq: f64, q: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::prepare(freq, q, sample_rate);
        Self::normalized([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    /// Peaking EQ: `gain_db` at `freq`, unity elsewhere.
    pub fn peaking(freq: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        let (cos, alpha) = Self::prepare(freq, q, sample_rate);
        let a = 10f64.powf(gain_db / 40.0);
        Self::normalized([1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a])
    }
}

fn mat_mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0] * b[0] + a[1] * b[2], a[0] * b[1] + a[1] * b[3], a[2] * b[0] + a[3] * b[2], a[2] * b[1] + a[3] * b[3]]
}

const IDENTITY: [f64; 4] = [1.0, 0.0, 0.0, 1.0];

/// One filter chain of a bank: `sections` in series, from input channel
/// `input` to output channel `output`. Lanes with the same output are summed.
#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    pub input: usize,
    pub output: usize,
    pub sections: Vec<Biquad>,
}

/// One stage of an [`IirBank`] on the GPU: its lanes, their signals and sums.
struct Stage {
    params: wgpu::Buffer,
    coef_buffer: wgpu::Buffer,
    state: wgpu::Buffer,
    out: wgpu::Buffer,
    scan: wgpu::BindGroup,
    mix: wgpu::BindGroup,
    sum: wgpu::BindGroup,
    lanes: usize,
    /// This stage's sections in the bank's `coefs`.
    sections: Range<usize>,
    outputs: usize,
    chunks: usize,
}

/// Many IIR filter chains computed on the GPU at once. See the module docs.
///
/// Banks can be chained ([`IirBank::chain`]): the stages run in series and
/// their signals stay on the GPU, so the whole chain costs one upload, one
/// submission and one readback per block (node fusion, design doc 9.7).
///
/// [`IirBank::process`] waits for the GPU: use it off the audio thread
/// (offline rendering, analysis), or through [`RealtimeIirBank`].
pub struct IirBank {
    ctx: Arc<Context>,
    input: wgpu::Buffer,
    staging: wgpu::Buffer,
    /// Filter state read back with the output when `capture_state` is set.
    state_staging: wgpu::Buffer,
    capture_state: bool,
    /// The state after the last block, in transposed direct form II
    /// coordinates (valid when `capture_state` is set).
    state_out: Vec<[f32; 2]>,
    stages: Vec<Stage>,
    /// First section of each lane (all stages, in order), and the section count.
    lane_sections: Vec<(usize, usize)>,
    coefs: Vec<Biquad>,
    /// Per section: carries the state over coefficient changes since the
    /// last block (identity when unchanged).
    transitions: Vec<[f64; 4]>,
    coefs_dirty: bool,
    inputs: usize,
    outputs: usize,
    max_block: usize,
}

/// Checks a chain of stages; returns the chain's input and output channel counts.
fn check_chain(stages: &[&[Lane]]) -> Result<(usize, usize), String> {
    if stages.is_empty() {
        return Err("an IIR chain needs at least one stage".into());
    }
    let inputs = stages[0].iter().map(|l| l.input + 1).max().unwrap_or(0);
    let mut available = inputs;
    for (k, stage) in stages.iter().enumerate() {
        if stage.is_empty() || stage.len() > MAX_DISPATCH {
            return Err(format!("stage {k}: an IIR bank needs 1 to {MAX_DISPATCH} lanes"));
        }
        if let Some(lane) = stage.iter().find(|l| l.input >= available) {
            return Err(format!("stage {k} reads channel {}, but its input has {available} channels", lane.input));
        }
        available = stage.iter().map(|l| l.output + 1).max().unwrap_or(0);
    }
    Ok((inputs, available))
}

impl IirBank {
    /// Prepares a bank for blocks of up to `max_block` frames. Input and
    /// output channel counts follow from the highest indices in `lanes`.
    pub fn new(lanes: &[Lane], max_block: usize) -> Result<Self, String> {
        Self::chain(&[lanes], max_block)
    }

    /// Banks in series: stage `k` reads the outputs of stage `k - 1` (the
    /// first stage reads the bank's inputs). Lanes are numbered across all
    /// stages, in order, for [`set_lane`](Self::set_lane).
    pub fn chain(stages: &[&[Lane]], max_block: usize) -> Result<Self, String> {
        let max_block = max_block.max(1);
        let (inputs, outputs) = check_chain(stages)?;
        if stages.iter().flat_map(|s| s.iter()).any(|l| l.output >= MAX_DISPATCH) || max_block.div_ceil(256) > MAX_DISPATCH {
            return Err("too many outputs or too large a block".into());
        }
        let ctx = context()?;
        let device = &ctx.device;
        let buffer = |label: &'static str, size: usize, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(16) as u64, usage, mapped_at_creation: false })
        };
        use wgpu::BufferUsages as U;
        let group = |pipeline: &wgpu::ComputePipeline, entries: &[wgpu::BindGroupEntry]| {
            device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries })
        };
        let input = buffer("iir input", inputs * max_block * 4, U::STORAGE | U::COPY_DST);
        let mut lane_sections = Vec::new();
        let mut coefs = Vec::new();
        let mut built: Vec<Stage> = Vec::with_capacity(stages.len());
        for lanes in stages {
            let first_section = coefs.len();
            let stage_outputs = lanes.iter().map(|l| l.output + 1).max().unwrap_or(0);
            let chunks = lanes.len().div_ceil(CHUNK);
            let mut table = Vec::with_capacity(lanes.len() * 4);
            for lane in lanes.iter() {
                // Section indices in a stage's table are relative to the stage.
                table.extend([(coefs.len() - first_section) as u32, lane.sections.len() as u32, lane.input as u32, lane.output as u32]);
                lane_sections.push((coefs.len(), lane.sections.len()));
                coefs.extend_from_slice(&lane.sections);
            }
            let sections = first_section..coefs.len();
            let params = buffer("iir params", 32, U::UNIFORM | U::COPY_DST);
            let lane_buffer = buffer("iir lanes", table.len() * 4, U::STORAGE | U::COPY_DST);
            let coef_buffer = buffer("iir coefficients", sections.len() * 13 * 4, U::STORAGE | U::COPY_DST);
            let state = buffer("iir state", sections.len() * 8, U::STORAGE | U::COPY_DST | U::COPY_SRC);
            let work = buffer("iir lane signals", lanes.len() * max_block.next_multiple_of(256) * 4, U::STORAGE);
            let partial = buffer("iir partial sums", chunks * stage_outputs * max_block * 4, U::STORAGE);
            let out = buffer("iir output", stage_outputs * max_block * 4, U::STORAGE | U::COPY_SRC);
            ctx.queue.write_buffer(&lane_buffer, 0, &u32_bytes(&table));
            // Data stays on the GPU: a stage reads the previous stage's output.
            let source = built.last().map_or(&input, |s| &s.out);
            let scan = group(
                &ctx.scan,
                &[entry(0, &params), entry(1, &lane_buffer), entry(2, &coef_buffer), entry(3, &state), entry(4, source), entry(5, &work)],
            );
            let mix = group(&ctx.mix, &[entry(0, &params), entry(1, &lane_buffer), entry(5, &work), entry(6, &partial)]);
            let sum = group(&ctx.sum, &[entry(0, &params), entry(6, &partial), entry(7, &out)]);
            built.push(Stage { params, coef_buffer, state, out, scan, mix, sum, lanes: lanes.len(), sections, outputs: stage_outputs, chunks });
        }
        let staging = buffer("iir staging", outputs * max_block * 4, U::MAP_READ | U::COPY_DST);
        let state_staging = buffer("iir state staging", coefs.len() * 8, U::MAP_READ | U::COPY_DST);

        let mut bank = Self {
            ctx: ctx.clone(),
            input,
            staging,
            state_staging,
            capture_state: false,
            state_out: vec![[0.0; 2]; coefs.len()],
            stages: built,
            lane_sections,
            transitions: vec![IDENTITY; coefs.len()],
            coefs,
            coefs_dirty: true,
            inputs,
            outputs,
            max_block,
        };
        bank.reset();
        // Pay pipeline setup now, not on the first block; then start clean.
        let silence = [0.0; 1];
        let mut sink = vec![vec![0.0; 1]; outputs];
        bank.process(&vec![&silence[..]; inputs], &mut sink.iter_mut().map(|v| &mut v[..]).collect::<Vec<_>>());
        bank.reset();
        Ok(bank)
    }

    /// The GPU in use.
    pub fn adapter(&self) -> &str {
        &self.ctx.adapter
    }

    /// Input channels (of the first stage: its highest lane input + 1).
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// Output channels (of the last stage: its highest lane output + 1).
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Lanes of all stages.
    pub fn lanes(&self) -> usize {
        self.lane_sections.len()
    }

    /// Replaces the coefficients of `lane` from the next block on. The
    /// filter continues as transposed direct form II would, so automation
    /// sounds the same as with a typical CPU biquad. A lane keeps the section
    /// count it was created with: extra sections are ignored, missing ones
    /// keep their coefficients.
    pub fn set_lane(&mut self, lane: usize, sections: &[Biquad]) {
        let Some(&(first, count)) = self.lane_sections.get(lane) else { return };
        for (i, new) in sections.iter().take(count).enumerate() {
            let sec = first + i;
            if *new != self.coefs[sec] {
                self.transitions[sec] = mat_mul(new.transition_from(&self.coefs[sec]), self.transitions[sec]);
                self.coefs[sec] = *new;
                self.coefs_dirty = true;
            }
        }
    }

    /// Clears the filter state (silence in, silence out).
    pub fn reset(&mut self) {
        for stage in &self.stages {
            self.ctx.queue.write_buffer(&stage.state, 0, &vec![0u8; stage.sections.len().max(2) * 8]);
        }
    }

    /// Filters one block: `inputs[i]` and `outputs[o]` are channels. Frames
    /// beyond the shortest slice (or beyond `max_block`) are left untouched.
    /// Waits for the GPU.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let frames = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0);
        Self::process_many(&mut [BatchJob { bank: self, inputs, outputs, frames }]);
    }

    /// Filters one block of several banks in one GPU submission and one
    /// readback (batching across instances, design doc 9.2): the fixed cost
    /// of a round trip is paid once for all of them. Waits for the GPU.
    pub fn process_many<I: AsRef<[f32]>, O: AsMut<[f32]>>(jobs: &mut [BatchJob<'_, I, O>]) {
        for job in jobs.iter_mut() {
            job.frames = job.frames.min(job.bank.max_block);
            if job.inputs.len() < job.bank.inputs || job.outputs.len() < job.bank.outputs {
                job.frames = 0;
            }
        }
        let Some(ctx) = jobs.iter().find(|j| j.frames > 0).map(|j| j.bank.ctx.clone()) else { return };
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        for job in jobs.iter_mut().filter(|j| j.frames > 0) {
            job.bank.upload(job.inputs, job.frames);
            job.bank.encode(&mut encoder, job.frames);
        }
        ctx.queue.submit([encoder.finish()]);
        for job in jobs.iter().filter(|j| j.frames > 0) {
            job.bank.staging.map_async(wgpu::MapMode::Read, .., |_| {});
            if job.bank.capture_state {
                job.bank.state_staging.map_async(wgpu::MapMode::Read, .., |_| {});
            }
        }
        let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
        for job in jobs.iter_mut().filter(|j| j.frames > 0) {
            job.bank.read(job.outputs, job.frames);
        }
    }

    /// Writes a block's input, changed coefficients and parameters.
    fn upload<I: AsRef<[f32]>>(&mut self, inputs: &[I], n: usize) {
        let queue = &self.ctx.queue;
        for (ch, channel) in inputs.iter().take(self.inputs).enumerate() {
            queue.write_buffer(&self.input, (ch * self.max_block * 4) as u64, &f32_bytes(&channel.as_ref()[..n]));
        }
        if self.coefs_dirty {
            for stage in &self.stages {
                let range = stage.sections.clone();
                let flat: Vec<f32> = self.coefs[range.clone()]
                    .iter()
                    .zip(&self.transitions[range])
                    .flat_map(|(c, t)| c.realization().0.into_iter().chain(*t).map(|v| v as f32))
                    .collect();
                queue.write_buffer(&stage.coef_buffer, 0, &f32_bytes(&flat));
            }
            // Transitions apply once: upload identities with the next block.
            self.coefs_dirty = self.transitions.iter().any(|t| *t != IDENTITY);
            self.transitions.fill(IDENTITY);
        }
        let stride = self.max_block.next_multiple_of(256) as u32;
        for stage in &self.stages {
            let p = [n as u32, stage.lanes as u32, stage.outputs as u32, stage.chunks as u32, self.max_block as u32, stride, 0, 0];
            queue.write_buffer(&stage.params, 0, &u32_bytes(&p));
        }
    }

    /// Records every stage, and the copies back, into `encoder`.
    fn encode(&self, encoder: &mut wgpu::CommandEncoder, n: usize) {
        let ctx = &self.ctx;
        let frames = (n as u32).div_ceil(256);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            for stage in &self.stages {
                pass.set_pipeline(&ctx.scan);
                pass.set_bind_group(0, &stage.scan, &[]);
                pass.dispatch_workgroups(stage.lanes as u32, 1, 1);
                pass.set_pipeline(&ctx.mix);
                pass.set_bind_group(0, &stage.mix, &[]);
                // One workgroup per step k of the transposed signals.
                pass.dispatch_workgroups(frames, stage.chunks as u32, stage.outputs as u32);
                pass.set_pipeline(&ctx.sum);
                pass.set_bind_group(0, &stage.sum, &[]);
                pass.dispatch_workgroups(frames, stage.outputs as u32, 1);
            }
        }
        let last = self.stages.last().expect("a bank has at least one stage");
        let bytes = (self.outputs * self.max_block * 4) as u64;
        encoder.copy_buffer_to_buffer(&last.out, 0, &self.staging, 0, bytes);
        if self.capture_state {
            for stage in self.stages.iter().filter(|s| !s.sections.is_empty()) {
                let at = (stage.sections.start * 8) as u64;
                encoder.copy_buffer_to_buffer(&stage.state, 0, &self.state_staging, at, (stage.sections.len() * 8) as u64);
            }
        }
    }

    /// Reads the mapped output (and state) after the submission finished.
    fn read<O: AsMut<[f32]>>(&mut self, outputs: &mut [O], n: usize) {
        if self.capture_state {
            if let Ok(view) = self.state_staging.get_mapped_range(..) {
                // The GPU's realization to direct form II: u = T·s.
                for ((u, c), [a, b, cc, d, e, f, g, h]) in self.state_out.iter_mut().zip(&self.coefs).zip(view.as_chunks::<8>().0) {
                    let s0 = f32::from_le_bytes([*a, *b, *cc, *d]) as f64;
                    let s1 = f32::from_le_bytes([*e, *f, *g, *h]) as f64;
                    let t = c.realization().1;
                    *u = [(t[0] * s0 + t[1] * s1) as f32, (t[2] * s0 + t[3] * s1) as f32];
                }
            }
            self.state_staging.unmap();
        }
        if let Ok(view) = self.staging.get_mapped_range(..) {
            for (o, channel) in outputs.iter_mut().take(self.outputs).enumerate() {
                let bytes = &view[o * self.max_block * 4..(o * self.max_block + n) * 4];
                for (s, chunk) in channel.as_mut()[..n].iter_mut().zip(bytes.as_chunks::<4>().0) {
                    *s = f32::from_le_bytes(*chunk);
                }
            }
        }
        self.staging.unmap();
    }
}

/// One bank's block in [`IirBank::process_many`].
pub struct BatchJob<'a, I = &'a [f32], O = &'a mut [f32]> {
    pub bank: &'a mut IirBank,
    pub inputs: &'a [I],
    pub outputs: &'a mut [O],
    /// Frames to process (at most the bank's `max_block`).
    pub frames: usize,
}

/// The same filter bank (or chain) on the CPU, sample by sample in transposed
/// direct form II: the fallback without a GPU, and the faster choice for
/// small banks. Real-time safe.
pub struct CpuIirBank {
    /// (first section, sections, input, output) per lane, all stages in order.
    lanes: Vec<(usize, usize, usize, usize)>,
    /// Lanes of each stage.
    stages: Vec<Range<usize>>,
    coefs: Vec<Biquad>,
    state: Vec<[f32; 2]>,
    scratch: Vec<f32>,
    /// Outputs of every stage but the last.
    between: Vec<Vec<Vec<f32>>>,
    inputs: usize,
    outputs: usize,
}

impl CpuIirBank {
    pub fn new(lanes: &[Lane], max_block: usize) -> Self {
        Self::build(&[lanes], max_block)
    }

    /// Banks in series; see [`IirBank::chain`].
    pub fn chain(stages: &[&[Lane]], max_block: usize) -> Result<Self, String> {
        check_chain(stages)?;
        Ok(Self::build(stages, max_block))
    }

    fn build(stages: &[&[Lane]], max_block: usize) -> Self {
        let max_block = max_block.max(1);
        let mut table = Vec::new();
        let mut coefs = Vec::new();
        let mut ranges = Vec::with_capacity(stages.len());
        let mut between = Vec::new();
        for (k, lanes) in stages.iter().enumerate() {
            let start = table.len();
            for lane in lanes.iter() {
                table.push((coefs.len(), lane.sections.len(), lane.input, lane.output));
                coefs.extend_from_slice(&lane.sections);
            }
            ranges.push(start..table.len());
            if k + 1 < stages.len() {
                let channels = lanes.iter().map(|l| l.output + 1).max().unwrap_or(0);
                between.push(vec![vec![0.0; max_block]; channels]);
            }
        }
        Self {
            inputs: stages.first().and_then(|s| s.iter().map(|l| l.input + 1).max()).unwrap_or(0),
            outputs: stages.last().and_then(|s| s.iter().map(|l| l.output + 1).max()).unwrap_or(0),
            lanes: table,
            stages: ranges,
            state: vec![[0.0; 2]; coefs.len()],
            coefs,
            scratch: vec![0.0; max_block],
            between,
        }
    }

    pub fn inputs(&self) -> usize {
        self.inputs
    }

    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Like [`IirBank::set_lane`]; direct form II keeps its state natively.
    pub fn set_lane(&mut self, lane: usize, sections: &[Biquad]) {
        let Some(&(first, count, ..)) = self.lanes.get(lane) else { return };
        let n = count.min(sections.len());
        self.coefs[first..first + n].copy_from_slice(&sections[..n]);
    }

    pub fn reset(&mut self) {
        self.state.fill([0.0; 2]);
    }

    /// Replaces the filter state (direct form II, one pair per section).
    pub fn set_state(&mut self, state: &[[f32; 2]]) {
        let n = state.len().min(self.state.len());
        self.state[..n].copy_from_slice(&state[..n]);
    }

    /// Sets one section's coefficients (sections numbered across all lanes).
    fn set_section(&mut self, section: usize, coefs: Biquad) {
        if let Some(c) = self.coefs.get_mut(section) {
            *c = coefs;
        }
    }

    /// Filters one block; see [`IirBank::process`].
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0);
        self.process_with(inputs, outputs, n);
    }

    /// [`process`](Self::process) for any channel containers, `n` frames.
    pub(crate) fn process_with<I: AsRef<[f32]>, O: AsMut<[f32]>>(&mut self, inputs: &[I], outputs: &mut [O], n: usize) {
        let n = n.min(self.scratch.len());
        if inputs.len() < self.inputs || outputs.len() < self.outputs {
            return;
        }
        let last = self.stages.len() - 1;
        for k in 0..=last {
            let lanes = &self.lanes[self.stages[k].clone()];
            let (done, rest) = self.between.split_at_mut(k);
            let (coefs, state, scratch) = (&self.coefs, &mut self.state, &mut self.scratch);
            match (k == 0, k == last) {
                (true, true) => run_stage(lanes, coefs, state, scratch, inputs, outputs, n),
                (true, false) => run_stage(lanes, coefs, state, scratch, inputs, &mut rest[0], n),
                (false, true) => run_stage(lanes, coefs, state, scratch, &done[k - 1], outputs, n),
                (false, false) => run_stage(lanes, coefs, state, scratch, &done[k - 1], &mut rest[0], n),
            }
        }
    }
}

/// One stage of a [`CpuIirBank`].
fn run_stage<I: AsRef<[f32]>, O: AsMut<[f32]>>(
    lanes: &[(usize, usize, usize, usize)],
    coefs: &[Biquad],
    state: &mut [[f32; 2]],
    scratch: &mut [f32],
    inputs: &[I],
    outputs: &mut [O],
    n: usize,
) {
    outputs.iter_mut().for_each(|c| c.as_mut()[..n].fill(0.0));
    let signal = &mut scratch[..n];
    for &(first, count, input, output) in lanes {
        signal.copy_from_slice(&inputs[input].as_ref()[..n]);
        for (c, s) in coefs[first..first + count].iter().zip(&mut state[first..first + count]) {
            for x in signal.iter_mut() {
                let y = c.b0 * *x + s[0];
                s[0] = c.b1 * *x - c.a1 * y + s[1];
                s[1] = c.b2 * *x - c.a2 * y;
                *x = y;
            }
        }
        outputs[output].as_mut()[..n].iter_mut().zip(signal.iter()).for_each(|(o, &y)| *o += y);
    }
}

/// Exchange between the audio thread and the GPU thread of a
/// [`RealtimeIirBank`]. Frames are interleaved.
struct Exchange {
    input: Vec<f32>,
    /// Input position of the first frame in `input`.
    input_start: u64,
    output: Vec<f32>,
    /// Frames the GPU thread has produced in all, and the filter state after
    /// the last of them (direct form II), for the CPU fallback.
    produced: u64,
    state: Vec<[f32; 2]>,
    /// Coefficient updates for the GPU thread to apply before its next block.
    coefs: Vec<Biquad>,
    dirty: Vec<bool>,
}

struct Shared {
    exchange: Mutex<Exchange>,
    /// The GPU thread fell too far behind and lost samples: from now on the
    /// CPU fallback (if on) computes everything.
    broken: AtomicBool,
    /// The CPU fallback is on: the GPU thread reads back the state.
    capture: AtomicBool,
    last_gpu_micros: AtomicU64,
    /// Test hook: the GPU thread sleeps this long before its next batch.
    stall_micros: AtomicU64,
}

/// The CPU copy of a bank that fills in for frames the GPU delivers late
/// (design doc 9.2). It starts from the GPU's state at that frame, so its
/// frames are the ones the GPU would have produced.
struct Fallback {
    cpu: CpuIirBank,
    /// Input frames by position, a ring per channel.
    history: Vec<Vec<f32>>,
    /// The GPU's state after `snapshot_pos` frames.
    snapshot: Vec<[f32; 2]>,
    snapshot_pos: u64,
    /// Frame at which the CPU copy's own state is valid (`u64::MAX`: none).
    cpu_pos: u64,
    /// Coefficient changes (input position, section, coefficients), at the
    /// position the GPU applies them.
    changes: VecDeque<(u64, usize, Biquad)>,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
}

/// An [`IirBank`] for the audio thread. The GPU work runs on a GPU thread
/// shared by all real-time banks of the process, which batches the banks of
/// all instances into one submission per block; [`process`](Self::process)
/// never calls the GPU API and never waits.
/// Output is delayed by [`latency`](Self::latency) frames (report it to the
/// host).
///
/// When the GPU delivers a frame too late, a CPU copy of the bank computes it
/// instead, starting from the state the GPU sent back with its last frames:
/// the output is the same, only that block costs CPU time. If the GPU stops
/// altogether, the CPU carries on. [`set_fallback`](Self::set_fallback)
/// turns this off (late frames are then silent).
pub struct RealtimeIirBank {
    shared: Arc<Shared>,
    /// Keeps this bank registered with the batcher.
    _slot: Arc<Slot>,
    batcher: Arc<Batcher<Slot>>,
    inputs: usize,
    outputs: usize,
    max_block: usize,
    latency: u64,
    pos: u64,
    /// Input frames not yet handed to the GPU thread, from position `unsent_start`.
    unsent: Vec<f32>,
    unsent_start: u64,
    /// Output frames from the GPU thread, not yet played; the last one is
    /// frame `received - 1`.
    ready: VecDeque<f32>,
    received: u64,
    lane_sections: Vec<(usize, usize)>,
    coefs: Vec<Biquad>,
    dirty: Vec<bool>,
    fallback: Box<Fallback>,
    use_fallback: bool,
    misses: u64,
    fallbacks: u64,
    adapter: String,
}

/// Blocks of input the exchange can hold before the GPU thread counts as stalled.
const QUEUE_BLOCKS: usize = 16;

impl RealtimeIirBank {
    /// Prepares the bank and starts its GPU thread. Call off the audio thread.
    pub fn new(lanes: &[Lane], max_block: usize) -> Result<Self, String> {
        Self::chain(&[lanes], max_block)
    }

    /// A chain of banks in series ([`IirBank::chain`]): one GPU round trip
    /// per block for the whole chain, and the latency of a single bank.
    pub fn chain(stages: &[&[Lane]], max_block: usize) -> Result<Self, String> {
        let mut bank = IirBank::chain(stages, max_block)?;
        let cpu = CpuIirBank::chain(stages, max_block)?;
        let (inputs, outputs, max_block) = (bank.inputs, bank.outputs, bank.max_block);
        let (lane_sections, coefs) = (bank.lane_sections.clone(), bank.coefs.clone());
        let sections = coefs.len();
        let adapter = bank.adapter().to_owned();
        bank.capture_state = true;
        let latency = 2 * max_block as u64;
        let history = (latency as usize + max_block).next_power_of_two();
        let shared = Arc::new(Shared {
            exchange: Mutex::new(Exchange {
                input: Vec::with_capacity(inputs * max_block * QUEUE_BLOCKS),
                input_start: 0,
                output: Vec::with_capacity(outputs * max_block * QUEUE_BLOCKS),
                produced: 0,
                state: vec![[0.0; 2]; sections],
                coefs: coefs.clone(),
                dirty: vec![false; lane_sections.len()],
            }),
            broken: AtomicBool::new(false),
            capture: AtomicBool::new(true),
            last_gpu_micros: AtomicU64::new(0),
            stall_micros: AtomicU64::new(0),
        });
        let slot = Arc::new(Slot {
            shared: shared.clone(),
            work: Mutex::new(SlotWork {
                frames: Vec::with_capacity(inputs * max_block * QUEUE_BLOCKS),
                done: 0,
                round: 0,
                results: Vec::with_capacity(outputs * max_block * QUEUE_BLOCKS),
                ins: vec![vec![0.0; max_block]; inputs],
                outs: vec![vec![0.0; max_block]; outputs],
                produced: 0,
                bank,
            }),
        });
        let batcher = BANKS.register("ferritron-gpu-batch", run_batcher, slot.clone())?;
        Ok(Self {
            shared,
            _slot: slot,
            batcher,
            inputs,
            outputs,
            max_block,
            latency,
            pos: 0,
            unsent: Vec::with_capacity(inputs * max_block * QUEUE_BLOCKS),
            unsent_start: 0,
            ready: VecDeque::with_capacity(outputs * max_block * QUEUE_BLOCKS * 2),
            received: 0,
            dirty: vec![false; lane_sections.len()],
            lane_sections,
            coefs,
            fallback: Box::new(Fallback {
                cpu,
                history: vec![vec![0.0; history]; inputs],
                snapshot: vec![[0.0; 2]; sections],
                snapshot_pos: 0,
                cpu_pos: u64::MAX,
                changes: VecDeque::with_capacity(sections * 4 + 64),
                scratch_in: vec![vec![0.0; max_block]; inputs],
                scratch_out: vec![vec![0.0; max_block]; outputs],
            }),
            use_fallback: true,
            misses: 0,
            fallbacks: 0,
            adapter,
        })
    }

    /// The GPU in use.
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Output delay in frames: twice `max_block`, so the GPU always gets at
    /// least one block of time with fixed-size blocks.
    pub fn latency(&self) -> usize {
        self.latency as usize
    }

    /// Blocks in which frames came neither from the GPU in time nor from the
    /// CPU fallback (they played silent).
    pub fn missed_blocks(&self) -> u64 {
        self.misses
    }

    /// Blocks in which the CPU fallback computed frames the GPU was late with.
    pub fn fallback_blocks(&self) -> u64 {
        self.fallbacks
    }

    /// Turns the CPU fallback on (the default) or off. Off saves reading the
    /// state back; late frames are then silent. Call before processing.
    pub fn set_fallback(&mut self, on: bool) {
        self.use_fallback = on;
        self.shared.capture.store(on, Relaxed);
    }

    /// GPU time of the most recent block.
    pub fn last_gpu_micros(&self) -> u64 {
        self.shared.last_gpu_micros.load(Relaxed)
    }

    /// Test hook: makes the GPU thread sleep before its next batch.
    #[doc(hidden)]
    pub fn debug_stall_gpu(&self, micros: u64) {
        self.shared.stall_micros.store(micros, Relaxed);
    }

    /// Test hook: behaves as if the GPU had failed for good.
    #[doc(hidden)]
    pub fn debug_break_gpu(&self) {
        self.shared.broken.store(true, Relaxed);
    }

    /// Waits until the GPU thread has filtered all input given so far, at
    /// most `timeout`; returns whether it caught up. Call off the audio
    /// thread after feeding blocks faster than real time (e.g. to measure
    /// the bank), or the backlog makes the first real-time blocks late.
    pub fn settle(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.exchange();
            if self.received >= self.pos || self.shared.broken.load(Relaxed) {
                return self.received >= self.pos;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
    }

    /// Replaces the coefficients of `lane`; see [`IirBank::set_lane`].
    /// Real-time safe. Takes effect when the GPU thread next picks up input.
    pub fn set_lane(&mut self, lane: usize, sections: &[Biquad]) {
        let Some(&(first, count)) = self.lane_sections.get(lane) else { return };
        let n = count.min(sections.len());
        self.coefs[first..first + n].copy_from_slice(&sections[..n]);
        self.dirty[lane] = true;
    }

    /// Filters one block, delayed by [`latency`](Self::latency). Real-time
    /// safe: no allocation, never waits. At most `max_block` frames.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0).min(self.max_block);
        if inputs.len() < self.inputs || outputs.len() < self.outputs || n == 0 {
            return;
        }
        let fb = &mut self.fallback;
        let mask = fb.history.first().map_or(0, |h| h.len() - 1);
        for (ring, input) in fb.history.iter_mut().zip(inputs) {
            for (i, &x) in input[..n].iter().enumerate() {
                ring[(self.pos as usize + i) & mask] = x;
            }
        }
        if !self.shared.broken.load(Relaxed) {
            if self.unsent.len() + n * self.inputs > self.unsent.capacity() {
                self.shared.broken.store(true, Relaxed);
            } else {
                if self.unsent.is_empty() {
                    self.unsent_start = self.pos;
                }
                for i in 0..n {
                    self.unsent.extend(inputs.iter().take(self.inputs).map(|c| c[i]));
                }
            }
        }
        self.exchange();

        let mut i = 0;
        while i < n && self.pos + (i as u64) < self.latency {
            outputs.iter_mut().take(self.outputs).for_each(|c| c[i] = 0.0);
            i += 1;
        }
        while i < n {
            let frame = self.pos + i as u64 - self.latency;
            let mut start = self.received - (self.ready.len() / self.outputs) as u64;
            if start < frame {
                // The CPU played these already: drop the late GPU frames.
                let late = ((frame - start) as usize).min(self.ready.len() / self.outputs);
                self.ready.drain(..late * self.outputs);
                start += late as u64;
            }
            if start != frame || self.ready.len() < self.outputs {
                break;
            }
            for channel in outputs.iter_mut().take(self.outputs) {
                channel[i] = self.ready.pop_front().unwrap_or(0.0);
            }
            i += 1;
        }
        if i < n {
            self.fill_late(i, n, outputs);
        }
        self.pos += n as u64;
    }

    /// Frames `from..n` of this block have no GPU output: computes them on
    /// the CPU, or plays silence.
    fn fill_late(&mut self, from: usize, n: usize, outputs: &mut [&mut [f32]]) {
        let first = self.pos + from as u64 - self.latency;
        let end = first + (n - from) as u64;
        let fb = &mut *self.fallback;
        let start_ok = self.use_fallback
            && if fb.cpu_pos == first {
                true
            } else if fb.snapshot_pos == first {
                fb.cpu.set_state(&fb.snapshot);
                true
            } else {
                false
            };
        if !start_ok {
            outputs.iter_mut().take(self.outputs).for_each(|c| c[from..n].fill(0.0));
            fb.cpu_pos = u64::MAX;
            self.misses += 1;
            return;
        }
        let mask = fb.history.first().map_or(0, |h| h.len() - 1);
        let mut at = first;
        while at < end {
            while let Some(&(pos, section, coefs)) = fb.changes.front() {
                if pos > at {
                    break;
                }
                fb.cpu.set_section(section, coefs);
                fb.changes.pop_front();
            }
            let next = fb.changes.front().map_or(end, |c| c.0.min(end));
            let len = (next - at) as usize;
            for (scratch, ring) in fb.scratch_in.iter_mut().zip(&fb.history) {
                for (k, x) in scratch[..len].iter_mut().enumerate() {
                    *x = ring[(at as usize + k) & mask];
                }
            }
            fb.cpu.process_with(&fb.scratch_in, &mut fb.scratch_out, len);
            let offset = from + (at - first) as usize;
            for (out, scratch) in outputs.iter_mut().zip(&fb.scratch_out) {
                out[offset..offset + len].copy_from_slice(&scratch[..len]);
            }
            at = next;
        }
        fb.cpu_pos = end;
        self.fallbacks += 1;
    }

    /// Hands over input and coefficients, and takes finished output and the
    /// GPU state. Never waits: if the GPU thread holds the lock, everything
    /// waits a block.
    fn exchange(&mut self) {
        let Ok(mut ex) = self.shared.exchange.try_lock() else { return };
        let ex = &mut *ex;
        let fb = &mut *self.fallback;
        let broken = self.shared.broken.load(Relaxed);
        // Where new coefficients take effect: the GPU applies them before
        // the first frame it picks up next.
        let mut applies_at = None;
        if broken {
            applies_at = Some(self.pos);
        } else if !self.unsent.is_empty() {
            if ex.input.len() + self.unsent.len() > ex.input.capacity() {
                self.shared.broken.store(true, Relaxed);
                applies_at = Some(self.pos);
            } else {
                if ex.input.is_empty() {
                    ex.input_start = self.unsent_start;
                }
                applies_at = Some(ex.input_start);
                ex.input.extend_from_slice(&self.unsent);
                self.unsent.clear();
            }
        }
        if let Some(at) = applies_at {
            for (lane, dirty) in self.dirty.iter_mut().enumerate() {
                if !*dirty {
                    continue;
                }
                let (first, count) = self.lane_sections[lane];
                ex.coefs[first..first + count].copy_from_slice(&self.coefs[first..first + count]);
                ex.dirty[lane] = true;
                for section in first..first + count {
                    if fb.changes.len() == fb.changes.capacity() {
                        // Full: apply the oldest change now (approximately).
                        if let Some((_, s, c)) = fb.changes.pop_front() {
                            fb.cpu.set_section(s, c);
                        }
                    }
                    fb.changes.push_back((at, section, self.coefs[section]));
                }
                *dirty = false;
            }
        }
        if !ex.output.is_empty() {
            if self.ready.len() + ex.output.len() > self.ready.capacity() {
                self.shared.broken.store(true, Relaxed);
            } else {
                self.received += (ex.output.len() / self.outputs) as u64;
                self.ready.extend(ex.output.drain(..));
            }
        }
        if ex.produced == self.received && fb.snapshot_pos != ex.produced {
            fb.snapshot.copy_from_slice(&ex.state);
            fb.snapshot_pos = ex.produced;
        }
        self.batcher.notify();
    }
}

/// The GPU thread shared by every [`RealtimeIirBank`] of the process (see
/// `batch`): it collects the pending blocks of all banks and runs them
/// together, one submission per round.
static BANKS: Registry<Slot> = Registry::new();

/// A bank registered with the batcher, until its [`RealtimeIirBank`] drops.
struct Slot {
    shared: Arc<Shared>,
    work: Mutex<SlotWork>,
}

/// GPU-thread side of one bank.
struct SlotWork {
    bank: IirBank,
    /// Input frames taken from the exchange (interleaved), `done` of them processed.
    frames: Vec<f32>,
    done: usize,
    /// Frames in the current round.
    round: usize,
    results: Vec<f32>,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    produced: u64,
}

impl Drop for RealtimeIirBank {
    fn drop(&mut self) {
        BANKS.unregister(&self._slot);
    }
}

fn run_batcher(batcher: &Batcher<Slot>) {
    while batcher.wait() {
        let slots = batcher.slots();
        let stall = slots.iter().map(|s| s.shared.stall_micros.swap(0, Relaxed)).max().unwrap_or(0);
        if stall > 0 {
            std::thread::sleep(std::time::Duration::from_micros(stall));
        }

        // Take every bank's pending input.
        let mut works = Vec::new();
        let mut owners = Vec::new();
        for slot in &slots {
            let mut work = slot.work.lock().unwrap_or_else(PoisonError::into_inner);
            {
                let mut ex = slot.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
                if ex.input.is_empty() {
                    continue;
                }
                std::mem::swap(&mut work.frames, &mut ex.input);
                let ex = &mut *ex;
                for (lane, dirty) in ex.dirty.iter_mut().enumerate() {
                    if *dirty {
                        let (first, count) = work.bank.lane_sections[lane];
                        work.bank.set_lane(lane, &ex.coefs[first..first + count]);
                        *dirty = false;
                    }
                }
            }
            work.bank.capture_state = slot.shared.capture.load(Relaxed);
            work.results.clear();
            work.done = 0;
            works.push(work);
            owners.push(slot);
        }
        if works.is_empty() {
            continue;
        }

        // Rounds: the next block of every bank that has frames left, in one submission.
        let begun = Instant::now();
        loop {
            let mut jobs = Vec::with_capacity(works.len());
            for work in works.iter_mut() {
                let w = &mut **work;
                let (inputs, max_block) = (w.bank.inputs.max(1), w.bank.max_block);
                let n = (w.frames.len() / inputs - w.done).min(max_block);
                w.round = n;
                if n == 0 {
                    continue;
                }
                for (i, frame) in w.frames[w.done * inputs..(w.done + n) * inputs].chunks_exact(inputs).enumerate() {
                    for (ch, &v) in frame.iter().enumerate().take(w.ins.len()) {
                        w.ins[ch][i] = v;
                    }
                }
                w.done += n;
                jobs.push(BatchJob { bank: &mut w.bank, inputs: &w.ins[..], outputs: &mut w.outs[..], frames: n });
            }
            if jobs.is_empty() {
                break;
            }
            IirBank::process_many(&mut jobs);
            drop(jobs);
            for work in works.iter_mut() {
                let w = &mut **work;
                for i in 0..w.round {
                    w.results.extend(w.outs.iter().map(|c| c[i]));
                }
                w.produced += w.round as u64;
            }
        }
        let micros = begun.elapsed().as_micros() as u64;

        for (work, slot) in works.iter_mut().zip(&owners) {
            work.frames.clear();
            slot.shared.last_gpu_micros.store(micros, Relaxed);
            let mut ex = slot.shared.exchange.lock().unwrap_or_else(PoisonError::into_inner);
            if ex.output.len() + work.results.len() > ex.output.capacity() {
                slot.shared.broken.store(true, Relaxed);
            } else {
                ex.output.extend_from_slice(&work.results);
                if work.bank.capture_state {
                    ex.state.copy_from_slice(&work.bank.state_out);
                }
                ex.produced = work.produced;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// Transposed direct form II in f64, sample by sample: the reference.
    struct Reference {
        lanes: Vec<Lane>,
        state: Vec<Vec<[f64; 2]>>,
    }

    impl Reference {
        fn new(lanes: &[Lane]) -> Self {
            Self { lanes: lanes.to_vec(), state: lanes.iter().map(|l| vec![[0.0; 2]; l.sections.len()]).collect() }
        }

        fn process(&mut self, inputs: &[Vec<f32>], outputs: usize, from: usize, n: usize) -> Vec<Vec<f32>> {
            let mut out = vec![vec![0.0f64; n]; outputs];
            for (lane, state) in self.lanes.iter().zip(&mut self.state) {
                for i in 0..n {
                    let mut x = inputs[lane.input][from + i] as f64;
                    for (c, s) in lane.sections.iter().zip(state.iter_mut()) {
                        let y = c.b0 as f64 * x + s[0];
                        s[0] = c.b1 as f64 * x - c.a1 as f64 * y + s[1];
                        s[1] = c.b2 as f64 * x - c.a2 as f64 * y;
                        x = y;
                    }
                    out[lane.output][i] += x;
                }
            }
            out.into_iter().map(|c| c.into_iter().map(|v| v as f32).collect()).collect()
        }
    }

    /// The same filters run sample by sample in f32: how close a plain CPU
    /// implementation gets to the f64 reference.
    fn serial_f32(lanes: &[Lane], input: &[f32], outputs: usize) -> Vec<Vec<f32>> {
        let mut out = vec![vec![0.0f32; input.len()]; outputs];
        for lane in lanes {
            let mut state = vec![[0.0f32; 2]; lane.sections.len()];
            for (i, &v) in input.iter().enumerate() {
                let mut x = v;
                for (c, s) in lane.sections.iter().zip(state.iter_mut()) {
                    let y = c.b0 * x + s[0];
                    s[0] = c.b1 * x - c.a1 * y + s[1];
                    s[1] = c.b2 * x - c.a2 * y;
                    x = y;
                }
                out[lane.output][i] += x;
            }
        }
        out
    }

    fn mixed_lanes(count: usize, inputs: usize, outputs: usize) -> Vec<Lane> {
        let rate = 48_000.0;
        (0..count)
            .map(|i| {
                let f = 60.0 * 1.13f64.powi(i as i32 % 40); // up to 7 kHz
                let sections = match i % 4 {
                    0 => vec![Biquad::lowpass(f, 0.707, rate)],
                    1 => vec![Biquad::highpass(f, 0.9, rate), Biquad::peaking(f * 2.0, 2.0, 6.0, rate)],
                    2 => vec![Biquad::bandpass(f, 30.0, rate)],
                    _ => vec![Biquad::peaking(f, 1.0, -9.0, rate), Biquad::lowpass(f * 3.0, 0.5, rate), Biquad::IDENTITY],
                };
                Lane { input: i % inputs, output: (i * 7) % outputs, sections }
            })
            .collect()
    }

    fn assert_close(got: &[f32], want: &[f32], what: &str) {
        let peak = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max).max(1e-3);
        let worst = got.iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 2e-4 * peak.max(1.0), "{what}: largest difference {worst} (peak {peak})");
    }

    #[test]
    fn bank_matches_direct_filtering_with_any_block_size() {
        let _serial = crate::serial();
        let lanes = mixed_lanes(70, 2, 3);
        let mut bank = match IirBank::new(&lanes, 1024) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let total = 6000;
        let inputs: Vec<Vec<f32>> = (0..2).map(|c| noise(total, 5 + c)).collect();
        let mut reference = Reference::new(&lanes);
        let (mut at, mut i) = (0, 0);
        let sizes = [1024, 1, 37, 256, 1000, 3, 511];
        while at < total {
            let n = sizes[i % sizes.len()].min(total - at);
            if i == 4 {
                // Change coefficients between blocks, as automation would.
                let new = [Biquad::lowpass(2000.0, 4.0, 48_000.0), Biquad::IDENTITY];
                bank.set_lane(1, &new);
                reference.lanes[1].sections = new.to_vec();
            }
            let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[at..at + n]).collect();
            let mut outs = vec![vec![0.0f32; n]; 3];
            bank.process(&ins, &mut outs.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            let want = reference.process(&inputs, 3, at, n);
            for o in 0..3 {
                assert_close(&outs[o], &want[o], &format!("block {i} ({n} frames), output {o}"));
            }
            at += n;
            i += 1;
        }
    }

    #[test]
    fn long_blocks_with_resonant_filters_stay_accurate() {
        let _serial = crate::serial();
        // Offline rendering: 8192-frame blocks, 32 frames per thread, and
        // resonators that ring for a long time.
        let lanes: Vec<Lane> = (0..64)
            .map(|i| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(100.0 + 150.0 * i as f64, 300.0, 48_000.0)] })
            .collect();
        let mut bank = match IirBank::new(&lanes, 8192) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let mut input = vec![0.0f32; 8192 * 4];
        input[0] = 1.0;
        input[20_000] = -0.5;
        let mut reference = Reference::new(&lanes);
        for block in 0..4 {
            let at = block * 8192;
            let mut out = vec![0.0f32; 8192];
            bank.process(&[&input[at..at + 8192]], &mut [&mut out[..]]);
            let want = reference.process(std::slice::from_ref(&input), 1, at, 8192);
            assert_close(&out, &want[0], &format!("block {block}"));
        }
    }

    #[test]
    fn realtime_bank_is_the_bank_delayed_by_its_latency() {
        let _serial = crate::serial();
        let lanes = mixed_lanes(24, 1, 2);
        let mut rt = match RealtimeIirBank::new(&lanes, 256) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let total = 256 * 60;
        let input = noise(total, 9);
        let mut got = vec![Vec::new(); 2];
        for at in (0..total).step_by(256) {
            let mut outs = vec![vec![0.0f32; 256]; 2];
            rt.process(&[&input[at..at + 256]], &mut outs.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            for o in 0..2 {
                got[o].extend_from_slice(&outs[o]);
            }
            std::thread::sleep(std::time::Duration::from_micros(256 * 1_000_000 / 48_000));
        }
        assert_eq!(rt.missed_blocks(), 0, "missed GPU deadlines");
        let want = Reference::new(&lanes).process(std::slice::from_ref(&input), 2, 0, total);
        let latency = rt.latency();
        let serial = serial_f32(&lanes, &input, 2);
        for o in 0..2 {
            // No less accurate than a plain f32 biquad on the CPU.
            let err = |a: &[f32]| a.iter().zip(&want[o]).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
            let (cpu, gpu) = (err(&serial[o]), err(&got[o][latency..]));
            assert!(gpu <= cpu.max(1e-6) * 2.0, "output {o}: GPU error {gpu}, f32 CPU error {cpu}");
            assert!(got[o][..latency].iter().all(|&v| v == 0.0));
            assert_close(&got[o][latency..], &want[o][..total - latency], &format!("output {o}"));
        }
    }

    /// Three stages: 2 inputs → 6 band filters (6 channels) → 6 resonators
    /// summed per pair (3 channels) → per-channel EQ (3 outputs).
    fn three_stages() -> Vec<Vec<Lane>> {
        let rate = 48_000.0;
        let bands = (0..6).map(|i| Lane { input: i % 2, output: i, sections: vec![Biquad::bandpass(200.0 * (i + 1) as f64, 2.0, rate)] });
        let resonators =
            (0..6).map(|i| Lane { input: i, output: i / 2, sections: vec![Biquad::bandpass(300.0 + 170.0 * i as f64, 40.0, rate)] });
        let eq = (0..3).map(|o| Lane {
            input: o,
            output: o,
            sections: vec![Biquad::peaking(1000.0, 1.0, 6.0, rate), Biquad::highpass(80.0, 0.7, rate)],
        });
        vec![bands.collect(), resonators.collect(), eq.collect()]
    }

    fn refs(stages: &[Vec<Lane>]) -> Vec<&[Lane]> {
        stages.iter().map(|s| &s[..]).collect()
    }

    #[test]
    fn a_cpu_chain_is_its_banks_in_series() {
        let _serial = crate::serial();
        let stages = three_stages();
        let input: Vec<Vec<f32>> = (0..2).map(|c| noise(3000, 40 + c)).collect();
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let mut chain = CpuIirBank::chain(&refs(&stages), 3000).unwrap();
        let mut fused = vec![vec![0.0f32; 3000]; 3];
        chain.process(&ins, &mut fused.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
        // The same banks, one after another.
        let mut signal = input.clone();
        for stage in &stages {
            let mut bank = CpuIirBank::new(stage, 3000);
            let mut next = vec![vec![0.0f32; 3000]; bank.outputs()];
            let ins: Vec<&[f32]> = signal.iter().map(|c| &c[..]).collect();
            bank.process(&ins, &mut next.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            signal = next;
        }
        assert_eq!(fused, signal);
    }

    #[test]
    fn a_gpu_chain_matches_the_cpu_chain_in_one_round_trip() {
        let _serial = crate::serial();
        let stages = three_stages();
        let Ok(mut gpu) = IirBank::chain(&refs(&stages), 1024) else { return eprintln!("skipping: no GPU") };
        let mut cpu = CpuIirBank::chain(&refs(&stages), 1024).unwrap();
        assert_eq!((gpu.inputs(), gpu.outputs(), gpu.lanes()), (2, 3, 15));
        let input: Vec<Vec<f32>> = (0..2).map(|c| noise(6000, 70 + c)).collect();
        let (mut at, mut i) = (0, 0);
        while at < 6000 {
            let n = [1024, 7, 300, 1000][i % 4].min(6000 - at);
            if i == 3 {
                // Automation in the middle stage (lane 6 + 2) and the last (lane 12).
                let change = [Biquad::bandpass(900.0, 10.0, 48_000.0)];
                gpu.set_lane(8, &change);
                cpu.set_lane(8, &change);
                let eq = [Biquad::peaking(2000.0, 2.0, -6.0, 48_000.0), Biquad::highpass(120.0, 0.7, 48_000.0)];
                gpu.set_lane(12, &eq);
                cpu.set_lane(12, &eq);
            }
            let ins: Vec<&[f32]> = input.iter().map(|c| &c[at..at + n]).collect();
            let mut want = vec![vec![0.0f32; n]; 3];
            let mut got = vec![vec![0.0f32; n]; 3];
            cpu.process(&ins, &mut want.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            gpu.process(&ins, &mut got.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            for o in 0..3 {
                assert_close(&got[o], &want[o], &format!("block {i}, output {o}"));
            }
            at += n;
            i += 1;
        }
    }

    #[test]
    fn a_chain_cannot_read_channels_the_previous_stage_lacks() {
        let _serial = crate::serial();
        let mut stages = three_stages();
        stages[2][0].input = 5; // stage 1 has only 3 outputs
        assert!(CpuIirBank::chain(&refs(&stages), 64).is_err());
        assert!(IirBank::chain(&refs(&stages), 64).is_err());
    }

    /// Plays noise through a real-time bank at real-time pace; `event` runs
    /// before each block. Compares with the f64 reference (delayed by the
    /// latency) and returns (missed blocks, fallback blocks).
    fn play_with(event: impl Fn(usize, &mut RealtimeIirBank, &mut Reference)) -> Option<(u64, u64)> {
        let lanes = mixed_lanes(24, 1, 2);
        let mut rt = match RealtimeIirBank::new(&lanes, 256) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: {e}");
                return None;
            }
        };
        let mut reference = Reference::new(&lanes);
        let blocks = 70;
        let input = noise(256 * blocks, 13);
        let (mut got, mut want) = (vec![Vec::new(); 2], vec![Vec::new(); 2]);
        for b in 0..blocks {
            event(b, &mut rt, &mut reference);
            let at = b * 256;
            let mut outs = vec![vec![0.0f32; 256]; 2];
            rt.process(&[&input[at..at + 256]], &mut outs.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            let r = reference.process(std::slice::from_ref(&input), 2, at, 256);
            for o in 0..2 {
                got[o].extend_from_slice(&outs[o]);
                want[o].extend_from_slice(&r[o]);
            }
            std::thread::sleep(std::time::Duration::from_micros(256 * 1_000_000 / 48_000));
        }
        let latency = rt.latency();
        for o in 0..2 {
            assert_close(&got[o][latency..], &want[o][..want[o].len() - latency], &format!("output {o}"));
        }
        Some((rt.missed_blocks(), rt.fallback_blocks()))
    }

    #[test]
    fn a_stalled_gpu_is_covered_by_the_cpu_without_a_trace() {
        let _serial = crate::serial();
        // The GPU sleeps 30 ms (about six blocks) once.
        let Some((missed, fallbacks)) = play_with(|b, rt, _| {
            if b == 20 {
                rt.debug_stall_gpu(30_000);
            }
        }) else {
            return;
        };
        assert_eq!(missed, 0, "no silent frames");
        assert!(fallbacks >= 2, "the CPU filled in ({fallbacks} blocks)");
    }

    #[test]
    fn automation_before_a_stall_is_followed_by_the_cpu() {
        let _serial = crate::serial();
        let Some((missed, fallbacks)) = play_with(|b, rt, reference| {
            if b == 15 {
                let new = [Biquad::lowpass(1500.0, 3.0, 48_000.0), Biquad::peaking(500.0, 1.0, 6.0, 48_000.0)];
                rt.set_lane(1, &new);
                reference.lanes[1].sections = new.to_vec();
            }
            if b == 16 {
                rt.debug_stall_gpu(30_000);
            }
        }) else {
            return;
        };
        assert_eq!(missed, 0);
        assert!(fallbacks >= 2, "the CPU filled in ({fallbacks} blocks)");
    }

    #[test]
    fn when_the_gpu_fails_the_cpu_carries_on() {
        let _serial = crate::serial();
        let Some((missed, fallbacks)) = play_with(|b, rt, reference| {
            if b == 20 {
                rt.debug_break_gpu();
            }
            if b == 40 {
                // Automation still works on the CPU alone.
                let new = [Biquad::highpass(700.0, 0.7, 48_000.0), Biquad::peaking(2000.0, 2.0, -4.0, 48_000.0)];
                rt.set_lane(1, &new);
                reference.lanes[1].sections = new.to_vec();
            }
        }) else {
            return;
        };
        assert_eq!(missed, 0);
        assert!(fallbacks >= 40, "the CPU took over ({fallbacks} blocks)");
    }

    #[test]
    fn many_banks_in_one_submission_match_one_by_one() {
        let _serial = crate::serial();
        let lanes: Vec<Vec<Lane>> = (0..6).map(|k| mixed_lanes(10 + k * 7, 1, 2)).collect();
        let Ok(mut batched) = lanes.iter().map(|l| IirBank::new(l, 512)).collect::<Result<Vec<_>, _>>() else {
            return eprintln!("skipping: no GPU");
        };
        let mut single: Vec<IirBank> = lanes.iter().map(|l| IirBank::new(l, 512).unwrap()).collect();
        let input = noise(512, 99);
        let ins = [input.clone()];
        let mut outs: Vec<Vec<Vec<f32>>> = vec![vec![vec![0.0; 512]; 2]; 6];
        // Different block lengths per bank in the same submission.
        let frames = [512, 300, 1, 512, 77, 256];
        let mut jobs: Vec<BatchJob<'_, Vec<f32>, Vec<f32>>> = batched
            .iter_mut()
            .zip(outs.iter_mut())
            .zip(frames)
            .map(|((bank, out), frames)| BatchJob { bank, inputs: &ins, outputs: &mut out[..], frames })
            .collect();
        IirBank::process_many(&mut jobs);
        drop(jobs);
        for (k, bank) in single.iter_mut().enumerate() {
            let n = frames[k];
            let mut want = vec![vec![0.0f32; n]; 2];
            bank.process(&[&input[..n]], &mut want.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            for o in 0..2 {
                assert_eq!(&outs[k][o][..n], &want[o][..], "bank {k}, output {o}");
            }
        }
    }

    #[test]
    fn instances_share_the_gpu_thread_and_stay_exact() {
        let _serial = crate::serial();
        // Eight instances, as on eight tracks, each with its own filters.
        let lanes: Vec<Vec<Lane>> = (0..8).map(|k| mixed_lanes(12 + k, 1, 1)).collect();
        let Ok(mut banks) = lanes.iter().map(|l| RealtimeIirBank::new(l, 256)).collect::<Result<Vec<_>, _>>() else {
            return eprintln!("skipping: no GPU");
        };
        let blocks = 50;
        let input = noise(256 * blocks, 5);
        let mut got = vec![Vec::new(); banks.len()];
        for b in 0..blocks {
            for (bank, out) in banks.iter_mut().zip(got.iter_mut()) {
                let mut buf = vec![0.0f32; 256];
                bank.process(&[&input[b * 256..(b + 1) * 256]], &mut [&mut buf]);
                out.extend_from_slice(&buf);
            }
            std::thread::sleep(std::time::Duration::from_micros(256 * 1_000_000 / 48_000));
        }
        for (k, (bank, out)) in banks.iter().zip(&got).enumerate() {
            assert_eq!(bank.missed_blocks(), 0, "instance {k}");
            let want = Reference::new(&lanes[k]).process(std::slice::from_ref(&input), 1, 0, input.len());
            let latency = bank.latency();
            assert_close(&out[latency..], &want[0][..input.len() - latency], &format!("instance {k}"));
        }
    }
}
