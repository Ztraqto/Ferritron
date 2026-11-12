//! Workers (R2): heavy computation off the audio thread.
//!
//! A plugin declares a worker type next to its `Process` type:
//!
//! ```ignore
//! #[ferritron::plugin(worker = MyWorker)]
//! impl Process for MyPlugin { /* ... */ }
//!
//! struct MyWorker { /* ... */ }
//!
//! impl ferritron::Worker for MyWorker {
//!     type Plugin = MyPlugin;
//!     fn new(ctx: &mut WorkerContext<'_, MyPlugin>) -> Self { /* ... */ }
//!     fn params_changed(&mut self, ctx: &mut WorkerContext<'_, MyPlugin>, changed: &ChangedParams<'_>) { /* ... */ }
//! }
//! ```
//!
//! Ferritron keeps **one worker thread per process** and calls every instance's worker in
//! turn, at most once per [`Worker::MIN_INTERVAL`] (10 ms by default). The thread runs with
//! denormals flushed, at normal priority (below the audio threads), and exists only while
//! some instance has a worker. Parameter changes arrive batched in `params_changed`: a
//! knob moved by automation causes at most one call per interval, listing everything that
//! moved since the previous call (the first call lists every parameter).
//!
//! The worker hands its results to the audio thread with
//! [`Swap`](crate::util::Swap): the worker keeps the writer, the plugin the reader, and the
//! audio thread reads the newest value without waiting or allocating. The worker is created
//! before [`Process::init`] runs, so it can pass the reader on through
//! [`InstanceHandle::attach`](crate::InstanceHandle::attach) and a [`Handoff`]; `init` takes it
//! with [`InstanceHandle::shared`](crate::InstanceHandle::shared).
//!
//! A worker is dropped when its instance is destroyed (its last call has then finished). A
//! worker that panics is switched off; the plugin keeps working.
//!
//! Offline rendering: the worker runs in real time beside the render, so what it hands over
//! is not tied to a sample position. A plugin that needs deterministic exports computes in
//! `process` when [`Setup::offline`](crate::Setup::offline) is set.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::stream::TapReader;
use crate::{InstanceHandle, Param, ParamCell, ParamSet, Process, Setup};

/// What a worker needs to see of its instance.
pub struct WorkerContext<'a, P: Process> {
    params: &'a P::Params,
    instance: &'a InstanceHandle,
    sample_rate: f64,
}

impl<'a, P: Process> WorkerContext<'a, P> {
    /// The plugin's parameters (read-only values, as `ctx.params()` in `process`).
    pub fn params(&self) -> &'a P::Params {
        self.params
    }

    /// The instance (identity, track, [`attach`](InstanceHandle::attach), streams).
    pub fn instance(&self) -> &'a InstanceHandle {
        self.instance
    }

    /// The sample rate of the last activation (0 before the first).
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Whether an editor of the instance is open.
    pub fn editor_open(&self) -> bool {
        self.instance.is_editor_open()
    }

    /// The reader of tap `name` (`kind = "tap"` under `[streams]`). One reader exists at a
    /// time: `None` when the name is unknown or the reader is taken.
    pub fn tap(&self, name: &str) -> Option<TapReader> {
        self.instance.tap(name)
    }
}

/// Parameters that changed since the worker's previous `params_changed` call.
pub struct ChangedParams<'a> {
    indices: &'a [usize],
    cells: &'a dyn CellLookup,
}

#[doc(hidden)]
pub trait CellLookup {
    fn cell_at(&self, index: usize) -> &ParamCell;
}

impl<S: ParamSet> CellLookup for S {
    fn cell_at(&self, index: usize) -> &ParamCell {
        self.cell(index)
    }
}

impl ChangedParams<'_> {
    /// The indices (see [`ParamSet`]) of the changed parameters.
    pub fn indices(&self) -> &[usize] {
        self.indices
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn contains_index(&self, index: usize) -> bool {
        self.indices.contains(&index)
    }

    /// Whether `p` is among the changed parameters.
    pub fn contains(&self, p: &impl Param) -> bool {
        let cell = p.param_cell();
        self.indices.iter().any(|&i| std::ptr::eq(self.cells.cell_at(i), cell))
    }
}

