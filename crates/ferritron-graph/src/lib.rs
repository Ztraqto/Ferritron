//! Hybrid node graph of Ferritron (design doc 9.2).
//!
//! A [`Graph`] connects DSP [`Node`]s, channel to channel, and runs them in
//! dependency order every block. Signals arriving at the same input are
//! summed. Nodes may add latency (a GPU pipeline, a look-ahead); the graph
//! delays the other paths so that everything meeting at a node lines up, and
//! reports the total as [`Graph::latency`].
//!
//! Where a node can run is decided by measurement: [`choose`] runs the CPU
//! and GPU candidates of a node and keeps the fastest one that the GPU
//! [`Policy`] allows (design doc 9.10).
//!
//! Nodes in series can be fused (design doc 9.7): when a node's outputs go
//! only to the next node, channel for channel, [`Graph::prepare`] asks the
//! first to absorb the second ([`Node::fuse`]). GPU nodes use this to keep
//! signals on the GPU, with one round trip per block for the whole run.
//! Ids of absorbed nodes keep working with [`Graph::node_mut`].
//!
//! This crate knows nothing about GPUs; GPU nodes live in `ferritron-gpu`.
//!
//! ```
//! use ferritron_graph::{Gain, Graph};
//! let mut graph = Graph::new(1, 1);
//! let half = graph.add(Gain::new(1, 0.5));
//! graph.connect(Graph::INPUT, 0, half, 0).unwrap();
//! graph.connect(half, 0, Graph::OUTPUT, 0).unwrap();
//! graph.connect(Graph::INPUT, 0, Graph::OUTPUT, 0).unwrap(); // dry, summed
//! graph.prepare(64).unwrap();
//! let input = [1.0f32; 64];
//! let mut output = [0.0f32; 64];
//! graph.process(&[&input], &mut [&mut output]);
//! assert_eq!(output[0], 1.5);
//! ```

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

/// Most input or output channels a node may have.
pub const MAX_PORTS: usize = 16;

/// How freely GPU work may be used (design doc 9.10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// CPU only.
    Off,
    /// Use the GPU only where it adds no latency.
    #[default]
    Balanced,
    /// Use the GPU wherever it is faster, accepting latency.
    Max,
}

impl Policy {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Policy::Off),
            "balanced" => Some(Policy::Balanced),
            "max" => Some(Policy::Max),
            _ => None,
        }
    }

    /// The policy to use: the user's `FERRITRON_GPU_POLICY` (off, balanced
    /// or max) if set, else the plugin's `default`.
    pub fn resolve(default: Self) -> Self {
        std::env::var("FERRITRON_GPU_POLICY").ok().and_then(|v| Self::parse(&v)).unwrap_or(default)
    }
}

/// What nodes are prepared for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeSetup {
    pub sample_rate: f64,
    pub max_block: usize,
    /// The host renders offline: nodes may wait for slow resources.
    pub offline: bool,
    pub policy: Policy,
}

/// Where a node runs. Non-exhaustive: a remote machine is planned as a
/// third device (design doc 15.2, Phase 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Device {
    Cpu,
    Gpu,
}

/// One processing step of a graph.
///
/// `process` runs on the audio thread: it must not allocate, lock or block
/// (except offline). Slices have one entry per channel, all of the block's
/// length.
pub trait Node: AsAny + Send + 'static {
    fn inputs(&self) -> usize;
    fn outputs(&self) -> usize;
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]);
    /// Delay this node adds, in samples. Must not change after `prepare`.
    fn latency(&self) -> u32 {
        0
    }
    /// Clears internal state.
    fn reset(&mut self) {}
    /// Blocks whose work arrived too late (and was skipped).
    fn missed_blocks(&self) -> u64 {
        0
    }
    /// Fusion: a node that can merge with `next` returns the merged node.
    /// The graph asks only when `next` consumes exactly this node's outputs
    /// (output `i` to input `i`, nothing else in between), during `prepare`.
    /// The merged node has this node's members first, then those of `next`.
    fn fuse(&self, next: &dyn Node) -> Option<Box<dyn Node>> {
        let _ = next;
        None
    }
    /// Nodes merged into this one (1 if none).
    fn members(&self) -> usize {
        1
    }
    /// Called by [`Graph::node_mut`] before it hands out this node on
    /// behalf of member `member`, so member-specific settings reach the
    /// right part.
    fn select_member(&mut self, member: usize) {
        let _ = member;
    }
    /// Where the node runs, for a node that picks its device (CPU or GPU).
    /// `None` for nodes with no choice.
    fn placement(&self) -> Option<Device> {
        None
    }
    /// Builds the node again on `device`, keeping its settings. Off the audio
    /// thread. Returns false if the node cannot run there (the default: it
    /// has no choice to make).
    fn set_placement(&mut self, device: Device) -> bool {
        let _ = device;
        false
    }
    /// The GPU this node runs on is lost. [`Graph::process`] reads it at the
    /// start of every control period, so it must be a flag read, not a call
    /// to the device.
    fn lost(&self) -> bool {
        false
    }
    /// Switches a lost GPU node to its CPU candidate, which the node kept
    /// since it was built, and keeps the GPU part for [`revive`](Self::revive).
    /// Only moves values that already exist (no allocation, no device call),
    /// so it is real-time safe. Returns false when the node is not lost or
    /// has no CPU candidate.
    fn fall_back(&mut self) -> bool {
        false
    }
    /// Called by [`Graph::prepare`], off the audio thread: builds a lost GPU
    /// part again and goes back to the GPU when that works. Nodes without a
    /// CPU candidate need nothing.
    fn revive(&mut self) {}
}

/// Lets [`Graph::node_mut`] hand nodes back with their concrete type.
pub trait AsAny {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// A node in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// The connections form a loop (feedback needs a node that delays).
    Cycle,
    /// A connection names a channel the node does not have.
    NoSuchPort { node: NodeId, port: usize },
    /// A node has more than [`MAX_PORTS`] channels.
    TooManyPorts(NodeId),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::Cycle => write!(f, "the graph has a cycle"),
            GraphError::NoSuchPort { node, port } => write!(f, "node {} has no channel {port}", node.0),
            GraphError::TooManyPorts(node) => write!(f, "node {} has more than {MAX_PORTS} channels", node.0),
        }
    }
}

impl std::error::Error for GraphError {}

struct Edge {
    from: usize,
    from_port: usize,
    to: usize,
    to_port: usize,
    /// Compensation delay in samples, through `ring` (of delay + max_block).
    delay: usize,
    ring: Vec<f32>,
    pos: usize,
}

