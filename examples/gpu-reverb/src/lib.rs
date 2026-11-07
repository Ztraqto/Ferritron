//! A convolution reverb with a 2-second impulse response, built as a node
//! graph (design doc 9.2): the input goes through a dry gain, and through a
//! GPU convolution and a wet gain per channel; the graph sums both at the
//! output. The convolution's first taps run on the CPU (zero latency), the
//! long tail on the GPU. Offline, it waits for the GPU, so bounces are
//! complete. Without a GPU (or with `FERRITRON_GPU_POLICY=off`), only the
//! dry path is built.

use ferritron::prelude::*;
use ferritron_gpu::ConvolverNode;
use ferritron_gpu::graph::{Gain, Graph, NodeId, NodeSetup, Policy};

const SECONDS: f32 = 2.0;
const CHANNELS: usize = 2;

#[derive(Default)]
struct GpuReverb {
    graph: Option<Graph>,
    dry: Option<NodeId>,
    wet: Vec<NodeId>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

/// A simple synthetic room: exponentially decaying noise, different per channel.
fn impulse_response(sample_rate: f32, seed: u32) -> Vec<f32> {
    let len = (sample_rate * SECONDS) as usize;
    let mut s = seed;
    (0..len)
        .map(|i| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let noise = (s as f32 / u32::MAX as f32) * 2.0 - 1.0;
            noise * (-6.9 * i as f32 / len as f32).exp() * 0.05
        })
        .collect()
}

#[ferritron::plugin]
impl Process for GpuReverb {
    fn activate(&mut self, setup: &Setup) {
        let rate = setup.sample_rate as f32;
        let node = NodeSetup {
            sample_rate: setup.sample_rate,
            max_block: setup.max_block_size,
            offline: setup.offline,
            policy: Policy::resolve(Policy::Balanced),
        };
        let mut graph = Graph::new(CHANNELS, CHANNELS);
        let dry = graph.add(Gain::new(CHANNELS, 1.0));
        let wiring = graph.connect_all(Graph::INPUT, dry).and_then(|_| graph.connect_all(dry, Graph::OUTPUT));
        self.wet.clear();
        for ch in 0..CHANNELS {
            // Built once per process and shared by every instance (and on
            // the GPU, its spectra are shared by every convolver).
            let seed = 0x1234_5678 + ch as u32;
            let ir = ferritron::share::cached(&format!("gpu-reverb/ir/{rate}/{seed}"), || impulse_response(rate, seed));
            let Ok(conv) = ConvolverNode::new(&ir, &node) else { break };
            let conv = graph.add(conv);
            let wet = graph.add(Gain::new(1, 0.0));
            graph.connect(Graph::INPUT, ch, conv, 0).expect("valid channels");
            graph.connect(conv, 0, wet, 0).expect("valid channels");
            graph.connect(wet, 0, Graph::OUTPUT, ch).expect("valid channels");
            self.wet.push(wet);
        }
        self.graph = wiring.and_then(|_| graph.prepare(setup.max_block_size)).ok().map(|_| graph);
        self.dry = Some(dry);
        self.inputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
        self.outputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
    }

    fn reset(&mut self) {
        if let Some(graph) = &mut self.graph {
            graph.reset();
        }
    }

    fn process(&mut self, ctx: &mut Context) {
        let (Some(graph), Some(dry)) = (self.graph.as_mut(), self.dry) else { return };
        let mix = ctx.params.mix.value() / 100.0;
        // Without convolvers, the dry path passes everything.
        let dry_gain = if self.wet.is_empty() { 1.0 } else { 1.0 - mix };
        if let Some(gain) = graph.node_mut::<Gain>(dry) {
            gain.set(dry_gain);
        }
        for &wet in &self.wet {
            if let Some(gain) = graph.node_mut::<Gain>(wet) {
                gain.set(mix);
            }
        }
        let (n, channels) = (ctx.io.frames(), ctx.io.num_channels().min(CHANNELS));
        for (ch, input) in self.inputs.iter_mut().enumerate() {
            if ch < channels {
                input[..n].copy_from_slice(ctx.io.channel_mut(ch));
            } else {
                input[..n].fill(0.0);
            }
        }
        let ins: [&[f32]; CHANNELS] = std::array::from_fn(|c| &self.inputs[c][..n]);
        let [left, right] = &mut self.outputs[..] else { return };
        graph.process(&ins, &mut [&mut left[..n], &mut right[..n]]);
        for ch in 0..channels {
            ctx.io.channel_mut(ch).copy_from_slice(&self.outputs[ch][..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    /// The reverb's wet path needs a GPU; without one the plugin is dry only.
    fn gpu() -> bool {
        let ok = ferritron_gpu::Convolver::new(&[0.0; 4096], 64).is_ok();
        if !ok {
            eprintln!("skipping: no GPU");
        }
        ok
    }

    #[test]
    fn an_impulse_comes_back_as_a_long_tail() {
        if !gpu() {
            return;
        }
        let mut h = Harness::<GpuReverb>::new(48_000.0, 256);
        h.set_param("mix", 100.0);
        let mut energy_late = 0.0f32;
        for block in 0..300 {
            let mut buf = vec![0.0f32; 256];
            if block == 0 {
                buf[0] = 1.0;
            }
            assert!(h.process(&mut [&mut buf]));
            if block > 100 {
                // more than 0.5 s after the impulse: only the GPU tail can produce this
                energy_late += buf.iter().map(|s| s * s).sum::<f32>();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(energy_late > 1e-6, "no reverb tail after 0.5 s (energy {energy_late})");
    }

    /// Renders an impulse as fast as possible; returns the output.
    fn bounce(mut h: Harness<GpuReverb>) -> Vec<f32> {
        h.set_param("mix", 100.0);
        let mut out = Vec::new();
        for block in 0..300 {
            let mut buf = vec![0.0f32; 256];
            if block == 0 {
                buf[0] = 1.0;
            }
            assert!(h.process(&mut [&mut buf]));
            out.extend_from_slice(&buf);
        }
        out
    }

    #[test]
    fn an_offline_bounce_has_the_whole_tail() {
        if !gpu() {
            return;
        }
        // Same impulse, rendered without real-time pacing. Offline, every
        // block waits for its GPU tail, so the result equals a paced real-time
        // run; in real-time mode, a fast bounce would lose most of the tail.
        let offline = bounce(Harness::offline(48_000.0, 256));
        let late: f32 = offline[48_000..].iter().map(|s| s * s).sum();
        assert!(late > 1e-6, "the tail is missing from the bounce (energy {late})");
        let ir = impulse_response(48_000.0, 0x1234_5678);
        let worst = offline.iter().zip(&ir).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "the bounce is the impulse response (largest difference {worst})");
    }
}
