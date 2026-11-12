//! Small DSP helpers.

/// Converts decibels to a linear gain factor.
pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Converts a linear gain factor to decibels.
pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(f32::MIN_POSITIVE).log10()
}

/// Keeps `T` on its own pair of cache lines (128 bytes: x86 prefetches the
/// neighbouring line), so a value the audio thread writes does not share a
/// line with one the UI thread writes (false sharing).
#[repr(align(128))]
#[derive(Default)]
pub struct CachePadded<T>(pub T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

struct SwapInner<T> {
    slots: [std::cell::UnsafeCell<T>; 3],
    /// Index of the middle slot, with `DIRTY` set when the writer published into it.
    middle: std::sync::atomic::AtomicU8,
}

const DIRTY: u8 = 4;

// SAFETY: the writer owns one slot, the reader another, the middle one is handed over by an atomic swap.
unsafe impl<T: Send> Sync for SwapInner<T> {}
unsafe impl<T: Send> Send for SwapInner<T> {}

/// A triple buffer for one writer and one reader on different threads, for
/// handing results from a worker thread to the audio thread (or audio
/// analysis to an editor). Three values are allocated up front; neither side
/// ever waits, locks or allocates, and the reader always sees the latest
/// complete value the writer published.
pub struct Swap;

impl Swap {
    /// Makes the pair; `make` builds the three values (allocate here, not later).
    pub fn new<T: Send>(mut make: impl FnMut() -> T) -> (SwapWriter<T>, SwapReader<T>) {
        let inner = std::sync::Arc::new(SwapInner {
            slots: [std::cell::UnsafeCell::new(make()), std::cell::UnsafeCell::new(make()), std::cell::UnsafeCell::new(make())],
            middle: std::sync::atomic::AtomicU8::new(1),
        });
        (SwapWriter { inner: inner.clone(), back: 0 }, SwapReader { inner, front: 2 })
    }
}

pub struct SwapWriter<T> {
    inner: std::sync::Arc<SwapInner<T>>,
    back: u8,
}

impl<T> SwapWriter<T> {
    /// The value being prepared (not visible to the reader until `publish`).
    pub fn back(&mut self) -> &mut T {
        // SAFETY: the writer owns the back slot.
        unsafe { &mut *self.inner.slots[self.back as usize].get() }
    }

    /// Makes the back value the one the reader sees next; the writer then
    /// gets a recycled value to overwrite (it holds old contents).
    pub fn publish(&mut self) {
        let old = self.inner.middle.swap(self.back | DIRTY, std::sync::atomic::Ordering::AcqRel);
        self.back = old & 3;
    }
}

pub struct SwapReader<T> {
    inner: std::sync::Arc<SwapInner<T>>,
    front: u8,
}

impl<T> SwapReader<T> {
    /// The latest published value (the previous one if nothing new came). Never waits.
    pub fn read(&mut self) -> &T {
        use std::sync::atomic::Ordering::*;
        if self.inner.middle.load(Acquire) & DIRTY != 0 {
            let old = self.inner.middle.swap(self.front, AcqRel);
            self.front = old & 3;
        }
        // SAFETY: the reader owns the front slot.
        unsafe { &*self.inner.slots[self.front as usize].get() }
    }

    /// Whether the writer published since the last `read`.
    pub fn has_new(&self) -> bool {
        self.inner.middle.load(std::sync::atomic::Ordering::Acquire) & DIRTY != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_hands_over_the_latest_complete_value_across_threads() {
        let (mut w, mut r) = Swap::new(|| [0u32; 64]);
        let t = std::thread::spawn(move || {
            for n in 1..=20_000u32 {
                w.back().fill(n);
                w.publish();
            }
        });
        let mut last = 0;
        while last < 20_000 {
            let v = r.read();
            assert!(v.iter().all(|&x| x == v[0]), "a torn value: {v:?}");
            assert!(v[0] >= last, "went back in time");
            last = v[0];
        }
        t.join().unwrap();
        assert!(!r.has_new());
    }

    #[test]
    fn cache_padded_values_sit_a_cache_line_pair_apart() {
        struct Two(CachePadded<u8>, CachePadded<u8>);
        let t = Two(CachePadded::new(1), CachePadded::new(2));
        let (a, b) = (&*t.0 as *const u8 as usize, &*t.1 as *const u8 as usize);
        assert!(b.abs_diff(a) >= 128);
    }

    #[test]
    fn db_roundtrip() {
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert!((gain_to_db(db_to_gain(-12.0)) + 12.0).abs() < 1e-4);
    }
}
