//! "Ferritron Chain" (design doc 9.7): several modules in one plugin, built
//! into one node graph. A plugin per module would cost a GPU round trip per
//! module (the DAW passes audio between plugins on the CPU); in one graph,
//! neighbouring GPU nodes of different modules fuse, and the chain runs in
//! one round trip.
//!
//! The modules: `Body` (1,024 resonators, a resonant body for any input),
//! `Tone` (an EQ per channel) and `Level` (a gain). Body and Tone are both
//! IIR banks, so they fuse across the module border.

use ferritron::prelude::*;
use ferritron_gpu::graph::{Gain, Graph, GraphError, Module, NodeId, NodeSetup, Policy};
use ferritron_gpu::{Biquad, IirNode, Lane};

const CHANNELS: usize = 2;

/// 1,024 resonators, alternating between the two channels.
struct Body;

impl Module for Body {
    fn build(&mut self, graph: &mut Graph, setup: &NodeSetup) -> Result<(NodeId, NodeId), GraphError> {
        let lanes: Vec<Lane> = (0..1024)
            .map(|i| {
                let freq = 90.0 * 1.0035f64.powi(i as i32);
                let q = 60.0 + (i % 5) as f64 * 30.0;
                let mut b = Biquad::bandpass(freq.min(setup.sample_rate * 0.45), q, setup.sample_rate);
                // Quieter towards the top, so the body stays warm.
                let gain = (0.02 / (1.0 + i as f64 / 256.0)) as f32;
                (b.b0, b.b1, b.b2) = (b.b0 * gain, b.b1 * gain, b.b2 * gain);
                Lane { input: i % CHANNELS, output: i % CHANNELS, sections: vec![b] }
            })
            .collect();
        let id = graph.add(IirNode::new(&lanes, setup));
        Ok((id, id))
    }
}

/// A four-band EQ per channel (peaking sections in series); the tone
/// parameter tilts it.
struct Tone;

/// The EQ for `gain_db` of tilt: cut lows, lift highs.
fn tone_sections(gain_db: f64, rate: f64) -> Vec<Biquad> {
    [(250.0, -0.5), (900.0, -0.2), (3000.0, 0.6), (8000.0, 1.0)]
        .iter()
        .map(|&(freq, weight)| Biquad::peaking(freq, 0.8, gain_db * weight, rate))
        .collect()
}

impl Module for Tone {
    fn build(&mut self, graph: &mut Graph, setup: &NodeSetup) -> Result<(NodeId, NodeId), GraphError> {
        let lanes: Vec<Lane> =
            (0..CHANNELS).map(|c| Lane { input: c, output: c, sections: tone_sections(0.0, setup.sample_rate) }).collect();
        let id = graph.add(IirNode::new(&lanes, setup));
        Ok((id, id))
    }
}

/// Output level.
struct Level;

impl Module for Level {
    fn build(&mut self, graph: &mut Graph, _: &NodeSetup) -> Result<(NodeId, NodeId), GraphError> {
        let id = graph.add(Gain::new(CHANNELS, 1.0));
        Ok((id, id))
    }
}

#[derive(Default)]
struct GpuChain {
    graph: Option<Graph>,
    /// (input, output) node of each module: Body, Tone, Level.
    ends: Vec<(NodeId, NodeId)>,
    tone: f64,
    sample_rate: f64,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

impl GpuChain {
    /// Sets the tone module's EQ (through its own node id, also when fused).
    fn set_tone(&mut self, gain_db: f64) {
        let (Some(graph), Some(&(tone, _))) = (self.graph.as_mut(), self.ends.get(1)) else { return };
        let Some(node) = graph.node_mut::<IirNode>(tone) else { return };
        let sections = tone_sections(gain_db, self.sample_rate);
        for c in 0..CHANNELS {
            node.set_lane(c, &sections);
        }
        self.tone = gain_db;
    }
}

#[ferritron::plugin]
impl Process for GpuChain {
    fn activate(&mut self, setup: &Setup) {
        let node = NodeSetup {
            sample_rate: setup.sample_rate,
            max_block: setup.max_block_size,
            offline: setup.offline,
            policy: Policy::resolve(Policy::Max),
        };
        self.sample_rate = setup.sample_rate;
        let (mut body, mut tone, mut level) = (Body, Tone, Level);
        if let Ok((graph, ends)) = Graph::chain(CHANNELS, &mut [&mut body, &mut tone, &mut level], &node) {
            self.graph = Some(graph);
            self.ends = ends;
        }
        self.tone = f64::NAN;
        self.inputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
        self.outputs = vec![vec![0.0; setup.max_block_size]; CHANNELS];
    }

    fn latency(&self) -> u32 {
        self.graph.as_ref().map_or(0, Graph::latency)
    }

    fn process(&mut self, ctx: &mut Context) {
        let tone = ctx.params.tone.value() as f64;
        if tone != self.tone {
            self.set_tone(tone);
        }
        let volume = db_to_gain(ctx.params.volume.value());
        let (Some(graph), Some(&(level, _))) = (self.graph.as_mut(), self.ends.get(2)) else { return };
        if let Some(gain) = graph.node_mut::<Gain>(level) {
            gain.set(volume);
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

    fn setup(offline: bool) -> NodeSetup {
        NodeSetup { sample_rate: 48_000.0, max_block: 256, offline, policy: Policy::Max }
    }

    /// The three modules in one graph, with or without fusion.
    fn build(fusion: bool) -> Graph {
        let setup = setup(true);
        let mut g = Graph::new(CHANNELS, CHANNELS);
        let mut prev = Graph::INPUT;
        let (mut body, mut tone, mut level) = (Body, Tone, Level);
        for module in [&mut body as &mut dyn Module, &mut tone, &mut level] {
            let (first, last) = module.build(&mut g, &setup).unwrap();
            g.connect_all(prev, first).unwrap();
            prev = last;
        }
        g.connect_all(prev, Graph::OUTPUT).unwrap();
        g.set_fusion(fusion);
        g.prepare(256).unwrap();
        g
    }

    fn render(g: &mut Graph) -> Vec<f32> {
        let mut out = Vec::new();
        for b in 0..20 {
            let mut input = vec![0.0f32; 256];
            if b % 5 == 0 {
                input[3] = 1.0;
            }
            let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
            g.process(&[&input, &input], &mut [&mut l, &mut r]);
            out.extend(l);
            out.extend(r);
        }
        out
    }

    #[test]
    fn modules_fuse_across_their_borders_and_sound_the_same() {
        let (mut fused, mut separate) = (build(true), build(false));
        assert_eq!(fused.fused_nodes(), 1, "Body and Tone became one chain");
        assert_eq!(separate.fused_nodes(), 0);
        let (a, b) = (render(&mut fused), render(&mut separate));
        let peak = b.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(peak > 1e-4 && worst < 1e-4 * peak.max(1.0), "fused differs by {worst} (peak {peak})");
    }

    #[test]
    fn the_plugin_reports_the_latency_of_one_gpu_round_trip() {
        let h = ferritron::testing::Harness::<GpuChain>::new(48_000.0, 256);
        let latency = h.latency();
        assert!(latency == 0 || latency == 512, "one GPU bank's latency at most, got {latency}");
    }
}