/// Nodes and their connections. Build it, [`prepare`](Self::prepare) it
/// (off the audio thread), then [`process`](Self::process) every block.
pub struct Graph {
    /// `None` for the graph's own input and output.
    nodes: Vec<Option<Box<dyn Node>>>,
    /// (inputs, outputs) per node.
    ports: Vec<(usize, usize)>,
    edges: Vec<Edge>,
    order: Vec<usize>,
    /// Edges into each node.
    incoming: Vec<Vec<usize>>,
    ins: Vec<Vec<Vec<f32>>>,
    outs: Vec<Vec<Vec<f32>>>,
    /// For a node absorbed by fusion: the node it lives in now, and its
    /// member index there.
    alias: Vec<Option<(usize, usize)>>,
    fusion: bool,
    fused: usize,
    latency: u32,
    max_block: usize,
    prepared: bool,
    /// A lost node switched to a CPU candidate with another latency.
    latency_changed: bool,
}

impl Graph {
    /// The graph's input: a node whose outputs are the input channels.
    pub const INPUT: NodeId = NodeId(0);
    /// The graph's output: a node whose inputs are the output channels.
    pub const OUTPUT: NodeId = NodeId(1);

    pub fn new(inputs: usize, outputs: usize) -> Self {
        Self {
            nodes: vec![None, None],
            ports: vec![(0, inputs), (outputs, 0)],
            edges: Vec::new(),
            order: Vec::new(),
            incoming: Vec::new(),
            ins: Vec::new(),
            outs: Vec::new(),
            alias: vec![None, None],
            fusion: true,
            fused: 0,
            latency: 0,
            max_block: 0,
            prepared: false,
            latency_changed: false,
        }
    }

    pub fn add(&mut self, node: impl Node) -> NodeId {
        self.ports.push((node.inputs(), node.outputs()));
        self.nodes.push(Some(Box::new(node)));
        self.alias.push(None);
        self.prepared = false;
        NodeId(self.nodes.len() - 1)
    }

    /// Feeds output channel `from_port` of `from` into input channel
    /// `to_port` of `to`. Several connections into one input are summed.
    pub fn connect(&mut self, from: NodeId, from_port: usize, to: NodeId, to_port: usize) -> Result<(), GraphError> {
        let (from, to) = (self.resolve(from), self.resolve(to));
        let ports = |id: NodeId| self.ports.get(id.0).copied();
        match ports(from) {
            Some((_, outs)) if from_port < outs => {}
            _ => return Err(GraphError::NoSuchPort { node: from, port: from_port }),
        }
        match ports(to) {
            Some((ins, _)) if to_port < ins => {}
            _ => return Err(GraphError::NoSuchPort { node: to, port: to_port }),
        }
        self.edges.push(Edge { from: from.0, from_port, to: to.0, to_port, delay: 0, ring: Vec::new(), pos: 0 });
        self.prepared = false;
        Ok(())
    }

    /// Connects channel `i` of `from` to channel `i` of `to`, for every
    /// channel both have.
    pub fn connect_all(&mut self, from: NodeId, to: NodeId) -> Result<(), GraphError> {
        let (from, to) = (self.resolve(from), self.resolve(to));
        let channels = self.ports.get(from.0).map_or(0, |p| p.1).min(self.ports.get(to.0).map_or(0, |p| p.0));
        (0..channels).try_for_each(|c| self.connect(from, c, to, c))
    }

    /// Orders the nodes, works out latency compensation and allocates
    /// buffers for blocks of up to `max_block` frames. Call again after
    /// changing the graph.
    pub fn prepare(&mut self, max_block: usize) -> Result<(), GraphError> {
        if self.fusion {
            while self.fuse_one() {}
        }
        // Before the latencies are read: a node may go back to its GPU.
        for node in self.nodes.iter_mut().flatten() {
            node.revive();
        }
        let count = self.nodes.len();
        if let Some(i) = self.ports.iter().position(|&(i, o)| i > MAX_PORTS || o > MAX_PORTS) {
            return Err(GraphError::TooManyPorts(NodeId(i)));
        }
        // Kahn's algorithm.
        let mut pending: Vec<usize> = vec![0; count];
        let mut incoming = vec![Vec::new(); count];
        for (e, edge) in self.edges.iter().enumerate() {
            pending[edge.to] += 1;
            incoming[edge.to].push(e);
        }
        let mut order: Vec<usize> = (0..count).filter(|&v| pending[v] == 0).collect();
        let mut next = 0;
        while next < order.len() {
            let v = order[next];
            next += 1;
            for edge in self.edges.iter().filter(|e| e.from == v) {
                pending[edge.to] -= 1;
                if pending[edge.to] == 0 {
                    order.push(edge.to);
                }
            }
        }
        if order.len() != count {
            return Err(GraphError::Cycle);
        }

        // Latency: when a node's input arrives, and when its output leaves.
        let mut arrive = vec![0u64; count];
        let mut leave = vec![0u64; count];
        for &v in &order {
            arrive[v] = incoming[v].iter().map(|&e| leave[self.edges[e].from]).max().unwrap_or(0);
            leave[v] = arrive[v] + self.nodes[v].as_ref().map_or(0, |n| n.latency() as u64);
        }
        for edge in &mut self.edges {
            edge.delay = (arrive[edge.to] - leave[edge.from]) as usize;
            edge.ring = if edge.delay > 0 { vec![0.0; edge.delay + max_block] } else { Vec::new() };
            edge.pos = 0;
        }

        self.ins = self.ports.iter().map(|&(i, _)| vec![vec![0.0; max_block]; i]).collect();
        self.outs = self.ports.iter().map(|&(_, o)| vec![vec![0.0; max_block]; o]).collect();
        self.latency = arrive[Self::OUTPUT.0] as u32;
        self.order = order;
        self.incoming = incoming;
        self.max_block = max_block;
        self.prepared = true;
        self.latency_changed = false;
        Ok(())
    }

    /// Turns fusion off (or on again) for the next [`prepare`](Self::prepare).
    pub fn set_fusion(&mut self, on: bool) {
        self.fusion = on;
    }

    /// Nodes absorbed into others by fusion so far.
    pub fn fused_nodes(&self) -> usize {
        self.fused
    }

    /// Where a node lives now: itself, or the node that absorbed it.
    fn resolve(&self, id: NodeId) -> NodeId {
        match self.alias.get(id.0) {
            Some(Some((host, _))) => NodeId(*host),
            _ => id,
        }
    }

