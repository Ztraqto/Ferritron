//! Small caches: [`Cached`] keeps the last result of a pure function of a key (filter
//! coefficients for the last frequency/gain/Q), [`shared`] builds an immutable table once
//! per process and hands the same `Arc` to every instance.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock};

/// Remembers the last `(key, value)`. The value is rebuilt only when the key changes.
/// Allocation happens only inside the builder closure the caller passes, so keep keys and
/// values `Copy`-like on the audio thread.
#[derive(Clone, Debug)]
pub struct Cached<K, V> {
    slot: Option<(K, V)>,
}

impl<K, V> Default for Cached<K, V> {
    fn default() -> Self {
        Cached { slot: None }
    }
}

impl<K: PartialEq + Clone, V> Cached<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The value for `key`, calling `build(key)` only when `key` differs from the last one.
    pub fn get_or_insert_with(&mut self, key: &K, build: impl FnOnce(&K) -> V) -> &V {
        let hit = matches!(&self.slot, Some((k, _)) if k == key);
        if !hit {
            self.slot = Some((key.clone(), build(key)));
        }
        &self.slot.as_ref().unwrap().1
    }

    /// The cached value when the key matches, without building.
    pub fn peek(&self, key: &K) -> Option<&V> {
        match &self.slot {
            Some((k, v)) if k == key => Some(v),
            _ => None,
        }
    }

    pub fn invalidate(&mut self) {
        self.slot = None;
    }
}

type Cell<T> = Arc<OnceLock<Arc<T>>>;
type Registry = Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>;

/// The one table of type `T` for `key` in this process: the first caller's `build` runs (once,
/// also under concurrent calls), everyone gets a clone of the same `Arc`. Tables stay alive
/// until the process ends. Call it from `activate`, never from `process`: it takes a lock.
/// `build` may itself call `shared` for other keys.
pub fn shared<K, T>(key: K, build: impl FnOnce() -> T) -> Arc<T>
where
    K: Hash + Eq + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let cell: Cell<T> = {
        let mut map = registry.lock().unwrap_or_else(|e| e.into_inner());
        let per_type = map
            .entry(TypeId::of::<(K, T)>())
            .or_insert_with(|| Box::new(HashMap::<K, Cell<T>>::new()))
            .downcast_mut::<HashMap<K, Cell<T>>>()
            .expect("registry entries are keyed by their own type");
        per_type.entry(key).or_default().clone()
    };
    cell.get_or_init(|| Arc::new(build())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn cached_rebuilds_only_on_a_new_key() {
        let mut calls = 0;
        let mut c: Cached<(u32, u32), u64> = Cached::new();
        assert!(c.peek(&(1, 2)).is_none());
        for _ in 0..5 {
            let v = *c.get_or_insert_with(&(1, 2), |k| {
                calls += 1;
                (k.0 + k.1) as u64
            });
            assert_eq!(v, 3);
        }
        assert_eq!(calls, 1);
        assert_eq!(c.peek(&(1, 2)), Some(&3));
        let v = *c.get_or_insert_with(&(5, 5), |k| {
            calls += 1;
            (k.0 + k.1) as u64
        });
        assert_eq!((v, calls), (10, 2));
        assert!(c.peek(&(1, 2)).is_none());
        c.invalidate();
        c.get_or_insert_with(&(5, 5), |_| {
            calls += 1;
            0
        });
        assert_eq!(calls, 3);
    }

    #[test]
    fn same_key_same_arc_built_once() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let a = shared(44_100u32, || {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            vec![1.0f32, 2.0, 3.0]
        });
        let b = shared(44_100u32, || {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            vec![9.0f32]
        });
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*b, vec![1.0, 2.0, 3.0]);
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
        let c = shared(48_000u32, || vec![0.0f32]);
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn different_value_types_do_not_collide() {
        let a = shared("k", || 7u32);
        let b = shared("k", || String::from("seven"));
        assert_eq!((*a, b.as_str()), (7, "seven"));
    }

    #[test]
    fn built_once_under_concurrency_and_nesting() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    shared(("race", 1u8), || {
                        BUILDS.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        // building may use other shared tables
                        let inner = shared(("inner", 1u8), || 5usize);
                        *inner + 1
                    })
                })
            })
            .collect();
        let arcs: Vec<Arc<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
        assert!(arcs.iter().all(|a| Arc::ptr_eq(a, &arcs[0]) && **a == 6));
    }
}
