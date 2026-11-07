//! Ferritron DirectLink (design doc, Phase 7): a common way for Ferritron
//! plugins to talk to each other, such as a gain-match pair where the plugin
//! after an effect needs the level measured before it.
//!
//! Plugins meet on a **channel**, named by a namespace (usually the plugin
//! id) and a channel name (e.g. "group 3"). A channel is a small region of
//! named shared memory, so it works between plugin libraries and between
//! processes (DAWs that run plugins in separate sandboxes), and the
//! operating system frees it with its last user: no leaks, and nothing left
//! dangling when a plugin crashes. On a channel, **ports** carry named
//! values (up to [`MAX_VALUES`] floats each):
//!
//! - publishing and reading never block, allocate or lock, so both are
//!   safe on the audio thread; a reading comes with its age;
//! - several writers on one port are safe: a write that would collide is
//!   skipped, never torn;
//! - links survive reopening a project: the plugin keeps its channel name
//!   in its parameters or state, and opening the same channel again joins
//!   the same peers.
//!
//! ```ignore
//! let link = DirectLink::open("com.example.gainmatch", "group 1")?; // in activate
//! let level = link.port("level")?;
//! level.publish(&[ms]);                                            // audio thread
//! let mut v = [0.0]; if let Some(r) = level.read(&mut v) { ... r.age ... }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Ports per channel.
pub const MAX_PORTS: usize = 32;
/// Floats per port.
pub const MAX_VALUES: usize = 64;
const NAME: usize = 48;
const MAGIC: u32 = u32::from_le_bytes(*b"FDL1");

#[repr(C)]
struct Slot {
    /// 0 = free, 1 = being claimed, 2 = named.
    claimed: AtomicU32,
    name_len: AtomicU32,
    name: [AtomicU32; NAME / 4],
    /// Seqlock: odd while a write is in progress.
    seq: AtomicU32,
    len: AtomicU32,
    time_ns: AtomicU64,
    values: [AtomicU32; MAX_VALUES],
}

#[repr(C)]
struct Region {
    magic: AtomicU32,
    version: AtomicU32,
    slots: [Slot; MAX_PORTS],
}

struct Inner {
    map: crate::shmem::Mapping,
}

impl Inner {
    fn region(&self) -> &Region {
        unsafe { &*(self.map.data() as *const Region) }
    }
}

/// An open channel. Cheap to clone.
#[derive(Clone)]
pub struct DirectLink {
    inner: Arc<Inner>,
    channel: String,
}

/// A named value on a channel. Cheap to clone; publish and read are
/// real-time safe.
#[derive(Clone)]
pub struct Port {
    inner: Arc<Inner>,
    slot: usize,
}

/// What [`Port::read`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// Values written into the caller's buffer.
    pub len: usize,
    /// Time since it was published.
    pub age: Duration,
}

impl DirectLink {
    /// Joins channel `channel` of `namespace` (creating it if this is the
    /// first user). Call off the audio thread.
    pub fn open(namespace: &str, channel: &str) -> std::io::Result<DirectLink> {
        let key = format!("{namespace}\u{1f}{channel}");
        let map = crate::shmem::Mapping::open(&crate::shmem::name_for("link", &key), size_of::<Region>())?;
        let inner = Arc::new(Inner { map });
        let region = inner.region();
        // A zero-filled region is a valid empty channel; mark its format.
        if region.magic.compare_exchange(0, MAGIC, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            region.version.store(1, Ordering::Release);
        } else if region.magic.load(Ordering::Acquire) != MAGIC || region.version.load(Ordering::Acquire) != 1 {
            return Err(std::io::Error::other("the channel was created by an incompatible DirectLink version"));
        }
        Ok(DirectLink { inner, channel: channel.to_string() })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The port named `name` (at most 48 bytes), claimed if new. None when
    /// the channel has no free port. Call off the audio thread.
    pub fn port(&self, name: &str) -> Option<Port> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME {
            return None;
        }
        let region = self.inner.region();
        let mut words = [0u32; NAME / 4];
        for (i, b) in bytes.iter().enumerate() {
            words[i / 4] |= (*b as u32) << (8 * (i % 4));
        }
        let matches = |slot: &Slot| {
            slot.claimed.load(Ordering::Acquire) == 2
                && slot.name_len.load(Ordering::Acquire) as usize == bytes.len()
                && slot.name.iter().zip(words).all(|(w, v)| w.load(Ordering::Relaxed) == v)
        };
        // Twice: a port may be being claimed by someone else right now.
        for _ in 0..2 {
            if let Some(i) = region.slots.iter().position(matches) {
                return Some(Port { inner: self.inner.clone(), slot: i });
            }
            for (i, slot) in region.slots.iter().enumerate() {
                if slot.claimed.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    for (w, v) in slot.name.iter().zip(words) {
                        w.store(v, Ordering::Relaxed);
                    }
                    slot.name_len.store(bytes.len() as u32, Ordering::Relaxed);
                    slot.claimed.store(2, Ordering::Release);
                    // Two processes may have claimed the same name at once:
                    // everyone uses the first slot with the name.
                    let first = region.slots.iter().position(matches).unwrap_or(i);
                    return Some(Port { inner: self.inner.clone(), slot: first });
                }
            }
            std::thread::yield_now();
        }
        None
    }
}

impl Port {
    fn slot(&self) -> &Slot {
        &self.inner.region().slots[self.slot]
    }

