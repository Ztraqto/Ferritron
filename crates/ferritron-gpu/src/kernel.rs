//! Slang kernels on the CPU and the GPU (design doc 9.11).
//!
//! A kernel is written once in Slang (`float process(float x, KernelArgs
//! args)`, or with `inout KernelState state` to keep values per channel
//! across samples and blocks) and compiled by `ferritron_slang::compile` in
//! the build script to WGSL and to C++. [`slang_kernel!`](crate::slang_kernel)
//! brings both into the plugin as a [`Kernel`], and [`KernelNode`] runs it
//! where it is faster.

use crate::{Context, context, entry, f32_bytes, u32_bytes};
use ferritron_graph::{Device, Measured, Node, NodeSetup, Policy, choose};
use std::sync::Arc;

/// What a kernel sees for each sample (`KernelArgs` in Slang).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KernelArgs {
    /// The kernel's parameters.
    pub values: [f32; 8],
    pub channel: u32,
    /// Frame within the block.
    pub frame: u32,
}

/// What a stateful kernel keeps per channel (`KernelState` in Slang): all
/// zeros at the start and after a reset.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KernelState {
    pub v: [f32; 16],
}

/// A kernel's CPU function.
#[derive(Clone, Copy)]
pub enum KernelFn {
    Stateless(unsafe extern "C" fn(f32, *const KernelArgs) -> f32),
    Stateful(unsafe extern "C" fn(f32, *const KernelArgs, *mut KernelState) -> f32),
}

/// A kernel compiled from Slang: WGSL for the GPU, a function for the CPU.
#[derive(Clone, Copy)]
pub struct Kernel {
    pub name: &'static str,
    pub wgsl: &'static str,
    pub cpu: KernelFn,
}

impl Kernel {
    pub fn is_stateful(&self) -> bool {
        matches!(self.cpu, KernelFn::Stateful(_))
    }

    /// Runs the CPU function on one sample.
    pub fn cpu(&self, x: f32, args: &KernelArgs, state: &mut KernelState) -> f32 {
        // SAFETY: the function reads `args` and reads and writes `state` only.
        unsafe {
            match self.cpu {
                KernelFn::Stateless(f) => f(x, args),
                KernelFn::Stateful(f) => f(x, args, state),
            }
        }
    }
}

/// The kernel `name` compiled by `ferritron_slang::compile(name, ...)` in
/// this crate's build script; `slang_kernel!(name, stateful)` for a kernel
/// with `inout KernelState` (the wrong kind fails to link).
#[macro_export]
macro_rules! slang_kernel {
    ($name:ident) => {{
        unsafe extern "C" {
            #[link_name = concat!("ferritron_cpu_", stringify!($name))]
            fn cpu(x: f32, args: *const $crate::KernelArgs) -> f32;
        }
        $crate::Kernel { name: stringify!($name), wgsl: include_str!(concat!(env!("OUT_DIR"), "/", stringify!($name), ".wgsl")), cpu: $crate::KernelFn::Stateless(cpu) }
    }};
    ($name:ident, stateful) => {{
        unsafe extern "C" {
            #[link_name = concat!("ferritron_cpu_stateful_", stringify!($name))]
            fn cpu(x: f32, args: *const $crate::KernelArgs, state: *mut $crate::KernelState) -> f32;
        }
        $crate::Kernel { name: stringify!($name), wgsl: include_str!(concat!(env!("OUT_DIR"), "/", stringify!($name), ".wgsl")), cpu: $crate::KernelFn::Stateful(cpu) }
    }};
}

/// Kernels on the GPU, for blocks of up to `max_block` frames: one, or a
/// chain of them run in series on the same data (fused, design doc 9.7):
/// one upload, one dispatch per kernel, one readback. Waits for the GPU in
/// [`process`](Self::process).
pub struct GpuKernel {
    ctx: Arc<Context>,
    stages: Vec<Stage>,
    data: wgpu::Buffer,
    staging: wgpu::Buffer,
    channels: usize,
    max_block: usize,
}

struct Stage {
    pipeline: wgpu::ComputePipeline,
    group: wgpu::BindGroup,
    params: wgpu::Buffer,
    /// Per-channel state of a stateful kernel.
    state: Option<wgpu::Buffer>,
}

impl GpuKernel {
    pub fn new(kernel: &Kernel, channels: usize, max_block: usize) -> Result<Self, String> {
        Self::chain(&[*kernel], channels, max_block)
    }