    /// Finds one pair of nodes in series that can fuse, and fuses it.
    fn fuse_one(&mut self) -> bool {
        for a in 2..self.nodes.len() {
            let Some(first) = self.nodes[a].as_deref() else { continue };
            let out: Vec<&Edge> = self.edges.iter().filter(|e| e.from == a).collect();
            let Some(b) = out.first().map(|e| e.to) else { continue };
            let Some(second) = self.nodes.get(b).and_then(|n| n.as_deref()) else { continue };
            let channels = self.ports[a].1;
            // Exactly output i to input i for every channel, and nothing
            // else leaving `a` or entering `b`.
            let into_b = self.edges.iter().filter(|e| e.to == b).count();
            let paired = (0..channels).all(|i| out.iter().filter(|e| e.to == b && e.from_port == i && e.to_port == i).count() == 1);
            if b == a || out.len() != channels || into_b != channels || self.ports[b].0 != channels || !paired {
                continue;
            }
            let Some(merged) = first.fuse(second) else { continue };
            let offset = first.members();
            self.ports[a] = (merged.inputs(), merged.outputs());
            self.ports[b] = (0, 0);
            self.nodes[a] = Some(merged);
            self.nodes[b] = None;
            self.edges.retain(|e| !(e.from == a && e.to == b));
            for edge in self.edges.iter_mut().filter(|e| e.from == b) {
                edge.from = a;
            }
            for alias in self.alias.iter_mut().flatten().filter(|(host, _)| *host == b) {
                *alias = (a, offset + alias.1);
            }
            self.alias[b] = Some((a, offset));
            self.fused += 1;
            return true;
        }
        false
    }

    /// Total delay from input to output, including compensation.
    pub fn latency(&self) -> u32 {
        self.latency
    }

    /// Blocks some node skipped work in, summed over nodes.
    pub fn missed_blocks(&self) -> u64 {
        self.nodes.iter().flatten().map(|n| n.missed_blocks()).sum()
    }

    /// Where node `id` runs (the node that absorbed it, if fused). `None` for
    /// a node with no choice of device.
    pub fn placement(&self, id: NodeId) -> Option<Device> {
        let host = self.resolve(id);
        self.nodes.get(host.0)?.as_deref()?.placement()
    }

    /// Fixes where node `id` runs, off the audio thread: the node is built
    /// again on `device` ([`Node::set_placement`]), and the graph is prepared
    /// again when it was prepared, since the latency may change. Returns
    /// `Ok(false)` if the node cannot run on `device` (it stays as it was).
    /// Save [`placement`](Self::placement) with the project to restore it.
    pub fn set_placement(&mut self, id: NodeId, device: Device) -> Result<bool, GraphError> {
        let host = self.resolve(id);
        let Some(node) = self.nodes.get_mut(host.0).and_then(|n| n.as_deref_mut()) else { return Ok(false) };
        if !node.set_placement(device) {
            return Ok(false);
        }
        if self.prepared {
            let max_block = self.max_block;
            self.prepare(max_block)?;
        }
        Ok(true)
    }

    /// True once after a lost GPU node switched to a CPU candidate with another
    /// latency. The graph keeps its compensation until the next
    /// [`prepare`](Self::prepare), so the plugin should report the new
    /// latency to the host (`request_latency`) and prepare again.
    pub fn take_latency_changed(&mut self) -> bool {
        std::mem::take(&mut self.latency_changed)
    }

    /// At the start of a control period: a lost GPU node runs its CPU
    /// candidate from now on. The candidate's state starts empty, so filter
    /// and delay contents are not carried over (a short gap in the sound).
    fn switch_lost_nodes(&mut self) {
        for node in self.nodes.iter_mut().flatten() {
            if node.lost() {
                let before = node.latency();
                if node.fall_back() && node.latency() != before {
                    self.latency_changed = true;
                }
            }
        }
    }

    /// A node with its concrete type, to change its settings.
    /// A node absorbed by fusion is found in the node that absorbed it,
    /// with its own part selected ([`Node::select_member`]).
    pub fn node_mut<T: Node>(&mut self, id: NodeId) -> Option<&mut T> {
        let (slot, member) = self.alias.get(id.0).copied().flatten().unwrap_or((id.0, 0));
        let node = self.nodes.get_mut(slot)?.as_deref_mut()?;
        node.select_member(member);
        node.as_any_mut().downcast_mut::<T>()
    }

    /// Clears the state of every node and of the compensation delays.
    pub fn reset(&mut self) {
        self.nodes.iter_mut().flatten().for_each(|n| n.reset());
        for edge in &mut self.edges {
            edge.ring.fill(0.0);
        }
    }

    /// Processes one block. Real-time safe. Without [`prepare`](Self::prepare),
    /// outputs silence. A block longer than the `max_block` given to
    /// `prepare` is processed in pieces of that size, so every frame is
    /// processed.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let total = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0);
        if !self.prepared || self.max_block == 0 {
            outputs.iter_mut().for_each(|c| c.fill(0.0));
            return;
        }
        self.switch_lost_nodes();
        let mut at = 0;
        while at < total {
            let n = (total - at).min(self.max_block);
            self.run_piece(inputs, outputs, at, n);
            at += n;
        }
    }

    /// Runs the nodes on frames `at..at + n` (at most `max_block` of them).
    fn run_piece(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], at: usize, n: usize) {
        let span = at..at + n;
        let Graph { nodes, ports, edges, order, incoming, ins, outs, .. } = self;
        for (c, buf) in outs[Self::INPUT.0].iter_mut().enumerate() {
            match inputs.get(c) {
                Some(input) => buf[..n].copy_from_slice(&input[span.clone()]),
                None => buf[..n].fill(0.0),
            }
        }
        for &v in order.iter().filter(|&&v| v != Self::INPUT.0) {
            for buf in &mut ins[v] {
                buf[..n].fill(0.0);
            }
            for &e in &incoming[v] {
                let edge = &mut edges[e];
                let src = &outs[edge.from][edge.from_port][..n];
                let dst = &mut ins[v][edge.to_port][..n];
                if edge.delay == 0 {
                    dst.iter_mut().zip(src).for_each(|(d, s)| *d += s);
                } else {
                    let len = edge.ring.len();
                    for (i, (d, &s)) in dst.iter_mut().zip(src).enumerate() {
                        let at = (edge.pos + i) % len;
                        edge.ring[at] = s;
                        *d += edge.ring[(at + len - edge.delay) % len];
                    }
                    edge.pos = (edge.pos + n) % len;
                }
            }
            if v == Self::OUTPUT.0 {
                for (out, buf) in outputs.iter_mut().zip(&ins[v]) {
                    out[span.clone()].copy_from_slice(&buf[..n]);
                }
                for out in outputs.iter_mut().skip(ins[v].len()) {
                    out[span.clone()].fill(0.0);
                }
                continue;
            }
            let Some(node) = nodes[v].as_mut() else { continue };
            let (ni, no) = ports[v];
            let node_ins = &ins[v];
            let in_refs: [&[f32]; MAX_PORTS] = std::array::from_fn(|c| if c < ni { &node_ins[c][..n] } else { &[] });
            let mut it = outs[v].iter_mut();
            let mut out_refs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| match it.next() {
                Some(buf) => &mut buf[..n],
                None => Default::default(),
            });
            node.process(&in_refs[..ni], &mut out_refs[..no]);
        }
    }
}