    /// Publishes `values` (at most [`MAX_VALUES`]). Never blocks; returns
    /// false if another writer was publishing at the same moment (the value
    /// is then dropped, not torn).
    pub fn publish(&self, values: &[f32]) -> bool {
        let slot = self.slot();
        let seq = slot.seq.load(Ordering::Acquire);
        if seq % 2 == 1 || slot.seq.compare_exchange(seq, seq + 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        let n = values.len().min(MAX_VALUES);
        for (dst, &v) in slot.values.iter().zip(&values[..n]) {
            dst.store(v.to_bits(), Ordering::Relaxed);
        }
        slot.len.store(n as u32, Ordering::Relaxed);
        slot.time_ns.store(now_ns(), Ordering::Relaxed);
        slot.seq.store(seq + 2, Ordering::Release);
        true
    }

    /// Reads the latest values into `out`. None if nothing was published
    /// yet (or a writer kept the value busy). Never blocks.
    pub fn read(&self, out: &mut [f32]) -> Option<Reading> {
        let slot = self.slot();
        for _ in 0..8 {
            let before = slot.seq.load(Ordering::Acquire);
            if before == 0 {
                return None;
            }
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let len = (slot.len.load(Ordering::Relaxed) as usize).min(MAX_VALUES).min(out.len());
            for (o, v) in out.iter_mut().zip(&slot.values[..len]) {
                *o = f32::from_bits(v.load(Ordering::Relaxed));
            }
            let time = slot.time_ns.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Relaxed) == before {
                return Some(Reading { len, age: Duration::from_nanos(now_ns().saturating_sub(time)) });
            }
        }
        None
    }
}

/// A monotonic clock shared by all processes of the machine.
fn now_ns() -> u64 {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(count: *mut i64) -> i32;
            fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
        }
        let (mut count, mut freq) = (0i64, 0i64);
        unsafe {
            QueryPerformanceCounter(&mut count);
            QueryPerformanceFrequency(&mut freq);
        }
        (count as u128 * 1_000_000_000 / freq.max(1) as u128) as u64
    }
    #[cfg(unix)]
    {
        #[repr(C)]
        struct Timespec {
            sec: i64,
            nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
        }
        let mut ts = Timespec { sec: 0, nsec: 0 };
        unsafe { clock_gettime(1, &mut ts) }; // CLOCK_MONOTONIC
        ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(name: &str) -> String {
        format!("ferritron-test/{}/{name}", std::process::id())
    }

    #[test]
    fn peers_on_a_channel_see_each_others_values() {
        let ns = unique("pair");
        let sender = DirectLink::open(&ns, "group 1").unwrap();
        let receiver = DirectLink::open(&ns, "group 1").unwrap();
        let (tx, rx) = (sender.port("level").unwrap(), receiver.port("level").unwrap());
        let mut out = [0.0f32; 4];
        assert_eq!(rx.read(&mut out), None, "nothing published yet");
        assert!(tx.publish(&[0.25, -3.0]));
        let r = rx.read(&mut out).unwrap();
        assert_eq!((r.len, out[0], out[1]), (2, 0.25, -3.0));
        assert!(r.age < Duration::from_secs(1));
        // Another channel, or another namespace, is separate.
        let other = DirectLink::open(&ns, "group 2").unwrap().port("level").unwrap();
        assert_eq!(other.read(&mut out), None);
        let foreign = DirectLink::open(&unique("other"), "group 1").unwrap().port("level").unwrap();
        assert_eq!(foreign.read(&mut out), None);
    }

    #[test]
    fn writers_from_many_threads_never_tear_a_value() {
        let link = DirectLink::open(&unique("threads"), "c").unwrap();
        let port = link.port("v").unwrap();
        let writers: Vec<_> = (0..4)
            .map(|t| {
                let port = port.clone();
                std::thread::spawn(move || {
                    for i in 0..20_000 {
                        let v = (t * 100_000 + i) as f32;
                        port.publish(&[v; MAX_VALUES]);
                    }
                })
            })
            .collect();
        let mut out = [0.0f32; MAX_VALUES];
        for _ in 0..20_000 {
            if let Some(r) = port.read(&mut out) {
                assert_eq!(r.len, MAX_VALUES);
                assert!(out.iter().all(|&v| v == out[0]), "a reading mixed two writes");
            }
        }
        writers.into_iter().for_each(|w| w.join().unwrap());
    }

    /// Child side of `values_cross_process_boundaries` (does nothing unless
    /// started by it).
    #[test]
    fn child_publisher() {
        let Ok(namespace) = std::env::var("FERRITRON_DIRECTLINK_CHILD") else { return };
        let port = DirectLink::open(&namespace, "x").unwrap().port("from-child").unwrap();
        for i in 0..400 {
            port.publish(&[42.0, i as f32]);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn values_cross_process_boundaries() {
        let namespace = unique("process");
        // Joined first, so the channel exists while the child runs.
        let port = DirectLink::open(&namespace, "x").unwrap().port("from-child").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "directlink::tests::child_publisher", "--quiet"])
            .env("FERRITRON_DIRECTLINK_CHILD", &namespace)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut out = [0.0f32; 2];
        let start = std::time::Instant::now();
        let mut seen = None;
        while start.elapsed() < Duration::from_secs(10) && seen.is_none() {
            if let Some(r) = port.read(&mut out)
                && out[0] == 42.0 && r.age < Duration::from_secs(1) {
                    seen = Some(out[1]);
                }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen.is_some(), "nothing arrived from the other process");
    }

    #[test]
    fn ports_are_found_by_name_and_limited() {
        let link = DirectLink::open(&unique("ports"), "c").unwrap();
        let a = link.port("alpha").unwrap();
        let again = link.port("alpha").unwrap();
        assert_eq!(a.slot, again.slot);
        assert!(link.port("").is_none());
        assert!(link.port(&"x".repeat(49)).is_none());
        let ports: Vec<_> = (0..MAX_PORTS).filter_map(|i| link.port(&format!("p{i}"))).collect();
        assert_eq!(ports.len(), MAX_PORTS - 1, "one slot is alpha");
    }
}