    /// Kernels in series, fused into one GPU round trip per block.
    pub fn chain(kernels: &[Kernel], channels: usize, max_block: usize) -> Result<Self, String> {
        if kernels.is_empty() {
            return Err("a kernel chain needs at least one kernel".into());
        }
        let ctx = context()?;
        let device = &ctx.device;
        let max_block = max_block.max(1);
        let buffer = |label: &'static str, size: usize, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(16) as u64, usage, mapped_at_creation: false })
        };
        use wgpu::BufferUsages as U;
        let data = buffer("kernel data", channels * max_block * 4, U::STORAGE | U::COPY_DST | U::COPY_SRC);
        let staging = buffer("kernel staging", channels * max_block * 4, U::MAP_READ | U::COPY_DST);
        let stages = kernels
            .iter()
            .map(|kernel| {
                let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(kernel.name), source: wgpu::ShaderSource::Wgsl(kernel.wgsl.into()) });
                let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(kernel.name),
                    layout: None,
                    module: &module,
                    entry_point: Some("ferritron_main"),
                    compilation_options: Default::default(),
                    cache: None,
                });
                let params = buffer("kernel params", 48, U::UNIFORM | U::COPY_DST);
                let state = kernel.is_stateful().then(|| buffer("kernel state", channels * std::mem::size_of::<KernelState>(), U::STORAGE | U::COPY_DST));
                let mut entries = vec![entry(0, &params), entry(1, &data)];
                if let Some(state) = &state {
                    entries.push(entry(2, state));
                }
                let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries: &entries });
                Stage { pipeline, group, params, state }
            })
            .collect();
        let mut gpu = Self { ctx: ctx.clone(), stages, data, staging, channels, max_block };
        // Pay pipeline setup now, not on the first block.
        let mut sink = vec![vec![0.0f32; 1]; channels];
        let values = vec![[0.0f32; 8]; kernels.len()];
        gpu.process_chain(&vec![&[0.0f32][..]; channels], &mut sink.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>(), &values);
        gpu.reset();
        Ok(gpu)
    }

    /// Clears the state of stateful kernels.
    pub fn reset(&mut self) {
        for state in self.stages.iter().filter_map(|s| s.state.as_ref()) {
            self.ctx.queue.write_buffer(state, 0, &vec![0u8; self.channels * std::mem::size_of::<KernelState>()]);
        }
    }

    /// Runs the kernel over a block (channels in, channels out).
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], values: &[f32; 8]) {
        self.process_chain(inputs, outputs, std::slice::from_ref(values));
    }

    /// Runs the chain over a block; `values[i]` are kernel `i`'s parameters.
    pub fn process_chain(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], values: &[[f32; 8]]) {
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0).min(self.max_block);
        if n == 0 || inputs.len() < self.channels || outputs.len() < self.channels {
            return;
        }
        let queue = &self.ctx.queue;
        for (ch, input) in inputs.iter().take(self.channels).enumerate() {
            queue.write_buffer(&self.data, (ch * self.max_block * 4) as u64, &f32_bytes(&input[..n]));
        }
        for (stage, values) in self.stages.iter().zip(values.iter().chain(std::iter::repeat(&[0.0; 8]))) {
            let mut params = u32_bytes(&[n as u32, self.channels as u32, self.max_block as u32, 0]);
            params.extend(f32_bytes(values));
            queue.write_buffer(&stage.params, 0, &params);
        }
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            // Each dispatch sees the previous one's writes (WebGPU orders them).
            for stage in &self.stages {
                pass.set_pipeline(&stage.pipeline);
                pass.set_bind_group(0, &stage.group, &[]);
                if stage.state.is_some() {
                    pass.dispatch_workgroups((self.channels as u32).div_ceil(64), 1, 1);
                } else {
                    pass.dispatch_workgroups((n as u32).div_ceil(256), self.channels as u32, 1);
                }
            }
        }
        encoder.copy_buffer_to_buffer(&self.data, 0, &self.staging, 0, (self.channels * self.max_block * 4) as u64);
        queue.submit([encoder.finish()]);
        self.staging.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.ctx.device.poll(wgpu::PollType::wait_indefinitely());
        if let Ok(view) = self.staging.get_mapped_range(..) {
            for (ch, out) in outputs.iter_mut().take(self.channels).enumerate() {
                let bytes = &view[ch * self.max_block * 4..(ch * self.max_block + n) * 4];
                for (s, b) in out[..n].iter_mut().zip(bytes.as_chunks::<4>().0) {
                    *s = f32::from_le_bytes(*b);
                }
            }
        }
        self.staging.unmap();
    }
}

