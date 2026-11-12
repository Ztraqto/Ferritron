//! Settings kept across projects and sessions (R4): language, theme, editor
//! size, folders. Not part of an instance's state (that is `InstanceHandle::store`).
//!
//! ```ignore
//! let settings = ferritron::settings::<MyPlugin>();
//! let zoom = settings.get("zoom").and_then(|v| v.as_f64()).unwrap_or(1.0);
//! settings.set("zoom", 1.5);
//! let _watch = settings.on_change(|key, value| println!("{key} = {value:?}"));
//! ```
//!
//! Values are JSON values, stored in one file per plugin ID:
//! `<Ferritron's data folder>/settings/<plugin id>.json` (`%APPDATA%\Ferritron` on
//! Windows, `~/Library/Application Support/Ferritron` on macOS,
//! `$XDG_DATA_HOME/ferritron` elsewhere; `FERRITRON_SETTINGS_DIR` replaces the
//! `settings` folder). The plugin ID, not its name, names the file, so renaming
//! a plugin keeps its settings.
//!
//! Several instances and processes may write at once: each change reads the
//! file, changes one key and writes it back under a lock file, so changes to
//! different keys are never lost. Changes made in this process reach every
//! `on_change` callback of the plugin at once; changes made by another process
//! are seen at the next `get`, `keys` or `refresh`, which then call the
//! callbacks for the keys that differ.
//!
//! Not for the audio thread: reading and writing touch the file system.

pub use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

type Callback = dyn Fn(&str, Option<&Value>) + Send + Sync;

/// What this process knows about one plugin's settings file.
struct Shared {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    values: Map<String, Value>,
    /// When the file was last read or written (None: not read yet).
    seen: Option<SystemTime>,
    callbacks: Vec<(u64, Arc<Callback>)>,
    next_callback: u64,
}

static OPEN: Mutex<Option<HashMap<String, Weak<Shared>>>> = Mutex::new(None);

/// The settings of one plugin. Cheap to clone; all handles of a plugin in
/// one process share their values and callbacks.
#[derive(Clone)]
pub struct Settings(Arc<Shared>);

/// Keeps an `on_change` callback registered; dropping it removes the callback.
#[must_use = "the callback is removed when this is dropped"]
pub struct Watch {
    shared: Weak<Shared>,
    id: u64,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.lock().callbacks.retain(|(id, _)| *id != self.id);
        }
    }
}

/// The settings of plugin type `P`.
pub fn settings<P: crate::__private::PluginMeta>() -> Settings {
    settings_for(P::INFO.id)
}

/// The settings of the plugin with this ID (`[plugin] id`).
pub fn settings_for(plugin_id: &str) -> Settings {
    let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    let open = open.get_or_insert_with(HashMap::new);
    if let Some(shared) = open.get(plugin_id).and_then(Weak::upgrade) {
        return Settings(shared);
    }
    let shared = Arc::new(Shared { path: file_for(plugin_id), inner: Mutex::new(Inner::default()) });
    open.retain(|_, w| w.strong_count() > 0);
    open.insert(plugin_id.to_string(), Arc::downgrade(&shared));
    Settings(shared)
}

/// The folder that holds the settings files.
pub fn settings_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FERRITRON_SETTINGS_DIR") {
        return Some(PathBuf::from(dir));
    }
    crate::i18n::data_dir().map(|d| d.join("settings"))
}

