//! Streams (R3): numbers the audio thread hands to editors and workers.
//!
//! Declared in the project file:
//!
//! ```toml
//! [streams]
//! spectrum = { len = 2048 }                 # the latest 2048 values
//! wave     = { len = 16384, kind = "tap" }  # a ring of samples for a worker
//! ```
//!
//! * A **data stream** holds the latest `len` values. The audio thread calls
//!   `ctx.streams().spectrum.write(&data)` (a triple buffer: never waits,
//!   locks or allocates); editors read the newest values ([`StreamReader`],
//!   `ferritron.onStream` in web editors, `ui.stream` in GPU editors). While
//!   no editor is open the writes are dropped, so an analysis nobody sees
//!   costs nothing.
//! * A **tap** is a single-producer single-consumer ring of `f32`: the audio
//!   thread writes samples ([`TapWriter::write`]), a [`Worker`](crate::Worker)
//!   reads them ([`TapReader`]) and does the heavy work (an FFT) off the
//!   audio thread. A tap writes whether or not an editor is open; check
//!   `ctx.editor_open()` first if the result is only for the editor.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::util::{CachePadded, Swap, SwapReader, SwapWriter};

/// Largest `len` of a stream (values).
pub const MAX_STREAM_LEN: usize = 1 << 22;

struct ReaderState {
    reader: SwapReader<Box<[f32]>>,
    /// The newest values taken from the triple buffer.
    latest: Vec<f32>,
    /// Counts new values; 0 = nothing was published yet.
    seq: u64,
}

pub struct DataShared {
    name: &'static str,
    len: usize,
    /// The instance's count of open editors (linked when the instance is created).
    editors: OnceLock<Arc<AtomicU32>>,
    state: Mutex<ReaderState>,
    dropped: AtomicU64,
}

impl DataShared {
    fn editor_open(&self) -> bool {
        self.editors.get().is_some_and(|e| e.load(Ordering::Acquire) > 0)
    }

    fn refresh(&self, st: &mut ReaderState) {
        if st.reader.has_new() {
            let v = st.reader.read();
            st.latest.clear();
            st.latest.extend_from_slice(v);
            st.seq += 1;
        }
    }
}

/// The audio thread's end of a data stream (`ctx.streams().<key>`).
pub struct StreamWriter {
    shared: Arc<DataShared>,
    w: UnsafeCell<SwapWriter<Box<[f32]>>>,
    /// Set while a `write` runs (see there).
    busy: AtomicBool,
}

/// Clears a writer's `busy` flag when the write ends, however it ends.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// SAFETY: writes go through `busy`, one at a time: a write that comes while another one runs is
// dropped. Everything else is atomics and a mutex.
unsafe impl Sync for StreamWriter {}
unsafe impl Send for StreamWriter {}

impl StreamWriter {
    /// A stream of `len` values (allocates; the generated `Streams` does this at instance creation).
    pub fn new(name: &'static str, len: usize) -> StreamWriter {
        let len = len.clamp(1, MAX_STREAM_LEN);
        let (w, r) = Swap::new(|| vec![0.0f32; len].into_boxed_slice());
        let shared = Arc::new(DataShared {
            name,
            len,
            editors: OnceLock::new(),
            state: Mutex::new(ReaderState { reader: r, latest: Vec::with_capacity(len), seq: 0 }),
            dropped: AtomicU64::new(0),
        });
        StreamWriter { shared, w: UnsafeCell::new(w), busy: AtomicBool::new(false) }
    }

    /// Publishes `data` as the newest values (at most `len` are taken; a
    /// shorter slice is padded with zeros). Returns `false` when it was
    /// dropped because no editor is open. Never waits, locks or allocates.
    /// Call it from the audio thread only.
    pub fn write(&self, data: &[f32]) -> bool {
        // Another thread is writing right now: this write is dropped, the buffer is not shared.
        if self.busy.swap(true, Ordering::Acquire) {
            return false;
        }
        let _busy = Busy(&self.busy);
        if !self.shared.editor_open() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: only one write runs at a time (`busy`), and only the audio thread writes (see the type's contract).
        let w = unsafe { &mut *self.w.get() };
        let back = w.back();
        let n = data.len().min(back.len());
        back[..n].copy_from_slice(&data[..n]);
        back[n..].fill(0.0);
        w.publish();
        true
    }

    pub fn len(&self) -> usize {
        self.shared.len
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether an editor is open (writes are kept only then).
    pub fn is_open(&self) -> bool {
        self.shared.editor_open()
    }

    /// How many `write` calls were dropped for lack of an editor.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    pub fn entry(&self) -> StreamEntry {
        StreamEntry::Data(self.shared.clone())
    }
}

/// An editor's (or a test's) end of a data stream: sees the newest values.
/// Several readers of one stream do not disturb each other.
pub struct StreamReader {
    shared: Arc<DataShared>,
    seen: u64,
}

impl StreamReader {
    pub fn name(&self) -> &'static str {
        self.shared.name
    }

