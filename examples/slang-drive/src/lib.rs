//! A saturator whose DSP kernel is written once, in Slang
//! (`kernels/drive.slang`), and compiled for both the CPU and the GPU
//! (design doc 9.11). A `KernelNode` measures both and runs the faster one:
//! a kernel this light stays on the CPU, but the same source runs on the
//! GPU when that wins (for example offline, with heavier kernels).

use ferritron::prelude::*;
use ferritron_gpu::KernelNode;
use ferritron_gpu::graph::{Node, NodeSetup, Policy};

const CHANNELS: usize = 2;

#[cfg(not(no_slang))]
fn kernel() -> ferritron_gpu::Kernel {
    ferritron_gpu::slang_kernel!(drive)
}

/// Without Slang: the same formula in Rust, behind the same interface.
#[cfg(no_slang)]
fn kernel() -> ferritron_gpu::Kernel {
    unsafe extern "C" fn drive(x: f32, args: *const ferritron_gpu::KernelArgs) -> f32 {
        let args = unsafe { &*args };
        let drive = args.values[0].max(1e-6);
        (x * drive).tanh() / drive.tanh() * args.values[1]
    }
    ferritron_gpu::Kernel { name: "drive", wgsl: "", cpu: ferritron_gpu::KernelFn::Stateless(drive) }
}

#[derive(Default)]
struct SlangDrive {
    node: Option<KernelNode>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

#[ferritron::plugin]
impl Process for SlangDrive {
    fn activate(&mut self, setup: &Setup) {
        let policy = if cfg!(no_slang) { Policy::Off } else { Policy::resolve(Policy::Balanced) };
        let node = NodeSetup { sample_rate: setup.sample_rate, max_block: setup.max_block_size, offline: setup.offline, policy };
        self.node = Some(KernelNode::new(kernel(), CHANNELS, &node));
        self.inputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
        self.outputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
    }

    fn process(&mut self, ctx: &mut Context) {
        let Some(node) = self.node.as_mut() else { return };
        node.set(0, db_to_gain(ctx.params().drive.value()));
        node.set(1, db_to_gain(ctx.params().output.value()));
        let (n, channels) = (ctx.io().frames(), ctx.io().num_channels().min(CHANNELS));
        for (ch, input) in self.inputs.iter_mut().enumerate() {
            if ch < channels {
                input[..n].copy_from_slice(ctx.io().channel_mut(ch));
            } else {
                input[..n].fill(0.0);
            }
        }
        let ins: [&[f32]; CHANNELS] = std::array::from_fn(|c| &self.inputs[c][..n]);
        let [left, right] = &mut self.outputs[..] else { return };
        node.process(&ins, &mut [&mut left[..n], &mut right[..n]]);
        for ch in 0..channels {
            ctx.io().channel_mut(ch).copy_from_slice(&self.outputs[ch][..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    fn expected(x: f32, drive_db: f32, output_db: f32) -> f32 {
        let (d, o) = (db_to_gain(drive_db), db_to_gain(output_db));
        (x * d).tanh() / d.tanh() * o
    }

    #[test]
    fn saturates_like_the_formula() {
        let mut h = Harness::<SlangDrive>::new(48_000.0, 256);
        h.set_param("drive", 18.0);
        h.set_param("output", -3.0);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 / 40.0).sin() * 0.9).collect();
        let (mut l, mut r) = (input.clone(), input.clone());
        assert!(h.process(&mut [&mut l, &mut r]));
        for (i, (&y, &x)) in l.iter().zip(&input).enumerate() {
            assert!((y - expected(x, 18.0, -3.0)).abs() < 1e-5, "sample {i}: {y}");
        }
        assert_eq!(l, r);
    }

    #[cfg(not(no_slang))]
    #[test]
    fn the_same_slang_kernel_runs_on_the_gpu() {
        let kernel = kernel();
        let Ok(mut gpu) = ferritron_gpu::GpuKernel::new(&kernel, 2, 1024) else { return eprintln!("skipping: no GPU") };
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 / 17.0).sin() * 1.5).collect();
        let mut values = [0.0f32; 8];
        values[0] = 6.0;
        values[1] = 0.5;
        let (mut l, mut r) = (vec![0.0f32; 1024], vec![0.0f32; 1024]);
        gpu.process(&[&input, &input], &mut [&mut l, &mut r], &values);
        let args = ferritron_gpu::KernelArgs { values, ..Default::default() };
        for (i, (&g, &x)) in l.iter().zip(&input).enumerate() {
            let cpu = kernel.cpu(x, &args, &mut Default::default());
            assert!((g - cpu).abs() < 1e-5, "sample {i}: GPU {g}, CPU {cpu}");
        }
        assert_eq!(l, r);
    }

