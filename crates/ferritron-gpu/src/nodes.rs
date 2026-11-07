//! GPU components as nodes of a `ferritron_graph::Graph` (design doc 9.2).
//!
//! Each node picks where it runs when it is created: it builds its CPU and
//! GPU candidates and lets [`ferritron_graph::choose`] measure them under
//! the GPU policy. Offline, GPU candidates wait for the GPU (no latency).
//!
//! IIR nodes in series fuse in the graph (design doc 9.7): `Graph::prepare`
//! merges them into one chain, so the run costs one GPU round trip per block
//! and the latency of one bank.

use crate::{Biquad, Convolver, CpuIirBank, IirBank, Lane, Mode, RealtimeIirBank};
use ferritron_vulkan::{ModalLane, PersistentModalBank, TensorMixer};
use ferritron_graph::{Device, Measured, Node, NodeSetup, Policy, choose};

enum Bank {
    Cpu(CpuIirBank),
    /// Real time: on a GPU thread, delayed by its latency.
    Gpu(RealtimeIirBank),
    /// Offline: waits for the GPU every block.
    GpuOffline(IirBank),
    /// Real time without latency: a persistent kernel (native Vulkan) that
    /// stays on the GPU between blocks. One biquad per lane, one stage.
    Persistent(Box<PersistentModalBank>),
}

/// An IIR filter bank ([`Lane`]s of biquads) that runs on the CPU or the
/// GPU, whichever is faster under the policy. Small banks usually stay on
/// the CPU; thousands of filters go to the GPU.
///
/// IIR nodes in series fuse into one chain in a graph. The ids of the
/// original nodes keep working: `graph.node_mut::<IirNode>(id)` selects
/// that node's part, so `set_lane` numbers lanes as that node did.
pub struct IirNode {
    bank: Bank,
    inputs: usize,
    outputs: usize,
    measured: Vec<Measured>,
    /// What the bank is made of (kept current by `set_lane`), to rebuild it
    /// when fusing.
    stages: Vec<Vec<Lane>>,
    setup: NodeSetup,
    /// First lane of each member (original node), and the selected member.
    members: Vec<usize>,
    active: usize,
}

impl IirNode {
    /// Builds the candidates and keeps the fastest allowed one. Never fails:
    /// the CPU candidate is always there. Call off the audio thread.
    pub fn new(lanes: &[Lane], setup: &NodeSetup) -> Self {
        Self::chain(&[lanes], setup).expect("a single stage is always a valid chain")
    }

    /// Like [`new`](Self::new), and in real time under `Policy::Max` also
    /// considers a persistent kernel (native Vulkan, design doc 9.3): the
    /// GPU without latency, even at 64-sample blocks. Opt-in, because the
    /// kernel keeps a part of the GPU busy waiting for blocks while audio
    /// runs, which slows down other GPU work on the computer (other
    /// processes, the DAW's own drawing). One per computer; others get the
    /// usual candidates. Needs one stage of one-biquad lanes and at most 8
    /// outputs.
    pub fn new_low_latency(lanes: &[Lane], setup: &NodeSetup) -> Self {
        Self::build(&[lanes], setup, true).expect("a single stage is always a valid chain")
    }

    /// Banks in series as one node ([`IirBank::chain`]). On the GPU the
    /// whole chain is fused: one round trip per block instead of one per
    /// bank. Fails only if a stage reads a channel the previous one lacks.
    pub fn chain(stages: &[&[Lane]], setup: &NodeSetup) -> Result<Self, String> {
        Self::build(stages, setup, false)
    }

