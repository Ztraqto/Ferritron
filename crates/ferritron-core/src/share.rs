//! Resource sharing (design doc, Phase 7): a plugin on many tracks, or
//! several plugins of a vendor, need the same impulse responses, wavetables,
//! sample data or lookup tables. Building them once and sharing them saves
//! memory and load time.
//!
//! - [`cached`]: any value, built once per key in this plugin library and
//!   shared by its instances while any of them holds it.
//! - [`bytes`]: read-only data in named shared memory, built once and
//!   shared by every plugin library and every process of the session (two
//!   DAWs, or a DAW's plugin sandboxes, use the same physical memory). The
//!   OS frees it with the last user.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

type Cache = HashMap<(TypeId, String), Weak<dyn Any + Send + Sync>>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The value for `key`, built by `build` if no instance holds it now. Call
/// off the audio thread (it may build and allocate).
pub fn cached<T: Send + Sync + 'static>(key: &str, build: impl FnOnce() -> T) -> Arc<T> {
    let id = (TypeId::of::<T>(), key.to_string());
    {
        let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = cache.as_ref().and_then(|c| c.get(&id)).and_then(Weak::upgrade)
            && let Ok(value) = found.downcast::<T>() {
                return value;
            }
    }
    // Built outside the lock: building may take long, and may share too.
    let value = Arc::new(build());
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(HashMap::new);
    // Another instance may have built it meanwhile: keep the first.
    if let Some(found) = cache.get(&id).and_then(Weak::upgrade)
        && let Ok(existing) = found.downcast::<T>() {
            return existing;
        }
    cache.retain(|_, weak| weak.strong_count() > 0);
    let shared: Arc<dyn Any + Send + Sync> = value.clone();
    cache.insert(id, Arc::downgrade(&shared));
    value
}

/// Read-only bytes shared by every library and process of the session.
/// Derefs to `[u8]`.
pub struct SharedBytes {
    _meta: Option<crate::shmem::Mapping>,
    data: Data,
}

enum Data {
    Mapped(crate::shmem::Mapping, usize),
    /// When shared memory is not available: a private copy.
    Private(Vec<u8>),
}

impl std::ops::Deref for SharedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.data {
            Data::Mapped(m, len) => unsafe { std::slice::from_raw_parts(m.data(), *len) },
            Data::Private(v) => v,
        }
    }
}

impl SharedBytes {
    /// The bytes live in shared memory (not a private fallback copy).
    pub fn is_shared(&self) -> bool {
        matches!(self.data, Data::Mapped(..))
    }
}

/// Coordinates who builds: in a small region of its own.
#[repr(C)]
struct Meta {
    state: AtomicU32, // 0 = empty, 1 = building, 2 = ready
    _pad: u32,
    len: AtomicU64,
}

const EMPTY: u32 = 0;
const BUILDING: u32 = 1;
const READY: u32 = 2;
/// How long to wait for another process that is building the same data.
const WAIT: Duration = Duration::from_secs(20);

/// The bytes named `name` (a key unique to the data, e.g. `"<plugin id>/ir/hall-2s"`),
/// built once by `build` for the whole session. Call off the audio thread.
pub fn bytes(name: &str, build: impl FnOnce() -> Vec<u8>) -> SharedBytes {
    let meta_name = crate::shmem::name_for("share-meta", name);
    let Ok(meta) = crate::shmem::Mapping::open(&meta_name, size_of::<Meta>()) else {
        return SharedBytes { _meta: None, data: Data::Private(build()) };
    };
    let m = unsafe { &*(meta.data() as *const Meta) };
    let data_name = crate::shmem::name_for("share-data", name);
    // First one: build and publish. Others: wait until ready.
    if m.state.compare_exchange(EMPTY, BUILDING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let bytes = build();
        match crate::shmem::Mapping::open(&data_name, bytes.len()) {
            Ok(data) => {
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.data(), bytes.len()) };
                m.len.store(bytes.len() as u64, Ordering::Release);
                m.state.store(READY, Ordering::Release);
                let len = bytes.len();
                return SharedBytes { _meta: Some(meta), data: Data::Mapped(data, len) };
            }
            Err(_) => {
                m.state.store(EMPTY, Ordering::Release);
                return SharedBytes { _meta: Some(meta), data: Data::Private(bytes) };
            }
        }
    }
    let start = Instant::now();
    while m.state.load(Ordering::Acquire) != READY {
        if start.elapsed() > WAIT {
            // The builder died or hangs: use a private copy.
            return SharedBytes { _meta: Some(meta), data: Data::Private(build()) };
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let len = m.len.load(Ordering::Acquire) as usize;
    match crate::shmem::Mapping::open(&data_name, len) {
        Ok(data) => SharedBytes { _meta: Some(meta), data: Data::Mapped(data, len) },
        Err(_) => SharedBytes { _meta: Some(meta), data: Data::Private(build()) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_values_are_built_once_while_held() {
        let builds = AtomicU32::new(0);
        let build = || {
            builds.fetch_add(1, Ordering::Relaxed);
            vec![1.0f32; 1000]
        };
        let a = cached("test/table", build);
        let b = cached("test/table", build);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(builds.load(Ordering::Relaxed), 1);
        // A different type under the same key is another value.
        let c = cached("test/table", || 7u32);
        assert_eq!(*c, 7);
        drop((a, b));
        // Nobody holds it: built again.
        let _d = cached("test/table", build);
        assert_eq!(builds.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn shared_bytes_are_built_once_for_the_session() {
        let name = format!("ferritron-test/{}/{:?}", std::process::id(), Instant::now());
        let first = bytes(&name, || (0..=255u8).collect());
        assert!(first.is_shared(), "named shared memory should be available");
        // Another user (another library or process) maps the same data.
        let second = bytes(&name, || panic!("must not build again"));
        assert_eq!(&first[..], &second[..]);
        assert_eq!(second.len(), 256);
        assert_eq!(second[200], 200);
    }
}
