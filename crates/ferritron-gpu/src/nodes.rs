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
use ferritron_graph::{Device, Measured, Node, NodeSetup, Policy, choose_keyed, config_key};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Keys of the placement memory (see `ferritron_graph::choose_keyed`): one
/// per kind of node, then its sizes.
const IIR_KEY: u64 = 0x11;
const MIXER_KEY: u64 = 0x12;

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
    /// The device-lost mark of the context a real-time GPU bank runs on.
    gpu_lost: Option<Arc<AtomicBool>>,
    /// The other side: the CPU candidate of a GPU bank, or the GPU bank
    /// parked after a switch to the CPU (see `fall_back`).
    fallback: Option<Box<IirNode>>,
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
        Self::build(&[lanes], setup, true, None).expect("a single stage is always a valid chain")
    }

    /// Banks in series as one node ([`IirBank::chain`]). On the GPU the
    /// whole chain is fused: one round trip per block instead of one per
    /// bank. Fails only if a stage reads a channel the previous one lacks.
    pub fn chain(stages: &[&[Lane]], setup: &NodeSetup) -> Result<Self, String> {
        Self::build(stages, setup, false, None)
    }

    /// Like [`chain`](Self::chain), on `device` if the policy allows it
    /// (no measuring then). The placement is remembered for this
    /// configuration in the process, so a second bank of the same size is
    /// placed without measuring.
    fn build(stages: &[&[Lane]], setup: &NodeSetup, low_latency: bool, fixed: Option<Device>) -> Result<Self, String> {
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
            gpu_lost: None,
            fallback: None,
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
        // A persistent kernel is chosen by its own call only, so it is not remembered.
        let lanes: u64 = stages.iter().map(|s| s.len() as u64).sum();
        let key = (!low_latency).then(|| config_key(&[IIR_KEY, stages.len() as u64, lanes, setup.max_block as u64, setup.policy as u64, setup.offline as u64]));
        let (mut chosen, measured) = choose_keyed(key, fixed, candidates, setup).expect("the CPU candidate is always there");
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
        // A GPU bank that can be lost keeps a CPU candidate beside it (built
        // now, off the audio thread); the context's lost mark is read later.
        if matches!(chosen.bank, Bank::Gpu(_)) {
            chosen.gpu_lost = crate::context().ok().map(|ctx| ctx.lost.clone());
        }
        if matches!(chosen.bank, Bank::Gpu(_) | Bank::Persistent(_)) {
            chosen.fallback = chosen.cpu_twin().map(Box::new);
        }
        chosen.measured = measured;
        Ok(chosen)
    }

    /// The CPU bank with this node's current coefficients and an empty state:
    /// the candidate a lost GPU bank falls back to.
    fn cpu_twin(&self) -> Option<Self> {
        let stages: Vec<&[Lane]> = self.stages.iter().map(|s| &s[..]).collect();
        let cpu = CpuIirBank::chain(&stages, self.setup.max_block).ok()?;
        Some(Self {
            bank: Bank::Cpu(cpu),
            inputs: self.inputs,
            outputs: self.outputs,
            measured: Vec::new(),
            stages: self.stages.clone(),
            setup: self.setup,
            members: self.members.clone(),
            active: self.active,
            gpu_lost: None,
            fallback: None,
        })
    }

    /// Builds a lost persistent kernel again on the current device. A real-time
    /// `Gpu` bank is not built again here (its GPU thread belongs to it): it
    /// stays lost. On an error the bank stays on the CPU.
    fn rebuild_lost(&mut self) {
        if !self.lost() {
            return;
        }
        let Bank::Persistent(_) = &self.bank else { return };
        let Some(placeholder) = self.cpu_twin() else { return };
        let Bank::Persistent(old) = std::mem::replace(&mut self.bank, placeholder.bank) else { return };
        if let Ok(fresh) = (*old).revive() {
            self.bank = Bank::Persistent(Box::new(fresh));
        }
    }

    /// Where the bank runs (for the graph's placement, see
    /// [`Node::placement`]).
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
        self.set_lane_here(lane, sections);
        if let Some(other) = self.fallback.as_mut() {
            other.set_lane_here(lane, sections);
        }
    }

    /// Sets lane `lane` (counted from the first member) on this side only.
    fn set_lane_here(&mut self, lane: usize, sections: &[Biquad]) {
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

    /// A real-time GPU bank whose context is lost, or a persistent kernel whose device is lost.
    fn lost(&self) -> bool {
        match &self.bank {
            Bank::Gpu(_) => self.gpu_lost.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)),
            Bank::Persistent(bank) => bank.lost(),
            Bank::Cpu(_) | Bank::GpuOffline(_) => false,
        }
    }

    /// A lost GPU bank becomes its CPU candidate; the GPU bank is kept.
    fn fall_back(&mut self) -> bool {
        if !self.lost() {
            return false;
        }
        let Some(mut other) = self.fallback.take() else { return false };
        std::mem::swap(self, &mut *other);
        self.fallback = Some(other);
        true
    }

    /// Builds a lost persistent kernel again and goes back to the GPU when it
    /// works. Off the audio thread.
    fn revive(&mut self) {
        self.fall_back();
        if let Some(parked) = self.fallback.as_mut() {
            parked.rebuild_lost();
        }
        if self.device() == Device::Cpu && self.fallback.as_ref().is_some_and(|f| f.device() == Device::Gpu && !f.lost()) {
            let Some(mut other) = self.fallback.take() else { return };
            std::mem::swap(self, &mut *other);
            self.fallback = Some(other);
            self.reset();
            if let Some(cpu) = self.fallback.as_mut() {
                cpu.reset();
            }
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

    fn placement(&self) -> Option<Device> {
        Some(self.device())
    }

    /// Builds the bank again from its stages (the current coefficients) on
    /// `device`. Persistent kernels are not rebuilt (`new_low_latency` is
    /// for them); the node keeps its members and the selected one.
    fn set_placement(&mut self, device: Device) -> bool {
        let stages: Vec<Vec<Lane>> = self.stages.clone();
        let refs: Vec<&[Lane]> = stages.iter().map(|s| &s[..]).collect();
        let Ok(mut fresh) = Self::build(&refs, &self.setup, false, Some(device)) else { return false };
        if fresh.device() != device {
            return false; // the policy does not allow it: the node stays as it was
        }
        fresh.members = std::mem::take(&mut self.members);
        fresh.active = self.active;
        *self = fresh;
        true
    }
}

/// The persistent-kernel bank for `stages`, if they fit it: one stage of
/// one-biquad lanes, at most 8 outputs, and a GPU with native Vulkan.
fn persistent(stages: &[&[Lane]], inputs: usize, outputs: usize, setup: &NodeSetup) -> Option<PersistentModalBank> {
    let [lanes] = stages else { return None };
    // Only on wgpu's own device: on a device of its own, a persistent
    // kernel would make wgpu's work wait for it.
    if !crate::shared_vulkan().is_ok_and(|g| g.is_shared()) {
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

    /// The convolver's device is lost; it passes its input through and builds
    /// itself again on a new device (see [`Convolver::device_lost`]). It has
    /// no CPU candidate, so the graph does not switch it.
    fn lost(&self) -> bool {
        self.convolver.device_lost()
    }

    /// Always the GPU: there is no CPU candidate (see [`ConvolverNode`]).
    fn placement(&self) -> Option<Device> {
        Some(Device::Gpu)
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
    setup: NodeSetup,
    /// The other side: the CPU mixer of a Tensor-core mixer, or the Tensor
    /// mixer parked after a switch to the CPU (see `fall_back`).
    fallback: Option<Box<MatrixMixerNode>>,
}

impl MatrixMixerNode {
    /// All weights start at zero. Call off the audio thread.
    pub fn new(inputs: usize, outputs: usize, setup: &NodeSetup) -> Self {
        Self::build(inputs, outputs, setup, None)
    }

    fn build(inputs: usize, outputs: usize, setup: &NodeSetup, fixed: Option<Device>) -> Self {
        let node = |tensor| Self { inputs, outputs, weights: vec![0.0; inputs * outputs], tensor, measured: Vec::new(), setup: *setup, fallback: None };
        let mut candidates = vec![(Device::Cpu, node(None))];
        // Tensor cores wait for the GPU (tens of microseconds): no latency.
        if setup.policy != Policy::Off
            && crate::context().is_ok()
            && let Ok(tensor) = TensorMixer::new(inputs, outputs, setup.max_block)
        {
            candidates.push((Device::Gpu, node(Some(tensor))));
        }
        let key = config_key(&[MIXER_KEY, inputs as u64, outputs as u64, setup.max_block as u64, setup.policy as u64, setup.offline as u64]);
        let (mut chosen, measured) = choose_keyed(Some(key), fixed, candidates, setup).expect("the CPU candidate is always there");
        // A Tensor-core mixer keeps the CPU mixer beside it, for a lost device.
        if chosen.tensor.is_some() {
            chosen.fallback = Some(Box::new(Self { inputs, outputs, weights: chosen.weights.clone(), tensor: None, measured: Vec::new(), setup: *setup, fallback: None }));
        }
        chosen.measured = measured;
        chosen
    }

    /// Sets the weight of input `input` in output `output`. Real-time safe.
    pub fn set_gain(&mut self, output: usize, input: usize, gain: f32) {
        self.store_gain(output, input, gain);
        if let Some(other) = self.fallback.as_mut() {
            other.store_gain(output, input, gain);
        }
    }

    /// Sets the weight on this side only.
    fn store_gain(&mut self, output: usize, input: usize, gain: f32) {
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
    fn placement(&self) -> Option<Device> {
        Some(self.device())
    }

    /// The Tensor-core mixer's device is lost.
    fn lost(&self) -> bool {
        self.tensor.as_ref().is_some_and(|t| t.lost())
    }

    /// A lost Tensor-core mixer becomes its CPU mixer; the Tensor mixer is kept.
    fn fall_back(&mut self) -> bool {
        if !self.lost() {
            return false;
        }
        let Some(mut other) = self.fallback.take() else { return false };
        std::mem::swap(self, &mut *other);
        self.fallback = Some(other);
        true
    }

    /// Builds a lost Tensor-core mixer again (its weights kept) and goes back to
    /// the Tensor cores when that works. Off the audio thread.
    fn revive(&mut self) {
        self.fall_back();
        if let Some(parked) = self.fallback.as_mut()
            && let Some(tensor) = parked.tensor.take()
        {
            parked.tensor = if tensor.lost() { tensor.revive().ok() } else { Some(tensor) };
        }
        if self.tensor.is_none() && self.fallback.as_ref().is_some_and(|f| f.tensor.is_some() && !f.lost()) {
            let Some(mut other) = self.fallback.take() else { return };
            std::mem::swap(self, &mut *other);
            self.fallback = Some(other);
        }
    }

    /// Builds the mixer again on `device`, keeping its weights. Off the audio thread.
    fn set_placement(&mut self, device: Device) -> bool {
        let mut fresh = Self::build(self.inputs, self.outputs, &self.setup, Some(device));
        if fresh.device() != device {
            return false; // the policy does not allow it: the node stays as it was
        }
        for o in 0..self.outputs {
            for i in 0..self.inputs {
                fresh.set_gain(o, i, self.weights[o * self.inputs + i]);
            }
        }
        *self = fresh;
        true
    }

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

    /// The wgpu side hands over a new device after a loss (by hand: its device
    /// is marked lost, and the next `context()` opens a new one, which adopts it).
    fn wgpu_hands_over_a_new_device() {
        if let Ok(old) = crate::context() {
            old.lost.store(true, Ordering::Relaxed);
            // Kept alive on purpose: the lost Vulkan objects still hold its raw
            // device handle and drop through it. Without this, wgpu destroys the
            // device and their drops call into it (access violation seen).
            std::mem::forget(old);
        }
        crate::context().expect("a new wgpu device");
    }

    /// G-f7 2-1 and 2-2 (hand-set loss, no device was lost): a persistent
    /// kernel marked lost runs its CPU candidate from the next block, with the
    /// same output as the CPU bank; `revive` builds the kernel again and goes
    /// back to it. Skipped where there is no persistent kernel.
    #[test]
    fn a_lost_persistent_kernel_runs_on_the_cpu_and_goes_back_after_revive() {
        let _serial = crate::serial();
        let lanes = resonators(3000);
        let mut node = IirNode::new_low_latency(&lanes, &setup(Policy::Max, false, 64));
        if !node.is_persistent() {
            // A real choice needs the shared device, which this run did not
            // have: build the persistent bank as `build` does, on Ferritron's
            // own device, and give the node its CPU candidate.
            let modal: Vec<ModalLane> = lanes
                .iter()
                .map(|l| {
                    let s = l.sections[0];
                    ModalLane { b0: s.b0, b1: s.b1, b2: s.b2, a1: s.a1, a2: s.a2, input: l.input as u32, output: l.output as u32, pad: 0 }
                })
                .collect();
            let budget = std::time::Duration::from_secs_f64(64.0 / 48_000.0 / 2.0);
            let Ok(mut bank) = PersistentModalBank::new(&modal, 1, 1, 64, budget) else {
                return eprintln!("skipping: no persistent kernel here ({:?})", node.device());
            };
            bank.settle();
            bank.reset();
            node.bank = Bank::Persistent(Box::new(bank));
            node.fallback = node.cpu_twin().map(Box::new);
        }
        match &node.bank {
            Bank::Persistent(bank) => bank.simulate_loss(),
            _ => unreachable!("is_persistent"),
        }
        assert!(node.lost(), "the bank reports its lost device");
        assert!(node.fall_back(), "a lost bank runs its CPU candidate");
        assert_eq!(node.device(), Device::Cpu);
        let mut cpu = CpuIirBank::new(&lanes, 64);
        let mut input = vec![0.0f32; 64];
        input[3] = 1.0;
        let (mut got, mut want) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        node.process(&[&input], &mut [&mut got]);
        cpu.process(&[&input], &mut [&mut want]);
        assert_eq!(got, want, "the CPU candidate answers like the CPU bank");

        wgpu_hands_over_a_new_device();
        node.revive();
        assert_eq!(node.device(), Device::Gpu, "built again, and back on the GPU");
        assert!(!node.lost(), "the new kernel is not on a lost device");
    }

    /// G-f7 2-1 and 2-2 for the Tensor-core mixer (hand-set loss): the CPU
    /// mixer answers exactly like a CPU-only mixer with the same weights, and
    /// after `revive` the Tensor-core mixer answers within f16 precision.
    #[test]
    fn a_lost_matrix_mixer_runs_on_the_cpu_and_goes_back_with_its_weights() {
        let _serial = crate::serial();
        let (inputs, outputs, frames) = (256, 256, 512);
        let mut mixer = MatrixMixerNode::new(inputs, outputs, &setup(Policy::Max, false, frames));
        if mixer.device() != Device::Gpu {
            return eprintln!("skipping: no Tensor cores here ({:?})", mixer.measurements());
        }
        let mut cpu = MatrixMixerNode::new(inputs, outputs, &setup(Policy::Off, false, frames));
        for o in 0..outputs {
            for i in (o % 3..inputs).step_by(3) {
                let g = ((o * 31 + i * 17) % 100) as f32 / 1600.0;
                mixer.set_gain(o, i, g);
                cpu.set_gain(o, i, g);
            }
        }
        let input: Vec<Vec<f32>> = (0..inputs).map(|c| (0..frames).map(|f| ((f + c * 5) as f32 * 0.02).sin()).collect()).collect();
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let run = |node: &mut MatrixMixerNode| {
            let mut out = vec![vec![0.0f32; frames]; outputs];
            node.process(&ins, &mut out.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>());
            out
        };
        let want = run(&mut cpu);
        if let Some(tensor) = &mixer.tensor {
            tensor.simulate_loss();
        }
        assert!(mixer.lost(), "the mixer reports its lost device");
        assert!(mixer.fall_back(), "a lost mixer runs its CPU mixer");
        assert_eq!(mixer.device(), Device::Cpu);
        assert!(run(&mut mixer) == want, "the CPU side answers exactly like a CPU-only mixer");

        wgpu_hands_over_a_new_device();
        mixer.revive();
        assert_eq!(mixer.device(), Device::Gpu, "built again, with its weights, back on the Tensor cores");
        let back = run(&mut mixer);
        let peak = want.iter().flatten().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = back.iter().flatten().zip(want.iter().flatten()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.1 && worst < 5e-3 * peak, "after revive: off by {worst} (peak {peak})");
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

    /// G4: a configuration keeps the placement it got (the same size is placed
    /// without measuring), and `set_placement` builds the node again on the
    /// device asked for, with the same output as that device's bank.
    #[test]
    fn placement_is_remembered_and_can_be_fixed() {
        let _serial = crate::serial();
        let lanes = resonators(4000);
        let s = setup(Policy::Max, true, 4096);
        let mut first = IirNode::new(&lanes, &s);
        if first.device() != Device::Gpu {
            return eprintln!("skipping: the bank does not run on the GPU here");
        }
        let second = IirNode::new(&lanes, &s);
        assert!(second.measurements().iter().all(|m| m.micros.is_infinite()), "the remembered placement is used without measuring");
        assert_eq!(second.device(), Device::Gpu);
        // Fix it on the CPU: the node is built again there, and plays what the CPU bank plays.
        assert!(first.set_placement(Device::Cpu));
        assert_eq!(first.placement(), Some(Device::Cpu));
        let mut cpu = CpuIirBank::new(&lanes, 4096);
        let mut input = vec![0.0f32; 4096];
        input[0] = 1.0;
        input[1000] = -0.5;
        let (mut got, mut want) = (vec![0.0f32; 4096], vec![0.0f32; 4096]);
        first.process(&[&input], &mut [&mut got]);
        cpu.process(&[&input], &mut [&mut want]);
        let peak = want.iter().map(|v| v.abs()).fold(1e-3f32, f32::max);
        let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-4 * peak, "fixed on the CPU: off by {worst} (peak {peak})");
        // Policy::Off allows only the CPU: asking for the GPU is refused, and the node stays.
        let mut off = IirNode::new(&lanes, &setup(Policy::Off, true, 4096));
        assert!(!off.set_placement(Device::Gpu));
        assert_eq!(off.placement(), Some(Device::Cpu));
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