/// A reusable piece of a graph: what one Ferritron module contributes to a
/// chain ("Ferritron Chain", design doc 9.7). Modules built into one graph
/// with [`Graph::chain`] share its fusion, so neighbouring GPU nodes of
/// different modules run in one GPU round trip.
pub trait Module {
    /// Adds the module's nodes to `graph`; returns the node that takes the
    /// module's input and the node that gives its output (they may be the
    /// same).
    fn build(&mut self, graph: &mut Graph, setup: &NodeSetup) -> Result<(NodeId, NodeId), GraphError>;
}

impl Graph {
    /// Modules in series, `channels` wide: input, each module, output,
    /// prepared (and fused). Returns the graph and each module's (input,
    /// output) nodes, for changing settings later.
    pub fn chain(channels: usize, modules: &mut [&mut dyn Module], setup: &NodeSetup) -> Result<(Graph, Vec<(NodeId, NodeId)>), GraphError> {
        let mut graph = Graph::new(channels, channels);
        let mut previous = Graph::INPUT;
        let mut ends = Vec::with_capacity(modules.len());
        for module in modules.iter_mut() {
            let (first, last) = module.build(&mut graph, setup)?;
            graph.connect_all(previous, first)?;
            previous = last;
            ends.push((first, last));
        }
        graph.connect_all(previous, Graph::OUTPUT)?;
        graph.prepare(setup.max_block)?;
        Ok((graph, ends))
    }
}

/// How a candidate of [`choose`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct Measured {
    pub device: Device,
    /// Time of one `process` call on the calling thread (the audio thread's cost).
    pub micros: f64,
    pub latency: u32,
    /// The policy allows this candidate.
    pub allowed: bool,
    pub chosen: bool,
}

/// Automatic placement: measures each candidate on silence and returns the
/// fastest one the policy allows, with the measurements. `Off` allows only
/// CPU candidates; `Balanced` also GPU candidates without latency; `Max`
/// all, but latency is only worth it for real savings: a candidate that adds
/// latency wins only if the best one without latency would take more than
/// an eighth of the block's real-time budget. If the policy allows none,
/// the first candidate is used. Measured candidates are reset afterwards.
pub fn choose<T: Node>(candidates: Vec<(Device, T)>, setup: &NodeSetup) -> Option<(T, Vec<Measured>)> {
    choose_keyed(None, None, candidates, setup)
}

/// Whether `policy` allows a candidate on `device` with `latency`.
fn allowed_by(policy: Policy, device: Device, latency: u32) -> bool {
    match policy {
        Policy::Off => device == Device::Cpu,
        Policy::Balanced => device == Device::Cpu || latency == 0,
        Policy::Max => true,
    }
}

/// Placement memory, by configuration key (see [`config_key`]): the device
/// each configuration was placed on in this process.
static REMEMBERED: Mutex<BTreeMap<u64, Device>> = Mutex::new(BTreeMap::new());

/// A stable key for a node's configuration (its sizes, kernels, settings),
/// from numbers the caller picks: the same configuration gives the same key
/// in every process, so [`choose_keyed`] can reuse a placement.
pub fn config_key(parts: &[u64]) -> u64 {
    parts.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &p| (h ^ p).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The device a configuration was last placed on in this process, if any.
pub fn remembered_placement(key: u64) -> Option<Device> {
    REMEMBERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&key).copied()
}

/// The device a placement result chose (from [`choose`] or [`choose_keyed`]).
pub fn placed_on(measured: &[Measured]) -> Option<Device> {
    measured.iter().find(|m| m.chosen).map(|m| m.device)
}

/// [`choose`] with a fixed placement and placement memory.
///
/// - `fixed`: this device is used if the policy allows it, without
///   measuring (the "set placement" of a saved project: opening it again
///   takes the same path it took before).
/// - `key`: when no `fixed` is given and this configuration was placed
///   before in this process, that placement is used; otherwise the candidates
///   are measured, and the result is remembered under `key`.
///
/// Measurements of fixed placements are infinite (not measured). If the
/// fixed device has no allowed candidate, the candidates are measured.
pub fn choose_keyed<T: Node>(key: Option<u64>, fixed: Option<Device>, candidates: Vec<(Device, T)>, setup: &NodeSetup) -> Option<(T, Vec<Measured>)> {
    let wanted = fixed.or_else(|| key.and_then(remembered_placement));
    if let Some(device) = wanted {
        let pick = candidates.iter().position(|(d, n)| *d == device && allowed_by(setup.policy, *d, n.latency()));
        if let Some(pick) = pick {
            let results = candidates
                .iter()
                .enumerate()
                .map(|(i, (d, n))| Measured { device: *d, micros: f64::INFINITY, latency: n.latency(), allowed: allowed_by(setup.policy, *d, n.latency()), chosen: i == pick })
                .collect();
            let node = candidates.into_iter().nth(pick)?.1;
            return Some((node, results));
        }
    }
    measure_and_choose(key, candidates, setup)
}