    pub fn len(&self) -> usize {
        self.shared.len
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// If values were published since the last call of this reader, copies
    /// the newest into `out` (replacing its contents) and returns `true`.
    pub fn read_new(&mut self, out: &mut Vec<f32>) -> bool {
        let mut st = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.shared.refresh(&mut st);
        if st.seq == self.seen {
            return false;
        }
        self.seen = st.seq;
        out.clear();
        out.extend_from_slice(&st.latest);
        true
    }

    /// The newest values, or an empty vector when nothing was published yet.
    pub fn latest(&mut self) -> Vec<f32> {
        let mut st = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.shared.refresh(&mut st);
        self.seen = st.seq;
        st.latest.clone()
    }
}

pub struct TapShared {
    name: &'static str,
    cap: usize,
    buf: Box<[UnsafeCell<f32>]>,
    /// Samples written / read so far.
    head: CachePadded<AtomicU64>,
    tail: CachePadded<AtomicU64>,
    dropped: AtomicU64,
    taken: AtomicBool,
}

// SAFETY: one producer writes only the free part, one consumer reads only the filled part;
// the counters hand the regions over (release / acquire).
unsafe impl Sync for TapShared {}
unsafe impl Send for TapShared {}

/// The audio thread's end of a tap (`ctx.streams().<key>`).
pub struct TapWriter {
    shared: Arc<TapShared>,
    /// Set while a `write` runs (see there).
    busy: AtomicBool,
}

impl TapWriter {
    /// A ring of `len` samples (allocates).
    pub fn new(name: &'static str, len: usize) -> TapWriter {
        let cap = len.clamp(1, MAX_STREAM_LEN);
        TapWriter {
            busy: AtomicBool::new(false),
            shared: Arc::new(TapShared {
                name,
                cap,
                buf: (0..cap).map(|_| UnsafeCell::new(0.0)).collect(),
                head: CachePadded::new(AtomicU64::new(0)),
                tail: CachePadded::new(AtomicU64::new(0)),
                dropped: AtomicU64::new(0),
                taken: AtomicBool::new(false),
            }),
        }
    }

    /// Appends `data`. When the ring is full the samples that do not fit are
    /// dropped (and counted). Returns how many were stored. Never waits,
    /// locks or allocates. Call it from the audio thread only.
    pub fn write(&self, data: &[f32]) -> usize {
        // Another thread is writing right now: these samples are dropped (two writers would
        // corrupt the head and the ring).
        if self.busy.swap(true, Ordering::Acquire) {
            return 0;
        }
        let _busy = Busy(&self.busy);
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        let tail = s.tail.load(Ordering::Acquire);
        let free = s.cap - (head - tail) as usize;
        let n = data.len().min(free);
        if n < data.len() {
            s.dropped.fetch_add((data.len() - n) as u64, Ordering::Relaxed);
        }
        let start = (head % s.cap as u64) as usize;
        let first = n.min(s.cap - start);
        // SAFETY: [head, head + n) is free: the consumer only touches [tail, head).
        unsafe {
            let base = s.buf.as_ptr() as *mut f32;
            std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(start), first);
            std::ptr::copy_nonoverlapping(data.as_ptr().add(first), base, n - first);
        }
        s.head.store(head + n as u64, Ordering::Release);
        n
    }

    pub fn capacity(&self) -> usize {
        self.shared.cap
    }

    /// Samples dropped so far because the reader was too slow.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    pub fn entry(&self) -> StreamEntry {
        StreamEntry::Tap(self.shared.clone())
    }
}

/// A worker's end of a tap. There is one reader at a time
/// ([`InstanceHandle::tap`](crate::InstanceHandle::tap)).
pub struct TapReader {
    shared: Arc<TapShared>,
}

impl TapReader {
    pub fn name(&self) -> &'static str {
        self.shared.name
    }

    pub fn capacity(&self) -> usize {
        self.shared.cap
    }

    /// Samples waiting.
    pub fn available(&self) -> usize {
        (self.shared.head.load(Ordering::Acquire) - self.shared.tail.load(Ordering::Relaxed)) as usize
    }

    /// Moves up to `out.len()` samples (oldest first) into `out`; returns how many.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        let n = out.len().min((head - tail) as usize);
        let start = (tail % s.cap as u64) as usize;
        let first = n.min(s.cap - start);
        // SAFETY: [tail, tail + n) was filled by the producer (acquire above).
        unsafe {
            let base = s.buf.as_ptr() as *const f32;
            std::ptr::copy_nonoverlapping(base.add(start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(base, out.as_mut_ptr().add(first), n - first);
        }
        s.tail.store(tail + n as u64, Ordering::Release);
        n
    }

    /// Throws away everything but the newest `keep` samples (a reader that
    /// fell behind catches up). Returns how many were thrown away.
    pub fn skip_to_latest(&mut self, keep: usize) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        let avail = (head - tail) as usize;
        let skip = avail.saturating_sub(keep);
        s.tail.store(tail + skip as u64, Ordering::Release);
        skip
    }

    /// Samples the audio thread had to drop because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for TapReader {
    fn drop(&mut self) {
        self.shared.taken.store(false, Ordering::Release);
    }
}