    fn build(stages: &[&[Lane]], setup: &NodeSetup, low_latency: bool) -> Result<Self, String> {
        let cpu = CpuIirBank::chain(stages, setup.max_block)?;
        let (inputs, outputs) = (cpu.inputs(), cpu.outputs());
        let node = |bank| Self {
            bank,
            inputs,
            outputs,
            measured: Vec::new(),
            stages: stages.iter().map(|s| s.to_vec()).collect(),
            setup: *setup,
            members: vec![0],
            active: 0,
        };
        let mut candidates = vec![(Device::Cpu, node(Bank::Cpu(cpu)))];
        if setup.policy != Policy::Off {
            let gpu = if setup.offline {
                IirBank::chain(stages, setup.max_block).map(Bank::GpuOffline)
            } else {
                RealtimeIirBank::chain(stages, setup.max_block).map(Bank::Gpu)
            };
            if let Ok(bank) = gpu {
                candidates.push((Device::Gpu, node(bank)));
            }
            // Max: the GPU may be kept busy, waiting for blocks.
            if low_latency
                && !setup.offline
                && setup.policy == Policy::Max
                && let Some(bank) = persistent(stages, inputs, outputs, setup)
            {
                candidates.push((Device::Gpu, node(Bank::Persistent(Box::new(bank)))));
            }
        }
        let (mut chosen, measured) = choose(candidates, setup).expect("the CPU candidate is always there");
        // The CPU covers late GPU blocks only if it can afford a block: at
        // most half the real-time budget, or the fallback itself would glitch.
        let budget = setup.max_block as f64 / setup.sample_rate * 1e6;
        let cpu = measured.iter().find(|m| m.device == Device::Cpu).map_or(f64::INFINITY, |m| m.micros);
        if let Bank::Gpu(bank) = &mut chosen.bank {
            bank.set_fallback(cpu <= budget / 2.0);
            // Measuring queued blocks faster than real time: let the GPU
            // catch up, or the first real blocks come late.
            bank.settle(std::time::Duration::from_secs(1));
        }
        if let Bank::Persistent(bank) = &mut chosen.bank {
            // Running and answering before the first audio block; clean state.
            bank.settle();
            bank.reset();
        }
        chosen.measured = measured;
        Ok(chosen)
    }

    /// Where the bank runs.
    pub fn device(&self) -> Device {
        match self.bank {
            Bank::Cpu(_) => Device::Cpu,
            Bank::Gpu(_) | Bank::GpuOffline(_) | Bank::Persistent(_) => Device::Gpu,
        }
    }

    /// True when a persistent kernel runs the bank (no latency on the GPU).
    pub fn is_persistent(&self) -> bool {
        matches!(self.bank, Bank::Persistent(_))
    }

    /// Real-time GPU blocks that the CPU fallback computed instead.
    pub fn fallback_blocks(&self) -> u64 {
        match &self.bank {
            Bank::Gpu(b) => b.fallback_blocks(),
            Bank::Cpu(_) | Bank::GpuOffline(_) | Bank::Persistent(_) => 0,
        }
    }

    /// What placement measured for each candidate.
    pub fn measurements(&self) -> &[Measured] {
        &self.measured
    }

    /// Replaces a lane's coefficients; see [`IirBank::set_lane`]. Real-time
    /// safe. In a fused node, `lane` counts within the selected member.
    pub fn set_lane(&mut self, lane: usize, sections: &[Biquad]) {
        let lane = self.members[self.active] + lane;
        match &mut self.bank {
            Bank::Cpu(b) => b.set_lane(lane, sections),
            Bank::Gpu(b) => b.set_lane(lane, sections),
            Bank::GpuOffline(b) => b.set_lane(lane, sections),
            Bank::Persistent(b) => {
                if let Some(s) = sections.first() {
                    b.set_lane(lane, [s.b0, s.b1, s.b2, s.a1, s.a2]);
                }
            }
        }
        // Keep the description current, for fusion (no allocation).
        let mut index = lane;
        for stage in &mut self.stages {
            if index < stage.len() {
                let kept = &mut stage[index].sections;
                let n = kept.len().min(sections.len());
                kept[..n].copy_from_slice(&sections[..n]);
                return;
            }
            index -= stage.len();
        }
    }
}

