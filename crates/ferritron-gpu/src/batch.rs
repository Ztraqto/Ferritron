//! The GPU threads shared by every instance of a kind of processor in the
//! process (batching across instances, design doc 9.2): audio threads hand
//! their work to a slot and [`Batcher::notify`]; the thread takes the work of
//! all slots and runs it together, so many instances pay the GPU round trip
//! once per round instead of once each.
//!
//! A thread lives only while slots exist: the last slot to go stops and joins
//! it, so nothing runs on after the host unloads the plugin library.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// One GPU thread and the slots it serves.
pub(crate) struct Batcher<S> {
    slots: Mutex<Vec<Arc<S>>>,
    pending: AtomicBool,
    stop: AtomicBool,
    signal: Mutex<()>,
    wake: Condvar,
}

impl<S> Batcher<S> {
    /// Called by audio threads after handing over work. Never blocks; a
    /// wake-up lost in a race is caught by the thread's 1 ms timeout.
    pub fn notify(&self) {
        self.pending.store(true, Relaxed);
        self.wake.notify_one();
    }

    /// Waits until work is pending (true) or the thread is to stop (false).
    pub fn wait(&self) -> bool {
        while !self.stop.load(Relaxed) {
            if self.pending.swap(false, Relaxed) {
                return true;
            }
            let guard = self.signal.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = self.wake.wait_timeout(guard, Duration::from_millis(1));
        }
        false
    }

    /// The slots registered now.
    pub fn slots(&self) -> Vec<Arc<S>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// A batcher and its thread.
type Running<S> = (Arc<Batcher<S>>, JoinHandle<()>);

/// The running batcher of one kind of slot and its thread, if any slot exists.
pub(crate) struct Registry<S>(Mutex<Option<Running<S>>>);

impl<S: Send + Sync + 'static> Registry<S> {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Registers `slot`, starting the thread `name` (running `run`) if it is not running.
    pub fn register(&self, name: &str, run: fn(&Batcher<S>), slot: Arc<S>) -> Result<Arc<Batcher<S>>, String> {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let batcher = match running.as_ref() {
            Some((batcher, _)) => batcher.clone(),
            None => {
                let batcher = Arc::new(Batcher {
                    slots: Mutex::new(Vec::new()),
                    pending: AtomicBool::new(false),
                    stop: AtomicBool::new(false),
                    signal: Mutex::new(()),
                    wake: Condvar::new(),
                });
                let b = batcher.clone();
                let thread = std::thread::Builder::new()
                    .name(name.into())
                    .spawn(move || run(&b))
                    .map_err(|e| format!("cannot start the GPU thread: {e}"))?;
                *running = Some((batcher.clone(), thread));
                batcher
            }
        };
        batcher.slots.lock().unwrap_or_else(PoisonError::into_inner).push(slot);
        Ok(batcher)
    }

    /// Unregisters `slot`; the last one stops and joins the thread.
    pub fn unregister(&self, slot: &Arc<S>) {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((batcher, _)) = running.as_ref() else { return };
        let empty = {
            let mut slots = batcher.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.retain(|s| !Arc::ptr_eq(s, slot));
            slots.is_empty()
        };
        if empty && let Some((batcher, thread)) = running.take() {
            batcher.stop.store(true, Relaxed);
            batcher.wake.notify_one();
            let _ = thread.join();
        }
    }

    /// Number of slots registered (for tests and diagnostics).
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or(0, |(b, _)| b.slots.lock().unwrap_or_else(PoisonError::into_inner).len())
    }
}