fn measure_and_choose<T: Node>(key: Option<u64>, candidates: Vec<(Device, T)>, setup: &NodeSetup) -> Option<(T, Vec<Measured>)> {
    let block = setup.max_block.max(1);
    let mut results = Vec::with_capacity(candidates.len());
    let mut nodes = Vec::with_capacity(candidates.len());
    for (device, mut node) in candidates {
        let latency = node.latency();
        let allowed = allowed_by(setup.policy, device, latency);
        let mut micros = f64::INFINITY;
        if allowed {
            let ins = vec![vec![0.0f32; block]; node.inputs()];
            let mut outs = vec![vec![0.0f32; block]; node.outputs()];
            let in_refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
            // Few blocks: real-time GPU candidates queue them without pacing.
            for round in 0..10 {
                let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|c| &mut c[..]).collect();
                let start = Instant::now();
                node.process(&in_refs, &mut out_refs);
                let took = start.elapsed().as_secs_f64() * 1e6;
                if round >= 2 {
                    micros = micros.min(took);
                }
            }
            node.reset();
        }
        results.push(Measured { device, micros, latency, allowed, chosen: false });
        nodes.push(node);
    }
    let fastest = |latency_ok: bool| {
        (0..results.len())
            .filter(|&i| results[i].allowed && (latency_ok || results[i].latency == 0))
            .min_by(|&a, &b| results[a].micros.total_cmp(&results[b].micros))
    };
    let budget = block as f64 / setup.sample_rate * 1e6;
    let best = match (fastest(true), fastest(false)) {
        (Some(any), Some(zero)) if results[any].latency > 0 && results[zero].micros <= budget / 8.0 => zero,
        (Some(any), _) => any,
        (None, _) => 0,
    };
    results.get_mut(best)?.chosen = true;
    if let Some(key) = key {
        REMEMBERED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key, results[best].device);
    }
    let node = nodes.into_iter().nth(best)?;
    Some((node, results))
}

/// Runs a node on batches of `batch` frames, however small the host's
/// blocks are (design doc 9.9: offline, beyond the host's block length).
/// Input is gathered until a batch is full and then processed at once, so a
/// GPU node pays its round trip once per batch instead of once per block;
/// the output is the inner node's, `batch` frames later (reported as
/// latency, so hosts compensate it).
///
/// Meant for offline rendering: create the inner node for blocks of `batch`
/// frames (`NodeSetup::max_block`), and wrap it. In real time the latency
/// is usually too high.
pub struct Batched<N: Node> {
    inner: N,
    batch: usize,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// Frames gathered in the current batch.
    fill: usize,
}

impl<N: Node> Batched<N> {
    /// `inner` must accept blocks of `batch` frames.
    pub fn new(inner: N, batch: usize) -> Self {
        let batch = batch.max(1);
        let input = vec![vec![0.0; batch]; inner.inputs()];
        let output = vec![vec![0.0; batch]; inner.outputs()];
        Self { inner, batch, input, output, fill: 0 }
    }

    pub fn inner(&self) -> &N {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut N {
        &mut self.inner
    }

    pub fn batch(&self) -> usize {
        self.batch
    }
}

impl<N: Node> Node for Batched<N> {
    fn inputs(&self) -> usize {
        self.inner.inputs()
    }

    fn outputs(&self) -> usize {
        self.inner.outputs()
    }

    fn latency(&self) -> u32 {
        self.inner.latency() + self.batch as u32
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.input.iter_mut().chain(&mut self.output).for_each(|c| c.fill(0.0));
        self.fill = 0;
    }

    fn missed_blocks(&self) -> u64 {
        self.inner.missed_blocks()
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0);
        let mut done = 0;
        while done < n {
            let take = (self.batch - self.fill).min(n - done);
            for (gathered, input) in self.input.iter_mut().zip(inputs) {
                gathered[self.fill..self.fill + take].copy_from_slice(&input[done..done + take]);
            }
            // The previous batch's results, frame for frame.
            for (out, ready) in outputs.iter_mut().zip(&self.output) {
                out[done..done + take].copy_from_slice(&ready[self.fill..self.fill + take]);
            }
            self.fill += take;
            done += take;
            if self.fill == self.batch {
                let ins: Vec<&[f32]> = self.input.iter().map(|c| &c[..]).collect();
                let mut outs: Vec<&mut [f32]> = self.output.iter_mut().map(|c| &mut c[..]).collect();
                self.inner.process(&ins, &mut outs);
                self.fill = 0;
            }
        }
    }
}

/// Multiplies every channel by a gain: the simplest node, for mixing.
/// Gains in series fuse into one multiplication.
pub struct Gain {
    channels: usize,
    /// One factor per member (fused gains), multiplied together.
    factors: Vec<f32>,
    gain: f32,
    active: usize,
}

impl Gain {
    pub fn new(channels: usize, gain: f32) -> Self {
        Self { channels, factors: vec![gain], gain, active: 0 }
    }

    /// Sets this gain (for a fused gain: the selected member's factor).
    pub fn set(&mut self, gain: f32) {
        self.factors[self.active] = gain;
        self.gain = self.factors.iter().product();
    }
}

impl Node for Gain {
    fn inputs(&self) -> usize {
        self.channels
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for (out, input) in outputs.iter_mut().zip(inputs) {
            out.iter_mut().zip(input.iter()).for_each(|(o, &i)| *o = i * self.gain);
        }
    }

    fn fuse(&self, next: &dyn Node) -> Option<Box<dyn Node>> {
        let next = next.as_any().downcast_ref::<Gain>()?;
        let factors: Vec<f32> = self.factors.iter().chain(&next.factors).copied().collect();
        Some(Box::new(Gain { channels: self.channels, gain: factors.iter().product(), factors, active: 0 }))
    }

    fn members(&self) -> usize {
        self.factors.len()
    }

    fn select_member(&mut self, member: usize) {
        self.active = member.min(self.factors.len() - 1);
    }
}

/// A fixed delay of whole samples, which it reports as latency (for
/// look-ahead, or to test latency compensation).
pub struct Delay {
    ring: Vec<Vec<f32>>,
    delay: usize,
    pos: usize,
}

impl Delay {
    pub fn new(channels: usize, delay: usize, max_block: usize) -> Self {
        Self { ring: vec![vec![0.0; delay + max_block.max(1)]; channels], delay, pos: 0 }
    }
}

impl Node for Delay {
    fn inputs(&self) -> usize {
        self.ring.len()
    }

    fn outputs(&self) -> usize {
        self.ring.len()
    }

    fn latency(&self) -> u32 {
        self.delay as u32
    }