fn file_for(plugin_id: &str) -> Option<PathBuf> {
    // IDs are reverse-domain names; keep anything else out of the path.
    let name: String = plugin_id.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    Some(settings_dir()?.join(format!("{name}.json")))
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Settings {
    /// The file the settings live in (None when there is no data folder).
    pub fn path(&self) -> Option<&Path> {
        self.0.path.as_deref()
    }

    /// The value of `key`, if set.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.refresh();
        self.0.lock().values.get(key).cloned()
    }

    /// The value of `key`, or `fallback`.
    pub fn get_or(&self, key: &str, fallback: impl Into<Value>) -> Value {
        self.get(key).unwrap_or_else(|| fallback.into())
    }

    /// All keys that have a value, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.refresh();
        self.0.lock().values.keys().cloned().collect()
    }

    /// Sets `key` and writes the file. Returns false when the file could not
    /// be written (the value is still kept for this process).
    pub fn set(&self, key: &str, value: impl Into<Value>) -> bool {
        self.change(key, Some(value.into()))
    }

    /// Removes `key`.
    pub fn remove(&self, key: &str) -> bool {
        self.change(key, None)
    }

    /// Calls `callback(key, new value)` whenever a key changes (`None`: removed).
    pub fn on_change(&self, callback: impl Fn(&str, Option<&Value>) + Send + Sync + 'static) -> Watch {
        let mut inner = self.0.lock();
        inner.next_callback += 1;
        let id = inner.next_callback;
        inner.callbacks.push((id, Arc::new(callback)));
        Watch { shared: Arc::downgrade(&self.0), id }
    }

    /// Reads the file again if another process changed it, and tells the
    /// callbacks about the keys that differ.
    pub fn refresh(&self) {
        let Some(path) = &self.0.path else { return };
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut inner = self.0.lock();
        if inner.seen.is_some() && inner.seen == modified {
            return;
        }
        let first = inner.seen.is_none();
        let fresh = read(path);
        inner.seen = modified.or(Some(SystemTime::UNIX_EPOCH));
        if first {
            inner.values = fresh;
            return;
        }
        let changes = diff(&inner.values, &fresh);
        inner.values = fresh;
        let callbacks: Vec<_> = inner.callbacks.iter().map(|(_, c)| c.clone()).collect();
        drop(inner);
        for (key, value) in &changes {
            for c in &callbacks {
                c(key, value.as_ref());
            }
        }
    }

    fn change(&self, key: &str, value: Option<Value>) -> bool {
        let written = match &self.0.path {
            Some(path) => write_key(path, key, value.as_ref()),
            None => None,
        };
        let mut inner = self.0.lock();
        let ok = written.is_some();
        if let Some((values, modified)) = written {
            // Other processes' changes to other keys arrive with this write.
            let mut changes = diff(&inner.values, &values);
            changes.retain(|(k, _)| k != key);
            inner.values = values;
            inner.seen = modified;
            changes.push((key.to_string(), value.clone()));
            let callbacks: Vec<_> = inner.callbacks.iter().map(|(_, c)| c.clone()).collect();
            drop(inner);
            for (k, v) in &changes {
                for c in &callbacks {
                    c(k, v.as_ref());
                }
            }
        } else {
            match &value {
                Some(v) => inner.values.insert(key.to_string(), v.clone()),
                None => inner.values.remove(key),
            };
            let callbacks: Vec<_> = inner.callbacks.iter().map(|(_, c)| c.clone()).collect();
            drop(inner);
            for c in &callbacks {
                c(key, value.as_ref());
            }
        }
        ok
    }
}

/// The keys whose values differ between `old` and `new`, with the new value.
fn diff(old: &Map<String, Value>, new: &Map<String, Value>) -> Vec<(String, Option<Value>)> {
    let mut out: Vec<(String, Option<Value>)> =
        new.iter().filter(|(k, v)| old.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    out.extend(old.keys().filter(|k| !new.contains_key(*k)).map(|k| (k.clone(), None)));
    out
}

/// What a settings file holds.
enum Read {
    Missing,
    Ok(Map<String, Value>),
    /// The file is there but is not a JSON object.
    Damaged,
}

fn read_state(path: &Path) -> Read {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Read::Ok(map),
            _ => Read::Damaged,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Read::Missing,
        Err(_) => Read::Damaged,
    }
}

fn read(path: &Path) -> Map<String, Value> {
    match read_state(path) {
        Read::Ok(map) => map,
        Read::Missing | Read::Damaged => Map::new(),
    }
}

/// A lock file next to the settings file, held while one process changes it.
struct FileLock(PathBuf);