/// Slang kernels as a graph node, on the CPU or the GPU by measurement.
///
/// The GPU is offered only offline: a kernel touches each sample once, so
/// in real time moving the block to the GPU and back costs more than the
/// work itself, and would add latency.
///
/// Kernel nodes in series fuse in a graph (design doc 9.7): one node runs
/// the whole chain, with one GPU round trip per block. The ids of the
/// original nodes keep working: `graph.node_mut::<KernelNode>(id)` selects
/// that node's kernel, so `set` reaches its parameters.
pub struct KernelNode {
    kernels: Vec<Kernel>,
    channels: usize,
    /// Parameters of each kernel, and the one `set` changes.
    args: Vec<KernelArgs>,
    active: usize,
    gpu: Option<GpuKernel>,
    /// Per-kernel, per-channel state on the CPU (stateful kernels).
    states: Vec<Vec<KernelState>>,
    setup: NodeSetup,
    measured: Vec<Measured>,
    values: Vec<[f32; 8]>,
}

impl KernelNode {
    pub fn new(kernel: Kernel, channels: usize, setup: &NodeSetup) -> Self {
        Self::chain(&[kernel], channels, setup)
    }

    /// Kernels in series as one node (the result of fusing kernel nodes).
    pub fn chain(kernels: &[Kernel], channels: usize, setup: &NodeSetup) -> Self {
        let node = |gpu| Self {
            kernels: kernels.to_vec(),
            channels,
            args: vec![KernelArgs::default(); kernels.len()],
            active: 0,
            gpu,
            states: vec![vec![KernelState::default(); channels]; kernels.len()],
            setup: *setup,
            measured: Vec::new(),
            values: vec![[0.0; 8]; kernels.len()],
        };
        let mut candidates = vec![(Device::Cpu, node(None))];
        if setup.offline
            && setup.policy != Policy::Off
            && let Ok(gpu) = GpuKernel::chain(kernels, channels, setup.max_block)
        {
            candidates.push((Device::Gpu, node(Some(gpu))));
        }
        let (mut chosen, measured) = choose(candidates, setup).expect("the CPU candidate is always there");
        chosen.measured = measured;
        chosen
    }

    /// Sets parameter `index` (`args.values[index]` in the kernel) of the
    /// selected kernel.
    pub fn set(&mut self, index: usize, value: f32) {
        if let Some(v) = self.args[self.active].values.get_mut(index) {
            *v = value;
        }
    }

    pub fn device(&self) -> Device {
        if self.gpu.is_some() { Device::Gpu } else { Device::Cpu }
    }

    pub fn measurements(&self) -> &[Measured] {
        &self.measured
    }
}

impl Node for KernelNode {
    fn inputs(&self) -> usize {
        self.channels
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        self.states.iter_mut().flatten().for_each(|s| *s = KernelState::default());
        if let Some(gpu) = &mut self.gpu {
            gpu.reset();
        }
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if let Some(gpu) = &mut self.gpu {
            for (v, a) in self.values.iter_mut().zip(&self.args) {
                *v = a.values;
            }
            gpu.process_chain(inputs, outputs, &self.values);
            return;
        }
        for (stage, (kernel, (args, states))) in self.kernels.iter().zip(self.args.iter().zip(&mut self.states)).enumerate() {
            let mut args = *args;
            for (ch, (out, state)) in outputs.iter_mut().zip(states.iter_mut()).enumerate() {
                args.channel = ch as u32;
                for i in 0..out.len() {
                    args.frame = i as u32;
                    // The first kernel reads the input, the next ones the previous output.
                    let x = if stage == 0 { inputs[ch][i] } else { out[i] };
                    out[i] = kernel.cpu(x, &args, state);
                }
            }
        }
    }

    /// Two kernel nodes in series become one chain, placed anew as a whole.
    fn fuse(&self, next: &dyn Node) -> Option<Box<dyn Node>> {
        let next = next.as_any().downcast_ref::<KernelNode>()?;
        if next.channels != self.channels || next.setup != self.setup {
            return None;
        }
        let kernels: Vec<Kernel> = self.kernels.iter().chain(&next.kernels).copied().collect();
        let mut fused = KernelNode::chain(&kernels, self.channels, &self.setup);
        fused.args = self.args.iter().chain(&next.args).copied().collect();
        Some(Box::new(fused))
    }

    fn members(&self) -> usize {
        self.kernels.len()
    }

    fn select_member(&mut self, member: usize) {
        self.active = member.min(self.kernels.len() - 1);
    }
}