impl Node for IirNode {
    fn inputs(&self) -> usize {
        self.inputs
    }

    fn outputs(&self) -> usize {
        self.outputs
    }

    fn latency(&self) -> u32 {
        match &self.bank {
            Bank::Gpu(b) => b.latency() as u32,
            Bank::Cpu(_) | Bank::GpuOffline(_) | Bank::Persistent(_) => 0,
        }
    }

    fn missed_blocks(&self) -> u64 {
        match &self.bank {
            Bank::Gpu(b) => b.missed_blocks(),
            Bank::Persistent(b) => b.missed_blocks(),
            Bank::Cpu(_) | Bank::GpuOffline(_) => 0,
        }
    }

    fn reset(&mut self) {
        match &mut self.bank {
            Bank::Cpu(b) => b.reset(),
            Bank::GpuOffline(b) => b.reset(),
            Bank::Persistent(b) => b.reset(),
            // Its state lives on the GPU thread; placement only feeds it silence.
            Bank::Gpu(_) => {}
        }
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        match &mut self.bank {
            Bank::Cpu(b) => b.process(inputs, outputs),
            Bank::Gpu(b) => b.process(inputs, outputs),
            Bank::GpuOffline(b) => b.process(inputs, outputs),
            Bank::Persistent(b) => {
                b.process(inputs, outputs);
            }
        }
    }

    /// Two IIR nodes in series become one chain, placed anew as a whole.
    fn fuse(&self, next: &dyn Node) -> Option<Box<dyn Node>> {
        let next = next.as_any().downcast_ref::<IirNode>()?;
        if next.setup != self.setup {
            return None;
        }
        let stages: Vec<&[Lane]> = self.stages.iter().chain(&next.stages).map(|s| &s[..]).collect();
        let mut fused = IirNode::chain(&stages, &self.setup).ok()?;
        let lanes: usize = self.stages.iter().map(Vec::len).sum();
        fused.members = self.members.iter().copied().chain(next.members.iter().map(|m| m + lanes)).collect();
        Some(Box::new(fused))
    }

    fn members(&self) -> usize {
        self.members.len()
    }

    fn select_member(&mut self, member: usize) {
        self.active = member.min(self.members.len() - 1);
    }
}

/// The persistent-kernel bank for `stages`, if they fit it: one stage of
/// one-biquad lanes, at most 8 outputs, and a GPU with native Vulkan.
fn persistent(stages: &[&[Lane]], inputs: usize, outputs: usize, setup: &NodeSetup) -> Option<PersistentModalBank> {
    let [lanes] = stages else { return None };
    // Only on wgpu's own device: on a device of its own, a persistent
    // kernel would make wgpu's work wait for it.
    crate::context().ok()?;
    if !ferritron_vulkan::Gpu::get().is_ok_and(|g| g.is_shared()) {
        return None;
    }
    if lanes.iter().any(|l| l.sections.len() != 1) || outputs > 8 {
        return None;
    }
    let modal: Vec<ModalLane> = lanes
        .iter()
        .map(|l| {
            let s = l.sections[0];
            ModalLane { b0: s.b0, b1: s.b1, b2: s.b2, a1: s.a1, a2: s.a2, input: l.input as u32, output: l.output as u32, pad: 0 }
        })
        .collect();
    // Half the block's real-time budget: the rest is the plugin's.
    let budget = std::time::Duration::from_secs_f64(setup.max_block as f64 / setup.sample_rate / 2.0);
    PersistentModalBank::new(&modal, inputs.max(1), outputs, setup.max_block, budget).ok()
}

/// A long convolution of one channel ([`Convolver`]): the head on the CPU,
/// the tail on the GPU, no latency. There is no CPU candidate (a long
/// convolution is far too slow there), so it needs a GPU and a policy other
/// than `Off`.
pub struct ConvolverNode {
    convolver: Convolver,
}