/// A computation that runs off the audio thread, one per plugin instance (see the [module docs](self)).
pub trait Worker: Send + 'static {
    /// The plugin this worker belongs to (`#[ferritron::plugin(worker = ...)]` checks it).
    type Plugin: Process;

    /// The shortest time between two calls for this instance (batching of parameter changes,
    /// pace of [`tick`](Worker::tick)).
    const MIN_INTERVAL: Duration = Duration::from_millis(10);

    /// Creates the worker, when the instance is created and before `Process::init`.
    /// Allocate and set up the hand-over to the audio thread here.
    fn new(ctx: &mut WorkerContext<'_, Self::Plugin>) -> Self;

    /// The plugin was activated (also after a sample-rate change).
    fn activated(&mut self, ctx: &mut WorkerContext<'_, Self::Plugin>, setup: &Setup) {
        let _ = (ctx, setup);
    }

    /// Parameters changed since the last call (the first call lists them all).
    fn params_changed(&mut self, ctx: &mut WorkerContext<'_, Self::Plugin>, changed: &ChangedParams<'_>) {
        let _ = (ctx, changed);
    }

    /// Called every [`MIN_INTERVAL`](Worker::MIN_INTERVAL): read a tap, update results.
    fn tick(&mut self, ctx: &mut WorkerContext<'_, Self::Plugin>) {
        let _ = ctx;
    }
}

/// A value handed from the worker to the plugin once: the worker `attach`es an
/// `Arc<Handoff<T>>`, `Process::init` takes `T` out of it.
pub struct Handoff<T>(Mutex<Option<T>>);

impl<T> Handoff<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(Some(value)))
    }

    /// The value, the first time; `None` afterwards.
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// A worker without its type (what Ferritron keeps).
#[doc(hidden)]
pub trait WorkerObj: Send {
    fn interval(&self) -> Duration;
    fn step(&mut self, setup: Option<Setup>);
}

struct Adapter<W: Worker> {
    w: W,
    params: Arc<<W::Plugin as Process>::Params>,
    instance: InstanceHandle,
    sample_rate: f64,
    versions: Vec<u32>,
    changed: Vec<usize>,
}

impl<W: Worker> WorkerObj for Adapter<W> {
    fn interval(&self) -> Duration {
        W::MIN_INTERVAL
    }

    fn step(&mut self, setup: Option<Setup>) {
        if let Some(setup) = setup {
            self.sample_rate = setup.sample_rate;
            let mut ctx = WorkerContext::<W::Plugin> { params: &self.params, instance: &self.instance, sample_rate: self.sample_rate };
            self.w.activated(&mut ctx, &setup);
        }
        self.changed.clear();
        for (i, v) in self.versions.iter_mut().enumerate() {
            let now = self.params.cell(i).version();
            if *v != now {
                *v = now;
                self.changed.push(i);
            }
        }
        let mut ctx = WorkerContext::<W::Plugin> { params: &self.params, instance: &self.instance, sample_rate: self.sample_rate };
        if !self.changed.is_empty() {
            let changed = ChangedParams { indices: &self.changed, cells: &*self.params };
            self.w.params_changed(&mut ctx, &changed);
        }
        self.w.tick(&mut ctx);
    }
}

/// Builds the worker of plugin instance `instance` (called by the generated code and `create`).
#[doc(hidden)]
pub fn make<W: Worker>(params: &Arc<dyn Any + Send + Sync>, instance: &InstanceHandle) -> Option<Box<dyn WorkerObj>> {
    let params = params.clone().downcast::<<W::Plugin as Process>::Params>().ok()?;
    let mut ctx = WorkerContext::<W::Plugin> { params: &params, instance, sample_rate: 0.0 };
    let w = W::new(&mut ctx);
    let n = <<W::Plugin as Process>::Params as ParamSet>::infos().len();
    // Versions that differ from the cells', so the first call lists every parameter.
    let versions = (0..n).map(|i| params.cell(i).version().wrapping_sub(1)).collect();
    Some(Box::new(Adapter::<W> { w, params, instance: instance.clone(), sample_rate: 0.0, versions, changed: Vec::with_capacity(n) }))
}

struct SlotInner {
    obj: Option<Box<dyn WorkerObj>>,
    last: Option<Instant>,
    interval: Duration,
}