    fn reset(&mut self) {
        self.ring.iter_mut().for_each(|r| r.fill(0.0));
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let n = inputs.first().map_or(0, |c| c.len());
        for ((ring, input), out) in self.ring.iter_mut().zip(inputs).zip(outputs.iter_mut()) {
            let len = ring.len();
            for i in 0..n {
                let at = (self.pos + i) % len;
                ring[at] = input[i];
                out[i] = ring[(at + len - self.delay) % len];
            }
        }
        self.pos = (self.pos + n) % self.ring.first().map_or(1, |r| r.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batched_node_is_the_node_delayed_by_its_batch() {
        // A node whose output depends on the block it is given: a running sum.
        struct Sum(f32);
        impl Node for Sum {
            fn inputs(&self) -> usize {
                1
            }
            fn outputs(&self) -> usize {
                1
            }
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
                for (o, &x) in outputs[0].iter_mut().zip(inputs[0]) {
                    self.0 += x;
                    *o = self.0;
                }
            }
        }
        let input: Vec<f32> = (0..1000).map(|i| (i % 7) as f32 - 3.0).collect();
        let mut want = vec![0.0f32; 1000];
        Sum(0.0).process(&[&input], &mut [&mut want]);
        let mut batched = Batched::new(Sum(0.0), 96);
        assert_eq!(batched.latency(), 96);
        let mut got = Vec::new();
        // Host blocks of odd sizes, smaller and larger than the batch.
        for chunk in input.chunks(37).chain([&[0.0f32; 200][..]]) {
            let mut out = vec![0.0f32; chunk.len()];
            batched.process(&[chunk], &mut [&mut out]);
            got.extend(out);
        }
        assert!(got[..96].iter().all(|&v| v == 0.0));
        assert_eq!(&got[96..96 + 900], &want[..900]);
    }

    fn run(graph: &mut Graph, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(block) {
            let mut buf = vec![0.0f32; chunk.len()];
            graph.process(&[chunk], &mut [&mut buf]);
            out.extend_from_slice(&buf);
        }
        out
    }

    #[test]
    fn parallel_paths_are_aligned_by_latency_compensation() {
        // Input → 100-sample delay → output, plus input → output directly.
        // Without compensation an impulse would come out twice; with it,
        // once, at 100 samples, with both paths summed.
        let mut g = Graph::new(1, 1);
        let delay = g.add(Delay::new(1, 100, 64));
        g.connect(Graph::INPUT, 0, delay, 0).unwrap();
        g.connect(delay, 0, Graph::OUTPUT, 0).unwrap();
        g.connect(Graph::INPUT, 0, Graph::OUTPUT, 0).unwrap();
        g.prepare(64).unwrap();
        assert_eq!(g.latency(), 100);
        let mut input = vec![0.0; 400];
        input[0] = 1.0;
        let out = run(&mut g, &input, 37);
        let hits: Vec<(usize, f32)> = out.iter().copied().enumerate().filter(|&(_, v)| v != 0.0).collect();
        assert_eq!(hits, vec![(100, 2.0)]);
    }

    #[test]
    fn latency_adds_up_along_a_chain_and_takes_the_longest_path() {
        let mut g = Graph::new(1, 1);
        let a = g.add(Delay::new(1, 10, 32));
        let b = g.add(Delay::new(1, 20, 32));
        let c = g.add(Delay::new(1, 5, 32));
        g.connect(Graph::INPUT, 0, a, 0).unwrap();
        g.connect(a, 0, b, 0).unwrap(); // 30 on this path
        g.connect(Graph::INPUT, 0, c, 0).unwrap(); // 5 on this one
        g.connect(b, 0, Graph::OUTPUT, 0).unwrap();
        g.connect(c, 0, Graph::OUTPUT, 0).unwrap();
        g.prepare(32).unwrap();
        assert_eq!(g.latency(), 30);
        let mut input = vec![0.0; 200];
        input[3] = 1.0;
        let out = run(&mut g, &input, 32);
        assert_eq!(out[33], 2.0);
        assert_eq!(out.iter().filter(|&&v| v != 0.0).count(), 1);
    }

    #[test]
    fn cycles_and_bad_ports_are_rejected() {
        let mut g = Graph::new(1, 1);
        let a = g.add(Gain::new(1, 1.0));
        let b = g.add(Gain::new(1, 1.0));
        assert_eq!(g.connect(a, 1, b, 0), Err(GraphError::NoSuchPort { node: a, port: 1 }));
        g.connect(a, 0, b, 0).unwrap();
        g.connect(b, 0, a, 0).unwrap();
        assert_eq!(g.prepare(16), Err(GraphError::Cycle));
    }

    #[test]
    fn nodes_can_be_changed_while_running() {
        let mut g = Graph::new(1, 1);
        let gain = g.add(Gain::new(1, 1.0));
        g.connect_all(Graph::INPUT, gain).unwrap();
        g.connect_all(gain, Graph::OUTPUT).unwrap();
        g.prepare(8).unwrap();
        g.node_mut::<Gain>(gain).unwrap().set(0.25);
        assert!(g.node_mut::<Delay>(gain).is_none(), "wrong type");
        let out = run(&mut g, &[2.0; 8], 8);
        assert_eq!(out, vec![0.5; 8]);
    }

    /// A node that takes `cost` microseconds per block.
    struct Busy {
        cost: u64,
        latency: u32,
        tag: u32,
    }

    impl Node for Busy {
        fn inputs(&self) -> usize {
            1
        }
        fn outputs(&self) -> usize {
            1
        }
        fn latency(&self) -> u32 {
            self.latency
        }
        fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]]) {
            let start = Instant::now();
            while start.elapsed().as_micros() < self.cost as u128 {}
            outputs[0].fill(self.tag as f32);
        }
    }

    fn setup(policy: Policy) -> NodeSetup {
        NodeSetup { sample_rate: 48_000.0, max_block: 64, offline: false, policy }
    }

    #[test]
    fn placement_picks_the_fastest_candidate_the_policy_allows() {
        let candidates = || {
            vec![(Device::Cpu, Busy { cost: 300, latency: 0, tag: 1 }), (Device::Gpu, Busy { cost: 20, latency: 128, tag: 2 })]
        };
        let tag = |policy| choose(candidates(), &setup(policy)).unwrap().0.tag;
        assert_eq!(tag(Policy::Max), 2, "faster, latency accepted");
        assert_eq!(tag(Policy::Balanced), 1, "GPU candidate adds latency");
        assert_eq!(tag(Policy::Off), 1);

        // 64 samples at 48 kHz: 1,333 us per block. A CPU that needs 50 us
        // is cheap enough: no latency for a GPU that would save 40 us.
        let cheap_cpu = vec![(Device::Cpu, Busy { cost: 50, latency: 0, tag: 1 }), (Device::Gpu, Busy { cost: 10, latency: 128, tag: 2 })];
        assert_eq!(choose(cheap_cpu, &setup(Policy::Max)).unwrap().0.tag, 1, "latency must pay off");

        let fast_cpu = vec![(Device::Cpu, Busy { cost: 5, latency: 0, tag: 1 }), (Device::Gpu, Busy { cost: 400, latency: 0, tag: 2 })];
        let (node, measured) = choose(fast_cpu, &setup(Policy::Max)).unwrap();
        assert_eq!(node.tag, 1, "the CPU wins when it is faster");
        assert!(measured[0].chosen && !measured[1].chosen && measured[1].micros > measured[0].micros);
    }

    /// G4: a placement can be fixed, and a configuration keeps the placement it got
    /// (without measuring again), as long as the policy allows it.
    #[test]
    fn a_fixed_or_remembered_placement_is_used_without_measuring() {
        let candidates = || vec![(Device::Cpu, Busy { cost: 5, latency: 0, tag: 1 }), (Device::Gpu, Busy { cost: 400, latency: 0, tag: 2 })];
        // Fixed: the GPU candidate, even though the CPU is faster.
        let (node, measured) = choose_keyed(None, Some(Device::Gpu), candidates(), &setup(Policy::Max)).unwrap();
        assert_eq!(node.tag, 2);
        assert_eq!(placed_on(&measured), Some(Device::Gpu));
        assert!(measured.iter().all(|m| m.micros.is_infinite()), "nothing was measured");
        // The policy still rules: CPU only.
        let (node, _) = choose_keyed(None, Some(Device::Gpu), candidates(), &setup(Policy::Off)).unwrap();
        assert_eq!(node.tag, 1, "Off allows only the CPU");
        // Remembered: the first call measures (the CPU wins), later calls reuse it.
        let key = config_key(&[0x7e57, 1]);
        let (node, measured) = choose_keyed(Some(key), None, candidates(), &setup(Policy::Max)).unwrap();
        assert_eq!((node.tag, remembered_placement(key)), (1, Some(Device::Cpu)));
        assert!(measured[0].micros.is_finite(), "the first placement measured");
        let (node, measured) = choose_keyed(Some(key), None, candidates(), &setup(Policy::Max)).unwrap();
        assert_eq!(node.tag, 1);
        assert!(measured.iter().all(|m| m.micros.is_infinite()), "the remembered placement is reused");
    }

    /// G5: a block longer than the prepared size is processed in pieces, all of it.
    #[test]
    fn a_block_longer_than_max_block_is_processed_whole() {
        let mut g = Graph::new(1, 1);
        let half = g.add(Gain::new(1, 0.5));
        g.connect(Graph::INPUT, 0, half, 0).unwrap();
        g.connect(half, 0, Graph::OUTPUT, 0).unwrap();
        g.prepare(8).unwrap();
        let input = [2.0f32; 40];
        let mut output = [0.0f32; 40];
        g.process(&[&input], &mut [&mut output]);
        assert!(output.iter().all(|&v| v == 1.0), "frames after the first 8 were dropped: {output:?}");
    }

    #[test]
    fn gains_in_series_fuse_and_keep_their_ids() {
        let mut g = Graph::new(2, 2);
        let a = g.add(Gain::new(2, 2.0));
        let b = g.add(Gain::new(2, 3.0));
        let c = g.add(Gain::new(2, 5.0));
        g.connect_all(Graph::INPUT, a).unwrap();
        g.connect_all(a, b).unwrap();
        g.connect_all(b, c).unwrap();
        g.connect_all(c, Graph::OUTPUT).unwrap();
        g.prepare(8).unwrap();
        assert_eq!(g.fused_nodes(), 2, "a, b and c are one node now");
        let (left, right) = ([1.0f32; 8], [0.5f32; 8]);
        let (mut l, mut r) = ([0.0f32; 8], [0.0f32; 8]);
        g.process(&[&left, &right], &mut [&mut l, &mut r]);
        assert_eq!((l[0], r[0]), (30.0, 15.0));
        // Each id still sets its own factor.
        g.node_mut::<Gain>(b).unwrap().set(1.0);
        g.node_mut::<Gain>(c).unwrap().set(0.5);
        g.process(&[&left, &right], &mut [&mut l, &mut r]);
        assert_eq!(l[0], 2.0 * 1.0 * 0.5);
    }

    #[test]
    fn nodes_with_other_listeners_or_crossed_channels_do_not_fuse() {
        // a feeds both b and the output: fusing would lose a's own output.
        let mut g = Graph::new(1, 1);
        let a = g.add(Gain::new(1, 2.0));
        let b = g.add(Gain::new(1, 3.0));
        g.connect_all(Graph::INPUT, a).unwrap();
        g.connect_all(a, b).unwrap();
        g.connect_all(b, Graph::OUTPUT).unwrap();
        g.connect_all(a, Graph::OUTPUT).unwrap();
        g.prepare(4).unwrap();
        assert_eq!(g.fused_nodes(), 0);
        let mut out = [0.0f32; 4];
        g.process(&[&[1.0; 4]], &mut [&mut out]);
        assert_eq!(out[0], 2.0 + 6.0);

        // Crossed channels (0 to 1, 1 to 0) are not a series connection.
        let mut g = Graph::new(2, 2);
        let a = g.add(Gain::new(2, 1.0));
        let b = g.add(Gain::new(2, 1.0));
        g.connect_all(Graph::INPUT, a).unwrap();
        g.connect(a, 0, b, 1).unwrap();
        g.connect(a, 1, b, 0).unwrap();
        g.connect_all(b, Graph::OUTPUT).unwrap();
        g.prepare(4).unwrap();
        assert_eq!(g.fused_nodes(), 0);

        // And fusion can be turned off.
        let mut g = Graph::new(1, 1);
        let a = g.add(Gain::new(1, 1.0));
        let b = g.add(Gain::new(1, 1.0));
        g.connect_all(Graph::INPUT, a).unwrap();
        g.connect_all(a, b).unwrap();
        g.connect_all(b, Graph::OUTPUT).unwrap();
        g.set_fusion(false);
        g.prepare(4).unwrap();
        assert_eq!(g.fused_nodes(), 0);
    }

    /// A module made of one gain node.
    struct GainModule(f32);

    impl Module for GainModule {
        fn build(&mut self, graph: &mut Graph, _: &NodeSetup) -> Result<(NodeId, NodeId), GraphError> {
            let id = graph.add(Gain::new(2, self.0));
            Ok((id, id))
        }
    }

    #[test]
    fn modules_chain_and_fuse_across_their_borders() {
        let (mut a, mut b, mut c) = (GainModule(2.0), GainModule(3.0), GainModule(0.5));
        let (mut g, ends) = Graph::chain(2, &mut [&mut a, &mut b, &mut c], &setup(Policy::Balanced)).unwrap();
        assert_eq!(g.fused_nodes(), 2, "the three modules' gains became one node");
        let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
        g.process(&[&[1.0; 4], &[2.0; 4]], &mut [&mut l, &mut r]);
        assert_eq!((l[0], r[0]), (3.0, 6.0));
        // Each module's node is still reachable by its id.
        g.node_mut::<Gain>(ends[1].0).unwrap().set(1.0);
        g.process(&[&[1.0; 4], &[2.0; 4]], &mut [&mut l, &mut r]);
        assert_eq!(l[0], 1.0);
    }

    /// G4: a node's placement is reported (through the node that absorbed it,
    /// if fused); fixing it builds the node again and prepares the graph
    /// again, so the latency follows; a node without a choice refuses.
    #[test]
    fn set_placement_rebuilds_the_node_and_prepares_again() {
        struct Placed {
            device: Device,
        }
        impl Node for Placed {
            fn inputs(&self) -> usize {
                1
            }
            fn outputs(&self) -> usize {
                1
            }
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
                outputs[0].copy_from_slice(inputs[0]);
            }
            fn latency(&self) -> u32 {
                if self.device == Device::Gpu { 64 } else { 0 }
            }
            fn placement(&self) -> Option<Device> {
                Some(self.device)
            }
            fn set_placement(&mut self, device: Device) -> bool {
                self.device = device;
                true
            }
        }
        let mut graph = Graph::new(1, 1);
        let id = graph.add(Placed { device: Device::Gpu });
        graph.connect(Graph::INPUT, 0, id, 0).unwrap();
        graph.connect(id, 0, Graph::OUTPUT, 0).unwrap();
        graph.prepare(32).unwrap();
        assert_eq!((graph.placement(id), graph.latency()), (Some(Device::Gpu), 64));
        assert!(graph.set_placement(id, Device::Cpu).unwrap());
        assert_eq!((graph.placement(id), graph.latency()), (Some(Device::Cpu), 0), "prepared again: the latency follows");
        let plain = graph.add(Gain::new(1, 1.0));
        assert_eq!(graph.placement(plain), None);
        assert!(!graph.set_placement(plain, Device::Gpu).unwrap());
    }

    /// A node with a GPU side (`gain`, `latency`) and its CPU candidate kept
    /// beside it (`other`), like the GPU nodes of `ferritron-gpu`. Each side
    /// has its own `flag` (the device-lost mark). The switch swaps the sides.
    struct Sided {
        gain: f32,
        latency: u32,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        other: Option<Box<Sided>>,
        revives: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Node for Sided {
        fn inputs(&self) -> usize {
            1
        }
        fn outputs(&self) -> usize {
            1
        }
        fn latency(&self) -> u32 {
            self.latency
        }
        fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
            for (o, &x) in outputs[0].iter_mut().zip(inputs[0]) {
                *o = x * self.gain;
            }
        }
        fn lost(&self) -> bool {
            self.flag.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn fall_back(&mut self) -> bool {
            if !self.lost() {
                return false;
            }
            let Some(mut other) = self.other.take() else { return false };
            std::mem::swap(self, &mut *other);
            self.other = Some(other);
            true
        }
        fn revive(&mut self) {
            self.revives.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn sided_graph(node: Sided) -> Graph {
        let mut g = Graph::new(1, 1);
        let id = g.add(node);
        g.connect(Graph::INPUT, 0, id, 0).unwrap();
        g.connect(id, 0, Graph::OUTPUT, 0).unwrap();
        g
    }

    /// G-f7 2-1: with the GPU side's loss flag set by hand, the next control
    /// period runs the CPU candidate, and its output equals a graph with only
    /// the CPU candidate. The latency change is reported once.
    #[test]
    fn a_lost_gpu_node_runs_its_cpu_candidate_from_the_next_block() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let flag = Arc::new(AtomicBool::new(false));
        let revives = Arc::new(AtomicUsize::new(0));
        let cpu = Sided { gain: 0.5, latency: 0, flag: Arc::new(AtomicBool::new(false)), other: None, revives: revives.clone() };
        let gpu = Sided { gain: 3.0, latency: 64, flag: flag.clone(), other: Some(Box::new(cpu)), revives: revives.clone() };
        let mut g = sided_graph(gpu);
        g.prepare(64).unwrap();
        assert_eq!(revives.load(Ordering::Relaxed), 1, "prepare revives each node once");
        assert_eq!(g.latency(), 64);
        let input: Vec<f32> = (0..128).map(|i| i as f32 * 0.01 - 0.6).collect();

        let mut first = vec![0.0f32; 64];
        g.process(&[&input[..64]], &mut [&mut first]);
        assert!(first.iter().zip(&input[..64]).all(|(o, i)| *o == i * 3.0), "the GPU side runs first");

        flag.store(true, Ordering::Relaxed);
        let mut second = vec![0.0f32; 64];
        g.process(&[&input[64..]], &mut [&mut second]);

        let mut reference = Graph::new(1, 1);
        let half = reference.add(Gain::new(1, 0.5));
        reference.connect(Graph::INPUT, 0, half, 0).unwrap();
        reference.connect(half, 0, Graph::OUTPUT, 0).unwrap();
        reference.prepare(64).unwrap();
        let mut want = vec![0.0f32; 64];
        reference.process(&[&input[64..]], &mut [&mut want]);
        assert_eq!(second, want, "the CPU candidate's output, the same as a CPU-only graph");

        assert!(g.take_latency_changed(), "the latency went from 64 to 0");
        assert!(!g.take_latency_changed(), "reported once");
        assert_eq!(g.latency(), 64, "the compensation stays until the next prepare");
    }

    /// A lost node without a CPU candidate keeps running where it is.
    #[test]
    fn a_lost_node_without_a_cpu_candidate_keeps_its_gpu_side() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let flag = Arc::new(AtomicBool::new(true));
        let revives = Arc::new(AtomicUsize::new(0));
        let gpu = Sided { gain: 3.0, latency: 64, flag, other: None, revives };
        let mut g = sided_graph(gpu);
        g.prepare(16).unwrap();
        let input = [1.0f32; 16];
        let mut out = [0.0f32; 16];
        g.process(&[&input], &mut [&mut out]);
        assert!(out.iter().all(|&v| v == 3.0), "no CPU candidate: the node is not switched");
        assert!(!g.take_latency_changed());
    }

    #[test]
    fn policy_names() {
        assert_eq!(Policy::parse(" MAX "), Some(Policy::Max));
        assert_eq!(Policy::parse("balanced"), Some(Policy::Balanced));
        assert_eq!(Policy::parse("gpu"), None);
    }
}