impl ConvolverNode {
    pub fn new(ir: &[f32], setup: &NodeSetup) -> Result<Self, String> {
        if setup.policy == Policy::Off {
            return Err("the GPU policy is off".into());
        }
        let mode = Mode::from_offline(setup.offline);
        Ok(Self { convolver: Convolver::with_mode(ir, setup.max_block, mode)? })
    }

    pub fn convolver(&self) -> &Convolver {
        &self.convolver
    }
}

impl Node for ConvolverNode {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn missed_blocks(&self) -> u64 {
        self.convolver.missed_blocks()
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let out = &mut *outputs[0];
        out.copy_from_slice(&inputs[0][..out.len()]);
        self.convolver.process(out);
    }
}

/// A matrix mixer: every output a weighted sum of every input
/// (`set_gain`), for many channels at once (spatial audio, large routing
/// matrices). On the CPU, or on Tensor cores (f16 products, f32 sums:
/// about three decimal digits) where they are faster, by measurement.
pub struct MatrixMixerNode {
    inputs: usize,
    outputs: usize,
    weights: Vec<f32>,
    tensor: Option<TensorMixer>,
    measured: Vec<Measured>,
}

impl MatrixMixerNode {
    /// All weights start at zero. Call off the audio thread.
    pub fn new(inputs: usize, outputs: usize, setup: &NodeSetup) -> Self {
        let node = |tensor| Self { inputs, outputs, weights: vec![0.0; inputs * outputs], tensor, measured: Vec::new() };
        let mut candidates = vec![(Device::Cpu, node(None))];
        // Tensor cores wait for the GPU (tens of microseconds): no latency.
        if setup.policy != Policy::Off
            && crate::context().is_ok()
            && let Ok(tensor) = TensorMixer::new(inputs, outputs, setup.max_block)
        {
            candidates.push((Device::Gpu, node(Some(tensor))));
        }
        let (mut chosen, measured) = choose(candidates, setup).expect("the CPU candidate is always there");
        chosen.measured = measured;
        chosen
    }

    /// Sets the weight of input `input` in output `output`. Real-time safe.
    pub fn set_gain(&mut self, output: usize, input: usize, gain: f32) {
        if output < self.outputs && input < self.inputs {
            self.weights[output * self.inputs + input] = gain;
            if let Some(tensor) = &mut self.tensor {
                tensor.set_gain(output, input, gain);
            }
        }
    }

    pub fn device(&self) -> Device {
        if self.tensor.is_some() { Device::Gpu } else { Device::Cpu }
    }

    pub fn measurements(&self) -> &[Measured] {
        &self.measured
    }
}

impl Node for MatrixMixerNode {
    fn inputs(&self) -> usize {
        self.inputs
    }

