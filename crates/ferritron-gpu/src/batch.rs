//! The GPU threads shared by every instance of a kind of processor in the
//! process (batching across instances, design doc 9.2): audio threads hand
//! their work to a slot and [`Batcher::notify`]; the thread takes the work of
//! all slots and runs it together, so many instances pay the GPU round trip
//! once per round instead of once each.
//!
//! A thread lives only while slots exist: the last slot to go stops it and
//! waits (for a bounded time) for it to exit, so nothing runs on after the
//! host unloads the plugin library. A thread that ended by panic is replaced
//! by the next slot that registers.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the last slot waits for its thread to exit before leaving it
/// behind (a thread stuck in a GPU call must not hold up the host).
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// Longest an idle thread sleeps without a notification. Notifications
/// normally wake it at once; this bounds a wake-up lost in the small window
/// between the thread's check for work and its wait.
const IDLE_CHECK: Duration = Duration::from_millis(10);

/// One GPU thread and the slots it serves.
pub(crate) struct Batcher<S> {
    slots: Mutex<Vec<Arc<S>>>,
    pending: AtomicBool,
    stop: AtomicBool,
    /// Set when the thread ended by panic: the registry starts a new one.
    dead: AtomicBool,
    signal: Mutex<()>,
    wake: Condvar,
    /// Set when the thread has returned (or panicked).
    exited: Mutex<bool>,
    exit_signal: Condvar,
}

impl<S> Batcher<S> {
    /// Called by audio threads after handing over work. Never blocks.
    pub fn notify(&self) {
        self.pending.store(true, Relaxed);
        self.wake.notify_one();
    }

    /// Waits until work is pending (true) or the thread is to stop (false).
    pub fn wait(&self) -> bool {
        loop {
            if let Some(answer) = self.wait_within(Duration::MAX) {
                return answer;
            }
        }
    }

    /// Like [`wait`](Self::wait), but gives up after `limit`: `None` means
    /// nothing came in time (the thread may do its own periodic work).
    pub fn wait_within(&self, limit: Duration) -> Option<bool> {
        let started = Instant::now();
        loop {
            if self.stop.load(Relaxed) {
                return Some(false);
            }
            if self.pending.swap(false, Relaxed) {
                return Some(true);
            }
            let left = limit.saturating_sub(started.elapsed());
            if left.is_zero() {
                return None;
            }
            let guard = self.signal.lock().unwrap_or_else(PoisonError::into_inner);
            if self.pending.load(Relaxed) || self.stop.load(Relaxed) {
                continue;
            }
            let _ = self.wake.wait_timeout(guard, left.min(IDLE_CHECK));
        }
    }

    /// The slots registered now.
    pub fn slots(&self) -> Vec<Arc<S>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// True if the thread ended by panic.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Relaxed)
    }
}

/// A batcher and its thread.
type Running<S> = (Arc<Batcher<S>>, Option<JoinHandle<()>>);

/// The running batcher of one kind of slot and its thread, if any slot exists.
pub(crate) struct Registry<S>(Mutex<Option<Running<S>>>);

impl<S: Send + Sync + 'static> Registry<S> {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Registers `slot`, starting the thread `name` (running `run`) if none
    /// is running (or the running one ended by panic).
    pub fn register(&self, name: &str, run: fn(&Batcher<S>), slot: Arc<S>) -> Result<Arc<Batcher<S>>, String> {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let batcher = match running.as_ref() {
            Some((batcher, _)) if !batcher.is_dead() => batcher.clone(),
            _ => {
                let batcher = Arc::new(Batcher {
                    slots: Mutex::new(Vec::new()),
                    pending: AtomicBool::new(false),
                    stop: AtomicBool::new(false),
                    dead: AtomicBool::new(false),
                    signal: Mutex::new(()),
                    wake: Condvar::new(),
                    exited: Mutex::new(false),
                    exit_signal: Condvar::new(),
                });
                let b = batcher.clone();
                let thread = std::thread::Builder::new()
                    .name(name.into())
                    .spawn(move || {
                        // A panic must not leave the slots waiting for results forever.
                        if catch_unwind(AssertUnwindSafe(|| run(&b))).is_err() {
                            b.dead.store(true, Relaxed);
                        }
                        *b.exited.lock().unwrap_or_else(PoisonError::into_inner) = true;
                        b.exit_signal.notify_all();
                    })
                    .map_err(|e| format!("cannot start the GPU thread: {e}"))?;
                *running = Some((batcher.clone(), Some(thread)));
                batcher
            }
        };
        batcher.slots.lock().unwrap_or_else(PoisonError::into_inner).push(slot);
        Ok(batcher)
    }

    /// Unregisters `slot`, which was registered with `batcher`. The last
    /// slot stops the thread and waits up to two seconds for it to exit;
    /// after that the thread is left detached.
    pub fn unregister(&self, slot: &Arc<S>, batcher: &Arc<Batcher<S>>) {
        let empty = {
            let mut slots = batcher.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.retain(|s| !Arc::ptr_eq(s, slot));
            slots.is_empty()
        };
        if !empty {
            return;
        }
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let is_running = running.as_ref().is_some_and(|(b, _)| Arc::ptr_eq(b, batcher));
        if !is_running {
            return;
        }
        let Some((batcher, thread)) = running.take() else { return };
        batcher.stop.store(true, Relaxed);
        batcher.wake.notify_one();
        let Some(thread) = thread else { return };
        let started = Instant::now();
        let mut exited = batcher.exited.lock().unwrap_or_else(PoisonError::into_inner);
        while !*exited {
            let left = EXIT_WAIT.saturating_sub(started.elapsed());
            if left.is_zero() {
                return; // detached: the thread stays behind
            }
            exited = batcher.exit_signal.wait_timeout(exited, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(exited);
        let _ = thread.join();
    }

    /// Number of slots registered (for tests and diagnostics).
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or(0, |(b, _)| b.slots.lock().unwrap_or_else(PoisonError::into_inner).len())
    }
}
