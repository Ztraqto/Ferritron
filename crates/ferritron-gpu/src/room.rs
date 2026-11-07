//! Room acoustics with ray tracing (design doc 9.8: RT cores): the energy
//! that reaches a listener over time, from rays sent from a source and
//! bounced off the room's walls, and an impulse response made from it.
//!
//! On a GPU with ray queries, the rays run on its RT cores against an
//! acceleration structure of the room; elsewhere on the CPU. Both run the
//! same algorithm with the same random numbers:
//!
//! - rays leave the source evenly in every direction (a Fibonacci sphere);
//! - at each wall a ray loses the wall's absorption, and is reflected like
//!   a mirror or, with the scattering probability, in a random direction
//!   (cosine-weighted, like a rough surface);
//! - a ray passing through the listener (a small sphere) adds its energy at
//!   the time it took to get there.
//!
//! Small floating-point differences make GPU and CPU rays diverge after
//! many bounces, so their results agree statistically (arrival times,
//! decay), not sample for sample.

use crate::{context, entry};
use std::sync::Arc;

const SPEED_OF_SOUND: f32 = 343.0;
/// Energy is summed in fixed point (GPU atomics are integers): 2^20 per
/// unit of a ray's starting energy.
const SCALE: f32 = 1048576.0;

/// A room: triangles (counter-clockwise seen from inside) and the
/// absorption of each (0 = reflects everything, 1 = absorbs everything).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Room {
    pub triangles: Vec<[[f32; 3]; 3]>,
    pub absorption: Vec<f32>,
}

impl Room {
    /// A rectangular room from (0, 0, 0) to `size` (metres, z up), all walls
    /// with the same absorption.
    pub fn shoebox(size: [f32; 3], absorption: f32) -> Room {
        let [w, d, h] = size;
        let p = |x: f32, y: f32, z: f32| [x, y, z];
        // Each face as a quad (a, b, c, e), wound to face the inside.
        let quads = [
            [p(0., 0., 0.), p(0., d, 0.), p(w, d, 0.), p(w, 0., 0.)], // floor (normal +z)
            [p(0., 0., h), p(w, 0., h), p(w, d, h), p(0., d, h)],     // ceiling (-z)
            [p(0., 0., 0.), p(w, 0., 0.), p(w, 0., h), p(0., 0., h)], // y = 0 (+y)
            [p(0., d, 0.), p(0., d, h), p(w, d, h), p(w, d, 0.)],     // y = d (-y)
            [p(0., 0., 0.), p(0., 0., h), p(0., d, h), p(0., d, 0.)], // x = 0 (+x)
            [p(w, 0., 0.), p(w, d, 0.), p(w, d, h), p(w, 0., h)],     // x = w (-x)
        ];
        // The quads above are listed clockwise seen from inside: reversed here.
        let triangles: Vec<_> = quads.iter().flat_map(|[a, b, c, e]| [[*a, *c, *b], [*a, *e, *c]]).collect();
        let absorption = vec![absorption; triangles.len()];
        Room { triangles, absorption }
    }

    /// Adds a box (furniture, a pillar) from `min` to `max`, its faces
    /// facing outward, into the room.
    pub fn add_box(&mut self, min: [f32; 3], max: [f32; 3], absorption: f32) {
        let solid = Room::shoebox(sub(max, min), absorption);
        // A shoebox faces inward: turn every triangle around, and move it.
        for [a, b, c] in solid.triangles {
            self.triangles.push([add(a, min), add(c, min), add(b, min)]);
            self.absorption.push(absorption);
        }
    }

    /// Unit normals, facing the side the vertices wind counter-clockwise on.
    fn normals(&self) -> Vec<[f32; 3]> {
        self.triangles.iter().map(|[a, b, c]| normalize(cross(sub(*b, *a), sub(*c, *a)))).collect()
    }
}