    fn outputs(&self) -> usize {
        self.outputs
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if let Some(tensor) = &mut self.tensor {
            tensor.process(inputs, outputs);
            return;
        }
        for (o, out) in outputs.iter_mut().enumerate() {
            out.fill(0.0);
            // Every weight, zero or not: the cost placement measures (with
            // all weights zero) is the cost of the mixer in use.
            for (input, &w) in inputs.iter().zip(&self.weights[o * self.inputs..(o + 1) * self.inputs]) {
                for (y, &x) in out.iter_mut().zip(input.iter()) {
                    *y += w * x;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(policy: Policy, offline: bool, max_block: usize) -> NodeSetup {
        NodeSetup { sample_rate: 48_000.0, max_block, offline, policy }
    }

    fn resonators(count: usize) -> Vec<Lane> {
        (0..count)
            .map(|i| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(100.0 + 3.0 * i as f64, 50.0, 48_000.0)] })
            .collect()
    }

    #[test]
    fn small_banks_stay_on_the_cpu_and_large_ones_go_to_the_gpu() {
        let _serial = crate::serial();
        let small = IirNode::new(&resonators(4), &setup(Policy::Max, true, 256));
        assert_eq!(small.device(), Device::Cpu, "4 filters: {:?}", small.measurements());
        let large = IirNode::new(&resonators(4000), &setup(Policy::Max, true, 4096));
        if large.measurements().len() < 2 {
            return eprintln!("skipping: no GPU");
        }
        assert_eq!(large.device(), Device::Gpu, "4000 filters: {:?}", large.measurements());
        let off = IirNode::new(&resonators(4000), &setup(Policy::Off, true, 4096));
        assert_eq!(off.device(), Device::Cpu);
        assert_eq!(off.measurements().len(), 1, "no GPU candidate is even built");
    }

    #[test]
    fn balanced_never_adds_latency() {
        let _serial = crate::serial();
        // In real time the GPU bank has latency, so balanced keeps the CPU.
        let node = IirNode::new(&resonators(4000), &setup(Policy::Balanced, false, 256));
        assert_eq!(node.device(), Device::Cpu);
        assert_eq!(node.latency(), 0);
    }

    /// Resonators (16 channels), then filters per channel, then a stereo
    /// mixdown, as three nodes; `size` lanes in each.
    fn three_nodes_sized(setup: &NodeSetup, fusion: bool, size: usize) -> (ferritron_graph::Graph, [ferritron_graph::NodeId; 3]) {
        use ferritron_graph::Graph;
        let rate = 48_000.0;
        let modes: Vec<Lane> = (0..size)
            .map(|i| Lane { input: 0, output: i % 16, sections: vec![Biquad::bandpass(80.0 + 4.0 * (i % 2000) as f64, 150.0, rate)] })
            .collect();
        let eq: Vec<Lane> = (0..size.max(16))
            .map(|i| Lane { input: i % 16, output: i % 16, sections: vec![Biquad::peaking(300.0 + i as f64, 1.0, 1.0, rate)] })
            .collect();
        let mix: Vec<Lane> = (0..size.max(16))
            .map(|i| Lane { input: i % 16, output: i % 2, sections: vec![Biquad::lowpass(8000.0 - i as f64, 0.7, rate)] })
            .collect();
        let mut g = Graph::new(1, 2);
        let ids = [g.add(IirNode::new(&modes, setup)), g.add(IirNode::new(&eq, setup)), g.add(IirNode::new(&mix, setup))];
        g.connect(Graph::INPUT, 0, ids[0], 0).unwrap();
        g.connect_all(ids[0], ids[1]).unwrap();
        g.connect_all(ids[1], ids[2]).unwrap();
        g.connect_all(ids[2], Graph::OUTPUT).unwrap();
        g.set_fusion(fusion);
        g.prepare(setup.max_block).unwrap();
        (g, ids)
    }

    fn render(g: &mut ferritron_graph::Graph, ids: [ferritron_graph::NodeId; 3], blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for block in 0..blocks {
            if block == 3 {
                // Through the original ids: lane 2 of the EQ, lane 1 of the mixdown.
                g.node_mut::<IirNode>(ids[1]).unwrap().set_lane(2, &[Biquad::peaking(3000.0, 2.0, -8.0, 48_000.0)]);
                g.node_mut::<IirNode>(ids[2]).unwrap().set_lane(1, &[Biquad::lowpass(2000.0, 0.7, 48_000.0)]);
            }
            let mut input = vec![0.0f32; 256];
            if block % 4 == 0 {
                input[block % 200] = 1.0;
            }
            let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
            g.process(&[&input], &mut [&mut l, &mut r]);
            out.extend(l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]));
        }
        out
    }