/// One declared stream, as the instance sees it.
#[derive(Clone)]
pub enum StreamEntry {
    Data(Arc<DataShared>),
    Tap(Arc<TapShared>),
}

/// What kind of stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Data,
    Tap,
}

/// All streams of an instance (built by the generated `Streams`).
pub struct StreamHub {
    entries: Vec<StreamEntry>,
}

impl StreamHub {
    pub fn new(entries: Vec<StreamEntry>) -> StreamHub {
        StreamHub { entries }
    }

    /// `(name, len, kind)` of every stream.
    pub fn list(&self) -> Vec<(&'static str, usize, StreamKind)> {
        self.entries
            .iter()
            .map(|e| match e {
                StreamEntry::Data(d) => (d.name, d.len, StreamKind::Data),
                StreamEntry::Tap(t) => (t.name, t.cap, StreamKind::Tap),
            })
            .collect()
    }

    /// Ties the data streams' "editor open" to the instance's counter.
    pub(crate) fn link(&self, editors: &Arc<AtomicU32>) {
        for e in &self.entries {
            if let StreamEntry::Data(d) = e {
                let _ = d.editors.set(editors.clone());
            }
        }
    }

    pub(crate) fn reader(&self, name: &str) -> Option<StreamReader> {
        self.entries.iter().find_map(|e| match e {
            StreamEntry::Data(d) if d.name == name => Some(StreamReader { shared: d.clone(), seen: 0 }),
            _ => None,
        })
    }

    pub(crate) fn tap(&self, name: &str) -> Option<TapReader> {
        self.entries.iter().find_map(|e| match e {
            StreamEntry::Tap(t) if t.name == name => {
                t.taken.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| TapReader { shared: t.clone() })
            }
            _ => None,
        })
    }
}

/// The `Streams` of a plugin without `[streams]`.
#[derive(Default)]
pub struct NoStreams;

/// Implemented by the generated `Params`: gives `ctx.streams()` its type.
pub trait HasStreams {
    type Streams: Sync + 'static;
    fn streams(&self) -> &Self::Streams;
}