/// Where the source and listener are, and how much to trace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trace {
    pub source: [f32; 3],
    pub listener: [f32; 3],
    /// Radius of the listener sphere (metres): larger catches more rays,
    /// with coarser timing.
    pub listener_radius: f32,
    pub rays: u32,
    pub bounces: u32,
    /// Probability that a reflection is diffuse rather than mirror-like.
    pub scattering: f32,
    pub sample_rate: f32,
    /// Length of the response in samples.
    pub length: usize,
}

/// Where the energy response was computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracer {
    /// Ray queries on the GPU (RT cores where the GPU has them).
    Gpu,
    Cpu,
}

/// The energy reaching the listener, per sample: on the GPU with ray
/// queries if it can, else on the CPU. Call off the audio thread.
pub fn energy_response(room: &Room, trace: &Trace) -> (Vec<f32>, Tracer) {
    match energy_response_gpu(room, trace) {
        Ok(energy) => (energy, Tracer::Gpu),
        Err(_) => (energy_response_cpu(room, trace), Tracer::Cpu),
    }
}

/// An impulse response from an energy response: noise shaped by the energy
/// in windows of `window` samples (deterministic for a `seed`).
pub fn impulse_response(energy: &[f32], window: usize, seed: u32) -> Vec<f32> {
    let window = window.max(1);
    let mut rng = seed.wrapping_mul(2654435761).wrapping_add(1);
    let mut out = Vec::with_capacity(energy.len());
    for chunk in energy.chunks(window) {
        let density = chunk.iter().sum::<f32>() / window as f32;
        for _ in chunk {
            // Uniform noise with the same power as a unit Gaussian.
            let u = random(&mut rng) * 2.0 - 1.0;
            out.push(u * 3f32.sqrt() * density.sqrt());
        }
    }
    out
}

// ---------------------------------------------------------------- shared math

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn normalize(a: [f32; 3]) -> [f32; 3] {
    scale(a, 1.0 / dot(a, a).sqrt().max(1e-30))
}

/// PCG hash step, as in the shader.
fn random(state: &mut u32) -> f32 {
    *state = state.wrapping_mul(747796405).wrapping_add(2891336453);
    let word = ((*state >> ((*state >> 28) + 4)) ^ *state).wrapping_mul(277803737);
    (((word >> 22) ^ word) >> 8) as f32 / 16777216.0
}

/// Direction `i` of `n` spread evenly over the sphere.
fn fibonacci(i: u32, n: u32) -> [f32; 3] {
    let z = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
    let r = (1.0 - z * z).max(0.0).sqrt();
    let phi = i as f32 * 2.399_963; // golden angle
    [r * phi.cos(), r * phi.sin(), z]
}

/// A cosine-weighted direction around `n`.
fn diffuse(n: [f32; 3], rng: &mut u32) -> [f32; 3] {
    let (u1, u2) = (random(rng), random(rng));
    let r = u1.sqrt();
    let phi = 6.283_185_5 * u2;
    let helper = if n[0].abs() > 0.5 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
    let t = normalize(cross(helper, n));
    let b = cross(n, t);
    add(add(scale(t, r * phi.cos()), scale(b, r * phi.sin())), scale(n, (1.0 - u1).max(0.0).sqrt()))
}

// ---------------------------------------------------------------- CPU