    #[test]
    fn iir_nodes_in_series_fuse_into_one_chain() {
        let _serial = crate::serial();
        // Large enough for the GPU: compares the fused GPU chain with the
        // banks run one by one.
        let setup = setup(Policy::Max, true, 256);
        let (mut fused, ids) = three_nodes_sized(&setup, true, 2048);
        let (mut separate, separate_ids) = three_nodes_sized(&setup, false, 2048);
        assert_eq!(fused.fused_nodes(), 2);
        assert_eq!(separate.fused_nodes(), 0);
        let a = render(&mut fused, ids, 12);
        let b = render(&mut separate, separate_ids, 12);
        let peak = b.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(peak > 1e-3 && worst < 1e-4 * peak.max(1.0), "fused differs by {worst} (peak {peak})");
    }

    #[test]
    fn a_fused_real_time_chain_has_the_latency_of_one_bank() {
        let _serial = crate::serial();
        // Every stage is heavy enough for the GPU on its own.
        let setup = setup(Policy::Max, false, 256);
        let (mut separate, ids) = three_nodes_sized(&setup, false, 2048);
        if ids.iter().any(|&id| separate.node_mut::<IirNode>(id).unwrap().device() != Device::Gpu) {
            return eprintln!("skipping: not every bank runs on the GPU here");
        }
        // Separately, the banks' latencies add up (a stage that runs on a
        // persistent kernel adds none).
        let each: u32 = ids.iter().map(|&id| separate.node_mut::<IirNode>(id).unwrap().latency()).sum();
        assert!(each >= 512, "at least one bank has GPU latency");
        assert_eq!(separate.latency(), each, "separately, each GPU bank adds its latency");
        let (fused, _) = three_nodes_sized(&setup, true, 2048);
        assert_eq!(fused.fused_nodes(), 2);
        assert_eq!(fused.latency(), 512, "fused, the chain has the latency of one bank");
    }

    #[test]
    fn offline_batches_beyond_the_host_block_give_the_same_sound_faster() {
        let _serial = crate::serial();
        use ferritron_graph::Batched;
        let lanes = resonators(4000);
        let host_block = 256;
        let batch = 8192;
        let small = IirNode::new(&lanes, &setup(Policy::Max, true, host_block));
        if small.device() != Device::Gpu {
            return eprintln!("skipping: the bank does not run on the GPU here");
        }
        let mut small = small;
        let mut batched = Batched::new(IirNode::new(&lanes, &setup(Policy::Max, true, batch)), batch);
        assert_eq!(batched.inner().device(), Device::Gpu);
        let frames = 4 * batch;
        let mut input = vec![0.0f32; frames];
        for i in (0..frames).step_by(3000) {
            input[i] = 1.0;
        }
        let run = |node: &mut dyn Node| {
            let start = std::time::Instant::now();
            let mut out = Vec::with_capacity(frames);
            for chunk in input.chunks(host_block) {
                let mut block = vec![0.0f32; chunk.len()];
                node.process(&[chunk], &mut [&mut block]);
                out.extend(block);
            }
            (out, start.elapsed())
        };
        let (want, slow) = run(&mut small);
        let (got, fast) = run(&mut batched);
        eprintln!("offline, {} frames: blocks of {host_block} {slow:?}, batches of {batch} {fast:?}", frames);
        let peak = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = got[batch..].iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(peak > 1e-3 && worst < 1e-4 * peak, "batched differs by {worst} (peak {peak})");
        assert!(fast < slow, "one round trip per batch must beat one per block ({fast:?} vs {slow:?})");
    }

    #[test]
    fn a_second_instance_falls_back_while_one_holds_the_persistent_kernel() {
        let _serial = crate::serial();
        let lanes = resonators(2048);
        let setup = setup(Policy::Max, false, 64);
        let first = IirNode::new_low_latency(&lanes, &setup);
        if !first.is_persistent() {
            return eprintln!("skipping: no persistent kernel here");
        }
        let second = IirNode::new_low_latency(&lanes, &setup);
        assert!(!second.is_persistent(), "one persistent kernel per computer");
        let plain = IirNode::new(&lanes, &setup);
        assert!(!plain.is_persistent(), "only new_low_latency considers it");
        drop(plain);
        drop(first);
        let third = IirNode::new_low_latency(&lanes, &setup);
        assert!(third.is_persistent(), "free again once the first instance is gone");
    }