    /// Kernel nodes in series fuse into one chain (one GPU round trip per
    /// block); the original ids still reach each kernel's parameters.
    #[cfg(not(no_slang))]
    #[test]
    fn kernel_nodes_in_series_fuse() {
        use ferritron_gpu::graph::Graph;
        let build = |fusion: bool, policy: Policy| {
            let setup = NodeSetup { sample_rate: 48_000.0, max_block: 512, offline: true, policy };
            let mut g = Graph::new(2, 2);
            let drive = g.add(KernelNode::new(kernel(), 2, &setup));
            let smooth = g.add(KernelNode::new(ferritron_gpu::slang_kernel!(smooth, stateful), 2, &setup));
            g.connect_all(Graph::INPUT, drive).unwrap();
            g.connect_all(drive, smooth).unwrap();
            g.connect_all(smooth, Graph::OUTPUT).unwrap();
            g.set_fusion(fusion);
            g.prepare(512).unwrap();
            for (id, index, value) in [(drive, 0, 4.0), (drive, 1, 0.8), (smooth, 0, 0.05)] {
                g.node_mut::<KernelNode>(id).unwrap().set(index, value);
            }
            g
        };
        let input: Vec<f32> = (0..512).map(|i| (i as f32 / 9.0).sin()).collect();
        let render = |g: &mut Graph| {
            let mut out = Vec::new();
            for _ in 0..3 {
                let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
                g.process(&[&input, &input], &mut [&mut l, &mut r]);
                out.extend(l);
            }
            out
        };
        for policy in [Policy::Off, Policy::Max] {
            let mut fused = build(true, policy);
            let mut separate = build(false, policy);
            assert_eq!(fused.fused_nodes(), 1);
            let (a, b) = (render(&mut fused), render(&mut separate));
            let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-5, "{policy:?}: fused differs by {worst}");
            assert!(a.iter().any(|v| v.abs() > 0.1), "the chain produced sound");
        }
        // The GPU chain itself, against the CPU.
        let Ok(mut gpu) = ferritron_gpu::GpuKernel::chain(&[kernel(), ferritron_gpu::slang_kernel!(smooth, stateful)], 1, 512) else { return eprintln!("skipping: no GPU") };
        let mut out = vec![0.0f32; 512];
        gpu.process_chain(&[&input], &mut [&mut out], &[[4.0, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], [0.05, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]]);
        let mut cpu = KernelNode::chain(&[kernel(), ferritron_gpu::slang_kernel!(smooth, stateful)], 1, &NodeSetup { sample_rate: 48_000.0, max_block: 512, offline: false, policy: Policy::Off });
        cpu.set(0, 4.0);
        cpu.set(1, 0.8);
        cpu.select_member(1);
        cpu.set(0, 0.05);
        let mut want = vec![0.0f32; 512];
        cpu.process(&[&input], &mut [&mut want]);
        let worst = out.iter().zip(&want).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "GPU chain differs from the CPU by {worst}");
    }

    /// A stateful kernel (a one-pole smoother): the GPU runs each channel in
    /// order and keeps its state across blocks, like the CPU.
    #[cfg(not(no_slang))]
    #[test]
    fn a_stateful_kernel_keeps_its_state_on_the_cpu_and_the_gpu() {
        let kernel = ferritron_gpu::slang_kernel!(smooth, stateful);
        assert!(kernel.is_stateful());
        let channels = 64;
        let Ok(mut gpu) = ferritron_gpu::GpuKernel::new(&kernel, channels, 512) else { return eprintln!("skipping: no GPU") };
        let mut values = [0.0f32; 8];
        values[0] = 0.01;
        let mut states = vec![ferritron_gpu::KernelState::default(); channels];
        for block in 0..3 {
            let inputs: Vec<Vec<f32>> = (0..channels).map(|c| (0..512).map(|i| ((i + block * 512) as f32 * 0.01 + c as f32).sin()).collect()).collect();
            let mut outputs = vec![vec![0.0f32; 512]; channels];
            gpu.process(&inputs.iter().map(|c| &c[..]).collect::<Vec<_>>(), &mut outputs.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>(), &values);
            for (c, (input, output)) in inputs.iter().zip(&outputs).enumerate() {
                let args = ferritron_gpu::KernelArgs { values, channel: c as u32, frame: 0 };
                for (i, (&x, &g)) in input.iter().zip(output).enumerate() {
                    let want = kernel.cpu(x, &args, &mut states[c]);
                    assert!((g - want).abs() < 1e-5, "block {block}, channel {c}, sample {i}: GPU {g}, CPU {want}");
                }
            }
        }
        gpu.reset();
        let mut out = vec![vec![0.0f32; 4]; channels];
        let ones = vec![vec![1.0f32; 4]; channels];
        gpu.process(&ones.iter().map(|c| &c[..]).collect::<Vec<_>>(), &mut out.iter_mut().map(|c| &mut c[..]).collect::<Vec<_>>(), &values);
        assert!((out[0][0] - 0.01).abs() < 1e-6, "reset clears the state: {}", out[0][0]);
    }
}

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<SlangDrive>(include_str!("../../../tests/fixtures/1.4.0/slang-drive.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}