/// The energy response on the CPU (the same algorithm as the GPU's).
pub fn energy_response_cpu(room: &Room, trace: &Trace) -> Vec<f32> {
    let normals = room.normals();
    let mut histogram = vec![0u32; trace.length];
    for i in 0..trace.rays {
        let mut rng = i.wrapping_mul(9781).wrapping_add(1);
        let mut dir = fibonacci(i, trace.rays);
        let (mut pos, mut energy, mut travelled) = (trace.source, 1.0f32, 0.0f32);
        for _ in 0..trace.bounces {
            // Closest wall (Möller–Trumbore).
            let mut best: Option<(f32, usize)> = None;
            for (k, [a, b, c]) in room.triangles.iter().enumerate() {
                let (e1, e2) = (sub(*b, *a), sub(*c, *a));
                let p = cross(dir, e2);
                let det = dot(e1, p);
                if det.abs() < 1e-12 {
                    continue;
                }
                let s = sub(pos, *a);
                let u = dot(s, p) / det;
                let q = cross(s, e1);
                let v = dot(dir, q) / det;
                let t = dot(e2, q) / det;
                if u >= 0.0 && v >= 0.0 && u + v <= 1.0 && t > 1e-4 && best.is_none_or(|(bt, _)| t < bt) {
                    best = Some((t, k));
                }
            }
            let Some((t, wall)) = best else { break };
            record(&mut histogram, trace, pos, dir, t, travelled, energy);
            travelled += t;
            pos = add(pos, scale(dir, t));
            energy *= 1.0 - room.absorption[wall];
            if energy < 1e-6 {
                break;
            }
            let n = normals[wall];
            dir = if random(&mut rng) < trace.scattering { diffuse(n, &mut rng) } else { sub(dir, scale(n, 2.0 * dot(dir, n))) };
            pos = add(pos, scale(n, 1e-4));
        }
    }
    histogram.into_iter().map(|v| v as f32 / SCALE).collect()
}

/// The ray's energy, if the segment from `pos` along `dir` for `t` passes
/// through the listener.
fn record(histogram: &mut [u32], trace: &Trace, pos: [f32; 3], dir: [f32; 3], t: f32, travelled: f32, energy: f32) {
    let rel = sub(trace.listener, pos);
    let along = dot(rel, dir);
    if along < 0.0 || along > t {
        return;
    }
    if dot(rel, rel) - along * along > trace.listener_radius * trace.listener_radius {
        return;
    }
    let sample = ((travelled + along) / SPEED_OF_SOUND * trace.sample_rate) as usize;
    if let Some(bin) = histogram.get_mut(sample) {
        *bin = bin.wrapping_add((energy * SCALE) as u32);
    }
}

// ---------------------------------------------------------------- GPU

const SHADER: &str = r#"
enable wgpu_ray_query;

struct Params {
    source: vec3<f32>,
    radius: f32,
    listener: vec3<f32>,
    sample_rate: f32,
    rays: u32,
    bounces: u32,
    length: u32,
    scattering: f32,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var room: acceleration_structure;
// Per triangle: normal (xyz) and absorption (w).
@group(0) @binding(2) var<storage, read> walls: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> histogram: array<atomic<u32>>;

fn random(state: ptr<function, u32>) -> f32 {
    *state = *state * 747796405u + 2891336453u;
    let s = *state;
    let word = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return f32(((word >> 22u) ^ word) >> 8u) / 16777216.0;
}

fn fibonacci(i: u32, n: u32) -> vec3<f32> {
    let z = 1.0 - 2.0 * (f32(i) + 0.5) / f32(n);
    let r = sqrt(max(1.0 - z * z, 0.0));
    let phi = f32(i) * 2.399963;
    return vec3<f32>(r * cos(phi), r * sin(phi), z);
}

fn diffuse(n: vec3<f32>, rng: ptr<function, u32>) -> vec3<f32> {
    let u1 = random(rng);
    let u2 = random(rng);
    let r = sqrt(u1);
    let phi = 6.2831855 * u2;
    var helper = vec3<f32>(1.0, 0.0, 0.0);
    if (abs(n.x) > 0.5) { helper = vec3<f32>(0.0, 1.0, 0.0); }
    let t = normalize(cross(helper, n));
    let b = cross(n, t);
    return t * (r * cos(phi)) + b * (r * sin(phi)) + n * sqrt(max(1.0 - u1, 0.0));
}

@compute @workgroup_size(64)
fn trace(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= p.rays) { return; }
    var rng = i * 9781u + 1u;
    var dir = fibonacci(i, p.rays);
    var pos = p.source;
    var energy = 1.0;
    var travelled = 0.0;
    for (var bounce = 0u; bounce < p.bounces; bounce++) {
        var rq: ray_query;
        rayQueryInitialize(&rq, room, RayDesc(RAY_FLAG_FORCE_OPAQUE, 0xffu, 1e-4, 1e5, pos, dir));
        while (rayQueryProceed(&rq)) {}
        let hit = rayQueryGetCommittedIntersection(&rq);
        if (hit.kind != RAY_QUERY_INTERSECTION_TRIANGLE) { break; }
        let t = hit.t;
        // Through the listener on the way?
        let rel = p.listener - pos;
        let along = dot(rel, dir);
        if (along >= 0.0 && along <= t && dot(rel, rel) - along * along <= p.radius * p.radius) {
            let sample = u32((travelled + along) / 343.0 * p.sample_rate);
            if (sample < p.length) {
                atomicAdd(&histogram[sample], u32(energy * 1048576.0));
            }
        }
        travelled += t;
        pos = pos + dir * t;
        let wall = walls[hit.primitive_index];
        energy *= 1.0 - wall.w;
        if (energy < 1e-6) { break; }
        let n = wall.xyz;
        if (random(&rng) < p.scattering) {
            dir = diffuse(n, &rng);
        } else {
            dir = dir - n * (2.0 * dot(dir, n));
        }
        pos = pos + n * 1e-4;
    }
}
"#;