impl FileLock {
    fn take(path: &Path) -> Option<Self> {
        let lock = path.with_extension("json.lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Some(Self(lock)),
                // Windows answers "access denied" for a lock file that is being deleted.
                Err(e) if matches!(e.kind(), std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied) => {
                    // A lock older than a few seconds was left by a process that died.
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(5));
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                    } else if start.elapsed() > Duration::from_secs(2) {
                        return None;
                    } else {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Reads the file, sets or removes one key, and writes it back (through a
/// temporary file, so readers never see half a file).
fn write_key(path: &Path, key: &str, value: Option<&Value>) -> Option<(Map<String, Value>, Option<SystemTime>)> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let _lock = FileLock::take(path)?;
    let mut values = match read_state(path) {
        Read::Ok(map) => map,
        Read::Missing => Map::new(),
        Read::Damaged => {
            // Keep the unreadable file for the user; if it cannot be kept, do not overwrite it.
            let stamp = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            std::fs::copy(path, path.with_extension(format!("json.damaged-{stamp}"))).ok()?;
            Map::new()
        }
    };
    match value {
        Some(v) => values.insert(key.to_string(), v.clone()),
        None => values.remove(key),
    };
    let text = serde_json::to_string_pretty(&Value::Object(values.clone())).ok()?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, text).ok()?;
    // A reader in another process may hold the file open for a moment (Windows).
    let start = Instant::now();
    while std::fs::rename(&temp, path).is_err() {
        if start.elapsed() > Duration::from_secs(1) {
            let _ = std::fs::remove_file(&temp);
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    Some((values, modified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferritron-settings-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(dir: &Path, id: &str) -> Settings {
        Settings(Arc::new(Shared { path: Some(dir.join(format!("{id}.json"))), inner: Mutex::new(Inner::default()) }))
    }

    #[test]
    fn a_damaged_file_is_kept_before_it_is_written_again() {
        let dir = temp_dir("damaged");
        std::fs::write(dir.join("p.json"), "{not json").unwrap();
        assert!(at(&dir, "p").set("a", 1));
        let kept: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".damaged-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "{not json");
    }

    #[test]
    fn values_are_kept_in_a_file() {
        let dir = temp_dir("kept");
        let s = at(&dir, "dev.ferritron.tests.a");
        assert_eq!(s.get("zoom"), None);
        assert!(s.set("zoom", 1.5));
        assert!(s.set("theme", "dark"));
        assert_eq!(s.get_or("zoom", 1.0), Value::from(1.5));
        assert_eq!(s.keys(), vec!["theme".to_string(), "zoom".to_string()]);
        // A new reader (another process) sees them.
        let again = at(&dir, "dev.ferritron.tests.a");
        assert_eq!(again.get("theme"), Some(Value::from("dark")));
        assert!(again.remove("theme"));
        assert_eq!(again.get("theme"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writers_do_not_lose_each_others_keys() {
        let dir = temp_dir("writers");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    // Separate handles: like separate processes, no shared cache.
                    let s = at(&dir, "dev.ferritron.tests.b");
                    for j in 0..10 {
                        assert!(s.set(&format!("k{i}-{j}"), i * 100 + j));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(at(&dir, "dev.ferritron.tests.b").keys().len(), 80);
        assert!(!dir.join("dev.ferritron.tests.b.json.lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn callbacks_hear_own_and_other_changes() {
        let dir = temp_dir("callbacks");
        let a = at(&dir, "dev.ferritron.tests.c");
        a.refresh();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        let watch = a.on_change(move |k, v| h.lock().unwrap().push((k.to_string(), v.cloned())));
        a.set("size", 2);
        a.remove("size");
        // Another process writes; the next read here notices.
        std::thread::sleep(Duration::from_millis(20));
        at(&dir, "dev.ferritron.tests.c").set("language", "ja");
        assert_eq!(a.get("language"), Some(Value::from("ja")));
        assert_eq!(
            *heard.lock().unwrap(),
            vec![("size".into(), Some(Value::from(2))), ("size".into(), None), ("language".into(), Some(Value::from("ja")))]
        );
        drop(watch);
        a.set("size", 3);
        assert_eq!(heard.lock().unwrap().len(), 3, "a dropped watch hears nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handles_of_one_plugin_share_values_and_callbacks() {
        let a = settings_for("dev.ferritron.tests.shared-handles");
        let b = settings_for("dev.ferritron.tests.shared-handles");
        assert!(Arc::ptr_eq(&a.0, &b.0));
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _w = a.on_change(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        // Without touching the user's data folder: a handle with no file keeps values in memory.
        let mem = Settings(Arc::new(Shared { path: None, inner: Mutex::new(Inner::default()) }));
        assert!(!mem.set("x", 1));
        assert_eq!(mem.get("x"), Some(Value::from(1)));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(file_for("a/b\\c").unwrap().file_name().unwrap().to_str().unwrap() == "a_b_c.json");
    }
}