    #[test]
    fn small_real_time_blocks_run_on_a_persistent_kernel_without_latency() {
        let _serial = crate::serial();
        // 64-sample blocks: too small for a GPU round trip per block, fine
        // for a persistent kernel.
        let lanes = resonators(3000);
        let node = IirNode::new_low_latency(&lanes, &setup(Policy::Max, false, 64));
        eprintln!("{:?}", node.measurements());
        if !node.is_persistent() {
            return eprintln!("skipping: no persistent kernel here ({:?})", node.device());
        }
        assert_eq!(node.latency(), 0);
        let mut node = node;
        let mut cpu = CpuIirBank::new(&lanes, 64);
        for block in 0..50 {
            let mut input = vec![0.0f32; 64];
            if block % 10 == 0 {
                input[3] = 1.0;
            }
            let (mut got, mut want) = (vec![0.0f32; 64], vec![0.0f32; 64]);
            node.process(&[&input], &mut [&mut got]);
            cpu.process(&[&input], &mut [&mut want]);
            let peak = want.iter().map(|v| v.abs()).fold(1e-3f32, f32::max);
            let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-3 * peak.max(1.0), "block {block}: off by {worst} (peak {peak})");
        }
        assert_eq!(node.missed_blocks(), 0);
    }

    #[test]
    fn a_large_matrix_mixer_goes_to_tensor_cores_and_mixes_alike() {
        let _serial = crate::serial();
        let (inputs, outputs) = (256, 256);
        let setup = setup(Policy::Balanced, false, 512);
        let mut mixer = MatrixMixerNode::new(inputs, outputs, &setup);
        eprintln!("{:?}", mixer.measurements());
        let mut cpu = MatrixMixerNode::new(inputs, outputs, &self::setup(Policy::Off, false, 512));
        for o in 0..outputs {
            for i in (o % 3..inputs).step_by(3) {
                let g = ((o * 31 + i * 17) % 100) as f32 / 1600.0;
                mixer.set_gain(o, i, g);
                cpu.set_gain(o, i, g);
            }
        }
        let input: Vec<Vec<f32>> = (0..inputs).map(|c| (0..512).map(|f| ((f + c * 5) as f32 * 0.02).sin()).collect()).collect();
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let (mut got, mut want) = (vec![vec![0.0f32; 512]; outputs], vec![vec![0.0f32; 512]; outputs]);
        mixer.process(&ins, &mut got.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
        cpu.process(&ins, &mut want.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
        let peak = want.iter().flatten().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = got.iter().flatten().zip(want.iter().flatten()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.1 && worst < 5e-3 * peak, "{:?}: off by {worst} (peak {peak})", mixer.device());
        if mixer.measurements().len() < 2 {
            return eprintln!("skipping the placement check: no Tensor cores here");
        }
        assert_eq!(mixer.device(), Device::Gpu, "256x256 channels: {:?}", mixer.measurements());
    }

    #[test]
    fn cpu_and_gpu_banks_agree() {
        let _serial = crate::serial();
        let lanes = resonators(300);
        let mut input = vec![0.0f32; 4096];
        input[0] = 1.0;
        input[1000] = -0.5;
        let mut cpu = CpuIirBank::new(&lanes, 4096);
        let mut want = vec![0.0f32; 4096];
        cpu.process(&[&input], &mut [&mut want]);
        let Ok(mut gpu) = IirBank::new(&lanes, 4096) else { return eprintln!("skipping: no GPU") };
        let mut got = vec![0.0f32; 4096];
        gpu.process(&[&input], &mut [&mut got]);
        let peak = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-4 * peak.max(1.0), "largest difference {worst} (peak {peak})");
    }
}