/// The energy response with ray queries on the GPU. Fails if the GPU has
/// none (no RT cores, or no `EXPERIMENTAL_RAY_QUERY` in wgpu).
pub fn energy_response_gpu(room: &Room, trace: &Trace) -> Result<Vec<f32>, String> {
    let ctx: Arc<_> = context()?;
    if !ctx.device.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY) {
        return Err("the GPU has no ray queries".into());
    }
    let device = &ctx.device;
    use wgpu::util::DeviceExt;
    let vertices: Vec<f32> = room.triangles.iter().flatten().flatten().copied().collect();
    let indices: Vec<u32> = (0..room.triangles.len() as u32 * 3).collect();
    let usage = wgpu::BufferUsages::BLAS_INPUT;
    let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("room vertices"), contents: &crate::f32_bytes(&vertices), usage });
    let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("room indices"), contents: &crate::u32_bytes(&indices), usage });
    let size = wgpu::BlasTriangleGeometrySizeDescriptor {
        vertex_format: wgpu::VertexFormat::Float32x3,
        vertex_count: (room.triangles.len() * 3) as u32,
        index_format: Some(wgpu::IndexFormat::Uint32),
        index_count: Some(indices.len() as u32),
        flags: wgpu::AccelerationStructureGeometryFlags::OPAQUE,
    };
    let blas = device.create_blas(
        &wgpu::CreateBlasDescriptor { label: Some("room"), flags: wgpu::AccelerationStructureFlags::PREFER_FAST_TRACE, update_mode: wgpu::AccelerationStructureUpdateMode::Build },
        wgpu::BlasGeometrySizeDescriptors::Triangles { descriptors: vec![size.clone()] },
    );
    let mut tlas = device.create_tlas(&wgpu::CreateTlasDescriptor {
        label: Some("room"),
        max_instances: 1,
        flags: wgpu::AccelerationStructureFlags::PREFER_FAST_TRACE,
        update_mode: wgpu::AccelerationStructureUpdateMode::Build,
    });
    tlas[0] = Some(wgpu::TlasInstance::new(&blas, [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0], 0, 0xff));

    let walls: Vec<f32> = room.normals().iter().zip(&room.absorption).flat_map(|(n, &a)| [n[0], n[1], n[2], a]).collect();
    let walls = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("room walls"), contents: &crate::f32_bytes(&walls), usage: wgpu::BufferUsages::STORAGE });
    let histogram = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("room energy"),
        size: (trace.length.max(1) * 4) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&histogram, 0, &vec![0u8; trace.length.max(1) * 4]);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("room energy staging"),
        size: (trace.length.max(1) * 4) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut params = crate::f32_bytes(&[trace.source[0], trace.source[1], trace.source[2], trace.listener_radius, trace.listener[0], trace.listener[1], trace.listener[2], trace.sample_rate]);
    params.extend(crate::u32_bytes(&[trace.rays, trace.bounces, trace.length as u32]));
    params.extend(crate::f32_bytes(&[trace.scattering]));
    let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("room params"), contents: &params, usage: wgpu::BufferUsages::UNIFORM });

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("ferritron-room"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("ferritron-room"),
        layout: None,
        module: &module,
        entry_point: Some("trace"),
        compilation_options: Default::default(),
        cache: None,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            entry(0, &params),
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::AccelerationStructure(&tlas) },
            entry(2, &walls),
            entry(3, &histogram),
        ],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let geometry = wgpu::BlasTriangleGeometry {
        size: &size,
        vertex_buffer: &vertex_buffer,
        first_vertex: 0,
        vertex_stride: 12,
        index_buffer: Some(&index_buffer),
        first_index: Some(0),
        transform_buffer: None,
        transform_buffer_offset: None,
    };
    let build = wgpu::BlasBuildEntry { blas: &blas, geometry: wgpu::BlasGeometries::TriangleGeometries(vec![geometry]) };
    encoder.build_acceleration_structures(std::iter::once(&build), std::iter::once(&tlas));
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(trace.rays.div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&histogram, 0, &staging, 0, (trace.length.max(1) * 4) as u64);
    ctx.queue.submit([encoder.finish()]);
    staging.map_async(wgpu::MapMode::Read, .., |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
    let view = staging.get_mapped_range(..).map_err(|e| format!("cannot read the result: {e:?}"))?;
    let energy = view.as_chunks::<4>().0.iter().take(trace.length).map(|b| u32::from_le_bytes(*b) as f32 / SCALE).collect();
    drop(view);
    staging.unmap();
    Ok(energy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Room, Trace) {
        let room = Room::shoebox([10.0, 8.0, 4.0], 0.15);
        let trace = Trace {
            source: [3.0, 3.0, 1.5],
            listener: [6.0, 3.0, 1.5],
            listener_radius: 0.35,
            rays: 40_000,
            bounces: 120,
            scattering: 0.3,
            sample_rate: 48_000.0,
            length: 48_000,
        };
        (room, trace)
    }

    /// Reverberation time from the energy decay (Schroeder integration,
    /// -5 to -25 dB, extrapolated to -60 dB).
    fn t60(energy: &[f32], sample_rate: f32) -> f32 {
        let mut tail = vec![0.0f64; energy.len() + 1];
        for i in (0..energy.len()).rev() {
            tail[i] = tail[i + 1] + energy[i] as f64;
        }
        let db = |i: usize| 10.0 * (tail[i] / tail[0]).log10();
        let a = (0..energy.len()).find(|&i| db(i) <= -5.0).unwrap_or(0);
        let b = (0..energy.len()).find(|&i| db(i) <= -25.0).unwrap_or(energy.len() - 1);
        (b - a) as f32 / sample_rate * 3.0
    }

    fn first_arrival(energy: &[f32]) -> usize {
        energy.iter().position(|&e| e > 0.0).unwrap_or(usize::MAX)
    }

    /// A furnished room: hundreds of boxes. The CPU tests every triangle for
    /// every ray; the GPU's acceleration structure (and RT cores) finds the
    /// hit in a few steps.
    #[test]
    fn a_furnished_room_is_traced_faster_with_ray_queries() {
        let _serial = crate::serial();
        let mut room = Room::shoebox([20.0, 15.0, 6.0], 0.1);
        let mut rng = 7u32;
        for _ in 0..100 {
            let x = 1.0 + random(&mut rng) * 17.0;
            let y = 1.0 + random(&mut rng) * 12.0;
            let size = [0.3 + random(&mut rng), 0.3 + random(&mut rng), 0.5 + random(&mut rng) * 1.5];
            if (x - 5.0).abs() < 2.5 && (y - 7.0).abs() < 2.5 {
                continue; // keep the source and the listener in the open
            }
            room.add_box([x, y, 0.0], [x + size[0], y + size[1], size[2]], 0.3);
        }
        let trace = Trace { source: [4.0, 7.0, 1.5], listener: [6.0, 7.0, 1.5], listener_radius: 0.4, rays: 10_000, bounces: 60, scattering: 0.4, sample_rate: 48_000.0, length: 48_000 };
        if energy_response_gpu(&room, &trace).is_err() {
            return eprintln!("skipping: no ray queries");
        }
        let started = std::time::Instant::now();
        let gpu = energy_response_gpu(&room, &trace).unwrap();
        let gpu_time = started.elapsed();
        let started = std::time::Instant::now();
        let cpu = energy_response_cpu(&room, &trace);
        let cpu_time = started.elapsed();
        let (g, c) = (t60(&gpu, trace.sample_rate), t60(&cpu, trace.sample_rate));
        eprintln!("room: furnished, {} triangles, {} rays x {} bounces: GPU (ray queries) {gpu_time:?}, CPU {cpu_time:?}; T60 GPU {g:.2} s, CPU {c:.2} s", room.triangles.len(), trace.rays, trace.bounces);
        assert_eq!(first_arrival(&gpu), first_arrival(&cpu));
        assert!((g / c - 1.0).abs() < 0.15, "T60 GPU {g}, CPU {c}");
        assert!(gpu_time < cpu_time, "ray queries must beat testing every triangle ({gpu_time:?} vs {cpu_time:?})");
    }

    #[test]
    fn a_shoebox_room_sounds_like_its_geometry() {
        let _serial = crate::serial();
        let (room, trace) = setup();
        let started = std::time::Instant::now();
        let cpu = energy_response_cpu(&room, &trace);
        let cpu_time = started.elapsed();
        // Direct sound: 3 m away.
        let direct = (3.0 / SPEED_OF_SOUND * trace.sample_rate) as usize;
        let first = first_arrival(&cpu);
        assert!(first.abs_diff(direct) <= 50, "direct sound at {first}, expected about {direct}");
        // Sabine: T60 = 0.161 V / (S a).
        let sabine = 0.161 * (10.0 * 8.0 * 4.0) / (2.0 * (10.0 * 8.0 + 10.0 * 4.0 + 8.0 * 4.0) * 0.15);
        let measured = t60(&cpu, trace.sample_rate);
        assert!((measured / sabine - 1.0).abs() < 0.35, "CPU T60 {measured} s, Sabine {sabine} s");

        let started = std::time::Instant::now();
        let gpu = match energy_response_gpu(&room, &trace) {
            Ok(e) => e,
            Err(e) => return eprintln!("skipping the GPU: {e}"),
        };
        let gpu_time = started.elapsed();
        eprintln!("room: {} rays x {} bounces: GPU (ray queries) {gpu_time:?}, CPU {cpu_time:?}; T60 GPU {:.2} s, CPU {measured:.2} s, Sabine {sabine:.2} s", trace.rays, trace.bounces, t60(&gpu, trace.sample_rate));
        assert_eq!(first_arrival(&gpu), first, "the same direct sound");
        let (g, c) = (gpu.iter().sum::<f32>(), cpu.iter().sum::<f32>());
        assert!((g / c - 1.0).abs() < 0.1, "total energy GPU {g}, CPU {c}");
        let gpu_t60 = t60(&gpu, trace.sample_rate);
        assert!((gpu_t60 / measured - 1.0).abs() < 0.1, "T60 GPU {gpu_t60}, CPU {measured}");

        let ir = impulse_response(&gpu, 64, 1);
        assert_eq!(ir.len(), trace.length);
        assert!(ir.iter().all(|v| v.is_finite()) && ir.iter().any(|&v| v != 0.0));
    }
}
