//! Notifications from any thread (the audio thread included) to the host's main thread (B5):
//! [`InstanceHandle::notify`](crate::InstanceHandle::notify) and
//! [`InstanceHandle::on_notify`](crate::InstanceHandle::on_notify).
//!
//! The queue is a fixed array of [`CAPACITY`] slots, allocated with the instance. Pushing never
//! allocates, locks or blocks:
//!
//! - a code already waiting is not queued again (the handler runs once for it);
//! - when every slot is taken, the new code is dropped and counted
//!   ([`InstanceHandle::notify_dropped`](crate::InstanceHandle::notify_dropped)).
//!
//! The main thread takes the codes in the order they were first queued.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Codes that can wait at once.
pub const CAPACITY: usize = 64;

pub(crate) struct NotifyQueue {
    codes: [AtomicU64; CAPACITY],
    /// Order of the first push of each slot's code.
    seq: [AtomicU64; CAPACITY],
    /// Slots taken by a producer (a code is being written or waits).
    reserved: AtomicU64,
    /// Slots whose code is written and waits for the main thread.
    ready: AtomicU64,
    next_seq: AtomicU64,
    dropped: AtomicU32,
}

impl NotifyQueue {
    pub(crate) fn new() -> Self {
        Self {
            codes: std::array::from_fn(|_| AtomicU64::new(0)),
            seq: std::array::from_fn(|_| AtomicU64::new(0)),
            reserved: AtomicU64::new(0),
            ready: AtomicU64::new(0),
            next_seq: AtomicU64::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    /// Queues `code`. False if it was dropped (queue full). Lock-free, any thread.
    pub(crate) fn push(&self, code: u64) -> bool {
        // Already waiting: the handler will see it. (A slot the main thread has already taken
        // is no longer in `ready`, so a code pushed after the handler started is queued anew.)
        let mut ready = self.ready.load(Ordering::Acquire);
        while ready != 0 {
            let i = ready.trailing_zeros() as usize;
            ready &= ready - 1;
            if self.codes[i].load(Ordering::Acquire) == code && self.ready.load(Ordering::Acquire) & (1 << i) != 0 {
                return true;
            }
        }
        // Claim a free slot.
        let mut reserved = self.reserved.load(Ordering::Acquire);
        loop {
            if reserved == u64::MAX {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let i = (!reserved).trailing_zeros();
            match self.reserved.compare_exchange_weak(reserved, reserved | (1 << i), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let i = i as usize;
                    self.codes[i].store(code, Ordering::Relaxed);
                    self.seq[i].store(self.next_seq.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
                    self.ready.fetch_or(1 << i, Ordering::Release);
                    return true;
                }
                Err(now) => reserved = now,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> bool {
        self.ready.load(Ordering::Acquire) != 0
    }

    /// Takes the waiting codes, oldest first, into `out`; returns how many. Main thread (one
    /// consumer at a time).
    pub(crate) fn take(&self, out: &mut [u64; CAPACITY]) -> usize {
        let taken = self.ready.swap(0, Ordering::AcqRel);
        let mut slots = [(0u64, 0u64); CAPACITY];
        let mut n = 0;
        let mut bits = taken;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            slots[n] = (self.seq[i].load(Ordering::Relaxed), self.codes[i].load(Ordering::Relaxed));
            n += 1;
        }
        self.reserved.fetch_and(!taken, Ordering::AcqRel);
        slots[..n].sort_unstable_by_key(|s| s.0);
        for (k, s) in slots[..n].iter().enumerate() {
            out[k] = s.1;
        }
        n
    }

    pub(crate) fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_merges_and_counts_drops() {
        let q = NotifyQueue::new();
        assert!(q.push(5));
        assert!(q.push(7));
        assert!(q.push(5)); // merged
        let mut out = [0; CAPACITY];
        assert_eq!(q.take(&mut out), 2);
        assert_eq!(&out[..2], &[5, 7]);
        assert!(!q.pending());
        for c in 0..CAPACITY as u64 {
            assert!(q.push(c));
        }
        assert!(!q.push(1000));
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.take(&mut out), CAPACITY);
        assert!(out.iter().enumerate().all(|(i, &c)| c == i as u64));
        // Slots are free again.
        assert!(q.push(1000));
    }

    #[test]
    fn many_producers() {
        let q = std::sync::Arc::new(NotifyQueue::new());
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let q = q.clone();
                std::thread::spawn(move || {
                    for k in 0..8u64 {
                        q.push(t * 100 + k);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut out = [0; CAPACITY];
        let n = q.take(&mut out);
        let mut got = out[..n].to_vec();
        got.sort();
        let mut want: Vec<u64> = (0..4).flat_map(|t| (0..8).map(move |k| t * 100 + k)).collect();
        want.sort();
        assert_eq!(got, want);
    }
}
