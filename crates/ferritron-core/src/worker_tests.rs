use super::*;
use crate::{FloatParam, ParamInfo, ParamKind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// The pool is process-wide: tests that look at it take turns.
static TURN: Mutex<()> = Mutex::new(());

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct Probe {
    steps: Arc<AtomicUsize>,
    setups: Arc<AtomicU32>,
    dropped: Arc<AtomicBool>,
    panic_on_step: bool,
    interval: Duration,
    flushed: Arc<AtomicBool>,
}

impl WorkerObj for Probe {
    fn interval(&self) -> Duration {
        self.interval
    }
    fn step(&mut self, setup: Option<Setup>) {
        if self.panic_on_step {
            panic!("a worker that panics");
        }
        if setup.is_some() {
            self.setups.fetch_add(1, Ordering::SeqCst);
        }
        // With denormals flushed, half of the smallest normal number is zero.
        let tiny = std::hint::black_box(f32::MIN_POSITIVE) * std::hint::black_box(0.5);
        self.flushed.store(tiny == 0.0, Ordering::SeqCst);
        self.steps.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct Handles {
    steps: Arc<AtomicUsize>,
    setups: Arc<AtomicU32>,
    dropped: Arc<AtomicBool>,
    flushed: Arc<AtomicBool>,
}

fn probe(interval_ms: u64, panics: bool) -> (Arc<Slot>, Handles) {
    let h = Handles { steps: Arc::default(), setups: Arc::default(), dropped: Arc::default(), flushed: Arc::default() };
    let p = Probe {
        steps: h.steps.clone(),
        setups: h.setups.clone(),
        dropped: h.dropped.clone(),
        panic_on_step: panics,
        interval: Duration::from_millis(interval_ms),
        flushed: h.flushed.clone(),
    };
    (Slot::new(Box::new(p)), h)
}

fn setup() -> Setup {
    Setup { sample_rate: 48_000.0, max_block_size: 512, offline: false, quality: crate::Quality::Normal, layout: Default::default(), latency: 0 }
}

#[test]
fn the_worker_thread_calls_workers_at_their_interval_and_stops_when_none_are_left() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let (slot, h) = probe(10, false);
    register(&slot);
    assert!(thread_running());
    wait_until("the first calls", || h.steps.load(Ordering::SeqCst) >= 5);
    // 10 ms apart: in 100 ms there are about 10 calls, never hundreds
    let before = h.steps.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100));
    let calls = h.steps.load(Ordering::SeqCst) - before;
    assert!((3..=14).contains(&calls), "{calls} calls in 100 ms");
    unregister(&slot);
    assert!(h.dropped.load(Ordering::SeqCst), "the worker is dropped with its instance");
    let after = h.steps.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(h.steps.load(Ordering::SeqCst), after, "no calls after unregister");
    wait_until("the thread to exit", || !thread_running());
    // and it comes back for the next worker
    let (slot2, h2) = probe(10, false);
    register(&slot2);
    wait_until("a call after restart", || h2.steps.load(Ordering::SeqCst) >= 1);
    unregister(&slot2);
}

#[test]
fn the_worker_thread_flushes_denormals() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let (slot, h) = probe(5, false);
    register(&slot);
    wait_until("a call", || h.steps.load(Ordering::SeqCst) >= 1);
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        assert!(h.flushed.load(Ordering::SeqCst), "denormals are flushed to zero on the worker thread");
    }
    unregister(&slot);
}

#[test]
fn activation_reaches_the_worker_without_waiting_for_the_interval() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let (slot, h) = probe(5_000, false);
    register(&slot);
    wait_until("the first call", || h.steps.load(Ordering::SeqCst) >= 1);
    slot.activated(setup());
    wait_until("the activation", || h.setups.load(Ordering::SeqCst) == 1);
    unregister(&slot);
}

#[test]
fn a_panicking_worker_is_switched_off_and_the_others_go_on() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let (bad, hb) = probe(5, true);
    let (good, hg) = probe(5, false);
    register(&bad);
    register(&good);
    wait_until("the good worker", || hg.steps.load(Ordering::SeqCst) >= 3);
    wait_until("the bad worker to be dropped", || hb.dropped.load(Ordering::SeqCst));
    unregister(&bad);
    unregister(&good);
}

// ---- the typed worker: batching of parameter changes -----------------------------------