impl<P: HasStreams> crate::Context<'_, P> {
    /// The plugin's streams (`ctx.streams().spectrum.write(&data)`), declared under
    /// `[streams]` in the project file.
    pub fn streams(&self) -> &P::Streams {
        self.params.streams()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(1))
    }

    #[test]
    fn data_stream_drops_writes_while_no_editor_is_open_and_keeps_the_latest_after() {
        let w = StreamWriter::new("s", 4);
        let hub = StreamHub::new(vec![w.entry()]);
        let editors = Arc::new(AtomicU32::new(0));
        hub.link(&editors);
        let mut r = hub.reader("s").unwrap();
        assert!(!w.write(&[1.0; 4]), "no editor: dropped");
        assert_eq!(w.dropped(), 1);
        assert!(r.latest().is_empty());
        editors.store(1, Ordering::Release);
        assert!(w.write(&[1.0, 2.0]));
        assert!(w.write(&[3.0, 4.0, 5.0, 6.0, 7.0]));
        let mut out = Vec::new();
        assert!(r.read_new(&mut out));
        assert_eq!(out, vec![3.0, 4.0, 5.0, 6.0], "the newest, cut to len");
        assert!(!r.read_new(&mut out), "nothing new");
        w.write(&[9.0]);
        assert!(r.read_new(&mut out));
        assert_eq!(out, vec![9.0, 0.0, 0.0, 0.0], "padded with zeros");
    }

    #[test]
    fn two_readers_both_see_new_values() {
        let w = StreamWriter::new("s", 2);
        let hub = StreamHub::new(vec![w.entry()]);
        hub.link(&open());
        let (mut a, mut b) = (hub.reader("s").unwrap(), hub.reader("s").unwrap());
        w.write(&[1.0, 2.0]);
        let mut out = Vec::new();
        assert!(a.read_new(&mut out));
        assert!(b.read_new(&mut out), "the second reader is not starved by the first");
        assert!(hub.reader("nope").is_none());
    }

    #[test]
    fn data_stream_write_does_not_allocate_or_block_across_threads() {
        let w = Arc::new(StreamWriter::new("s", 64));
        let hub = StreamHub::new(vec![w.entry()]);
        hub.link(&open());
        let mut r = hub.reader("s").unwrap();
        let w2 = w.clone();
        let t = std::thread::spawn(move || {
            for n in 1..=5000u32 {
                w2.write(&[n as f32; 64]);
            }
        });
        let mut out = Vec::new();
        let mut last = 0.0;
        while last < 5000.0 {
            if r.read_new(&mut out) {
                assert!(out.iter().all(|v| *v == out[0]), "torn");
                assert!(out[0] >= last);
                last = out[0];
            }
        }
        t.join().unwrap();
    }

    #[test]
    fn tap_keeps_order_wraps_and_counts_drops() {
        let w = TapWriter::new("t", 8);
        let hub = StreamHub::new(vec![w.entry()]);
        let mut r = hub.tap("t").unwrap();
        assert!(hub.tap("t").is_none(), "one reader at a time");
        assert_eq!(w.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);
        let mut out = [0.0; 4];
        assert_eq!(r.read(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        // 2 left, 6 free: this wraps around the end
        assert_eq!(w.write(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0]), 6);
        assert_eq!(w.dropped(), 1);
        assert_eq!(r.available(), 8);
        let mut all = [0.0; 8];
        assert_eq!(r.read(&mut all), 8);
        assert_eq!(all, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);
        assert_eq!(r.read(&mut all), 0);
        drop(r);
        assert!(hub.tap("t").is_some(), "the reader can be taken again after it is dropped");
    }

    #[test]
    fn tap_skip_to_latest_catches_up() {
        let w = TapWriter::new("t", 16);
        let hub = StreamHub::new(vec![w.entry()]);
        let mut r = hub.tap("t").unwrap();
        w.write(&(0..10).map(|i| i as f32).collect::<Vec<_>>());
        assert_eq!(r.skip_to_latest(3), 7);
        let mut out = [0.0; 8];
        assert_eq!(r.read(&mut out), 3);
        assert_eq!(&out[..3], &[7.0, 8.0, 9.0]);
    }

    #[test]
    fn tap_passes_every_sample_in_order_between_threads() {
        let w = TapWriter::new("t", 256);
        let hub = StreamHub::new(vec![w.entry()]);
        let mut r = hub.tap("t").unwrap();
        let t = std::thread::spawn(move || {
            let mut n = 0u32;
            while n < 100_000 {
                let chunk: Vec<f32> = (n..n + 50).map(|v| v as f32).collect();
                let stored = w.write(&chunk);
                n += stored as u32;
                if stored == 0 {
                    std::thread::yield_now();
                }
            }
        });
        let mut expect = 0u32;
        let mut buf = [0.0f32; 64];
        while expect < 100_000 {
            let n = r.read(&mut buf);
            for v in &buf[..n] {
                // partial writes may split a chunk: values stay consecutive
                assert_eq!(*v as u32, expect);
                expect += 1;
            }
        }
        t.join().unwrap();
    }

    /// A6: two threads writing a data stream at once: a write that overlaps another is dropped,
    /// so every value the reader takes is a whole write of one of them.
    #[test]
    fn two_stream_writers_at_once_never_tear_the_values() {
        let w = Arc::new(StreamWriter::new("s", 8));
        let hub = StreamHub::new(vec![w.entry()]);
        let editors = open();
        hub.link(&editors);
        let mut r = hub.reader("s").unwrap();
        let writers: Vec<_> = [1.0f32, 2.0]
            .into_iter()
            .map(|v| {
                let w = w.clone();
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        w.write(&[v; 8]);
                    }
                })
            })
            .collect();
        let mut out = Vec::new();
        while !writers.iter().all(|t| t.is_finished()) {
            if r.read_new(&mut out) {
                assert!(out.iter().all(|v| *v == out[0]), "torn: {out:?}");
                assert!(out[0] == 1.0 || out[0] == 2.0, "{out:?}");
            }
        }
        for t in writers {
            t.join().unwrap();
        }
    }

    /// A6: two threads writing a tap at once: samples of an overlapping write are dropped, the
    /// ring is never corrupted (no zeros or other values come out).
    #[test]
    fn two_tap_writers_at_once_never_corrupt_the_ring() {
        let w = Arc::new(TapWriter::new("t", 64));
        let hub = StreamHub::new(vec![w.entry()]);
        let mut r = hub.tap("t").unwrap();
        let writers: Vec<_> = [1.0f32, 2.0]
            .into_iter()
            .map(|v| {
                let w = w.clone();
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        w.write(&[v; 8]);
                    }
                })
            })
            .collect();
        let mut buf = [0.0f32; 64];
        while !writers.iter().all(|t| t.is_finished()) {
            let n = r.read(&mut buf);
            assert!(buf[..n].iter().all(|v| *v == 1.0 || *v == 2.0), "{:?}", &buf[..n]);
        }
        for t in writers {
            t.join().unwrap();
        }
    }
}