/// One instance's worker in the pool.
pub struct Slot {
    inner: Mutex<SlotInner>,
    setup: Mutex<Option<Setup>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Slot {
    pub(crate) fn new(obj: Box<dyn WorkerObj>) -> Arc<Slot> {
        let interval = obj.interval().max(Duration::from_millis(1));
        Arc::new(Slot { inner: Mutex::new(SlotInner { obj: Some(obj), last: None, interval }), setup: Mutex::new(None) })
    }

    /// The instance was (re)activated: the worker hears about it on its next call, soon.
    pub(crate) fn activated(&self, setup: Setup) {
        *lock(&self.setup) = Some(setup);
        wake();
    }

    /// Runs the worker if it is due (or `force`). Returns when it is due next; `None` when it is gone.
    fn run(&self, force: bool) -> Option<Instant> {
        let mut g = lock(&self.inner);
        let interval = g.interval;
        g.obj.as_ref()?;
        let pending = lock(&self.setup).take();
        let due = force || pending.is_some() || g.last.is_none_or(|l| l.elapsed() >= interval);
        if !due {
            return g.last.map(|l| l + interval);
        }
        let obj = g.obj.as_mut().expect("checked above");
        if catch_unwind(AssertUnwindSafe(|| obj.step(pending))).is_err() {
            // A panicking worker is switched off; the plugin keeps working.
            g.obj = None;
            return None;
        }
        let now = Instant::now();
        g.last = Some(now);
        Some(now + interval)
    }

    /// Takes the worker out (after its running call has finished) and drops it.
    fn close(&self) {
        let obj = lock(&self.inner).obj.take();
        drop(obj);
    }
}

struct Pool {
    slots: Vec<Arc<Slot>>,
    /// The worker thread, while there are workers. Joined when the last one goes, so no
    /// thread of the plugin's library outlives its instances (a host may unload the library
    /// right after destroying the last instance).
    thread: Option<std::thread::JoinHandle<()>>,
    /// Which thread is current: a thread whose number is no longer this one ends.
    generation: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { slots: Vec::new(), thread: None, generation: 0 });
static WAKE: Condvar = Condvar::new();

fn wake() {
    WAKE.notify_all();
}

/// Adds a worker to the pool (starting the thread if it is not running).
pub(crate) fn register(slot: &Arc<Slot>) {
    let mut pool = lock(&POOL);
    pool.slots.push(slot.clone());
    if pool.thread.is_none() {
        pool.generation += 1;
        let generation = pool.generation;
        let spawned = std::thread::Builder::new().name("ferritron-worker".into()).spawn(move || thread_main(generation));
        match spawned {
            Ok(handle) => pool.thread = Some(handle),
            Err(_) => {
                // No thread: the worker never runs, the plugin still works.
                pool.slots.retain(|s| !Arc::ptr_eq(s, slot));
            }
        }
    }
    drop(pool);
    wake();
}

/// Removes a worker; returns after its running call (if any) is over, and drops it. The last
/// one also waits for the worker thread to end.
pub(crate) fn unregister(slot: &Arc<Slot>) {
    let finished = {
        let mut pool = lock(&POOL);
        pool.slots.retain(|s| !Arc::ptr_eq(s, slot));
        if pool.slots.is_empty() {
            pool.generation += 1;
            pool.thread.take()
        } else {
            None
        }
    };
    slot.close();
    wake();
    if let Some(handle) = finished
        && handle.thread().id() != std::thread::current().id()
    {
        let _ = handle.join();
    }
}

fn thread_main(generation: u64) {
    // Denormals flushed for the thread's whole life (as on audio threads).
    let _denormals = crate::abi::DenormalGuard::new();
    loop {
        let slots = {
            let pool = lock(&POOL);
            if pool.generation != generation || pool.slots.is_empty() {
                return;
            }
            pool.slots.clone()
        };
        let mut next: Option<Instant> = None;
        for s in &slots {
            if let Some(t) = s.run(false) {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        drop(slots);
        let wait = next.map_or(Duration::from_millis(100), |t| t.saturating_duration_since(Instant::now())).max(Duration::from_millis(1));
        let pool = lock(&POOL);
        if pool.generation != generation {
            return;
        }
        let _ = WAKE.wait_timeout(pool, wait).unwrap_or_else(PoisonError::into_inner);
    }
}

/// How many workers are registered (tests).
#[doc(hidden)]
pub fn registered() -> usize {
    lock(&POOL).slots.len()
}

/// Whether the worker thread is running (tests).
#[doc(hidden)]
pub fn thread_running() -> bool {
    lock(&POOL).thread.is_some()
}

/// Runs every worker once now on this thread, ignoring the interval (tests: deterministic
/// instead of waiting for the thread).
#[doc(hidden)]
pub fn poll_now() {
    let slots = lock(&POOL).slots.clone();
    for s in slots {
        let _ = s.run(true);
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