struct Two {
    a: FloatParam,
    b: FloatParam,
}
static INFOS: [ParamInfo; 2] = [
    ParamInfo { id: 1, key: "a", name: "A", unit: "", kind: ParamKind::Float { min: 0.0, max: 1.0 }, default: 0.0, scale: crate::ParamScale::Linear, text: None },
    ParamInfo { id: 2, key: "b", name: "B", unit: "", kind: ParamKind::Float { min: 0.0, max: 1.0 }, default: 0.0, scale: crate::ParamScale::Linear, text: None },
];
impl Default for Two {
    fn default() -> Self {
        Self { a: FloatParam::new(0.0), b: FloatParam::new(0.0) }
    }
}
impl ParamSet for Two {
    fn infos() -> &'static [ParamInfo] {
        &INFOS
    }
    fn cell(&self, index: usize) -> &ParamCell {
        [self.a.cell(), self.b.cell()][index]
    }
    fn index_of(id: u32) -> Option<usize> {
        INFOS.iter().position(|i| i.id == id)
    }
}

#[derive(Default)]
struct Plug;
impl Process for Plug {
    type Params = Two;
    fn process(&mut self, _ctx: &mut crate::Context<'_, Two>) {}
}

type Log = Arc<Mutex<Vec<String>>>;

struct Logger {
    log: Log,
}
static LOG: Mutex<Option<Log>> = Mutex::new(None);

impl Worker for Logger {
    type Plugin = Plug;
    fn new(_ctx: &mut WorkerContext<'_, Plug>) -> Self {
        Logger { log: LOG.lock().unwrap().clone().unwrap() }
    }
    fn activated(&mut self, ctx: &mut WorkerContext<'_, Plug>, setup: &Setup) {
        self.log.lock().unwrap().push(format!("activated {} {}", setup.sample_rate, ctx.sample_rate()));
    }
    fn params_changed(&mut self, ctx: &mut WorkerContext<'_, Plug>, changed: &ChangedParams<'_>) {
        let a = changed.contains(&ctx.params().a);
        let b = changed.contains(&ctx.params().b);
        self.log.lock().unwrap().push(format!("changed {:?} a={a} b={b}", changed.indices()));
    }
}

fn typed(log: &Log) -> (Arc<Slot>, Arc<Two>, InstanceHandle) {
    *LOG.lock().unwrap() = Some(log.clone());
    let params = Arc::new(Two::default());
    let link = crate::instance::Link::new::<Plug>("test.worker", params.clone(), &[]);
    let handle = InstanceHandle(link);
    let any: Arc<dyn Any + Send + Sync> = params.clone();
    let obj = make::<Logger>(&any, &handle).expect("the worker is made");
    (Slot::new(obj), params, handle)
}

#[test]
fn parameter_changes_are_batched_and_the_first_call_lists_every_parameter() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let log: Log = Arc::default();
    let (slot, params, handle) = typed(&log);
    // Not registered with the thread: driven by hand, so the batching is exact.
    assert!(slot.run(true).is_some());
    assert_eq!(log.lock().unwrap().as_slice(), ["changed [0, 1] a=true b=true"], "the first call lists them all");
    // a burst of changes between two calls is one call, listing what moved
    for v in [0.1, 0.2, 0.3] {
        params.a.cell().set(v);
    }
    assert!(slot.run(true).is_some());
    assert_eq!(log.lock().unwrap().last().unwrap(), "changed [0] a=true b=false");
    // nothing changed: no params_changed
    let n = log.lock().unwrap().len();
    assert!(slot.run(true).is_some());
    assert_eq!(log.lock().unwrap().len(), n);
    // writing the value it already has is not a change
    params.b.cell().set(0.0);
    slot.run(true);
    assert_eq!(log.lock().unwrap().len(), n);
    slot.activated(setup());
    slot.run(true);
    assert_eq!(log.lock().unwrap().last().unwrap(), "activated 48000 48000");
    handle.0.destroy();
}

#[test]
fn within_the_interval_a_busy_parameter_causes_one_call() {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let log: Log = Arc::default();
    let (slot, params, _handle) = typed(&log);
    register(&slot);
    wait_until("the first call", || !log.lock().unwrap().is_empty());
    let start = log.lock().unwrap().len();
    // 300 changes within a few ms
    for i in 0..300 {
        params.b.cell().set((i % 7) as f64 / 7.0 + 0.01);
    }
    wait_until("the batched call", || log.lock().unwrap().len() > start);
    std::thread::sleep(Duration::from_millis(40));
    let calls = log.lock().unwrap().len() - start;
    assert!((1..=2).contains(&calls), "{calls} calls for one burst");
    unregister(&slot);
}
