//! Watching instances and their editors (E4, E5).
//!
//! **[`instances_watch`]** calls a function whenever an instance in the process is created or
//! destroyed, its identity, track or store changes, its parameters move, or one of its editors
//! opens, closes or gets the focus. One Ferritron thread (`ferritron-watch`) serves every watch,
//! so an editor that shows several instances needs no thread of its own:
//!
//! ```ignore
//! let redraw = ui.redraw_handle();
//! let watch = ferritron::instances_watch(move |event| {
//!     if event.plugin_id() == MY_ID { redraw.request(); }
//! });
//! // keep `watch`; dropping it stops the calls
//! ```
//!
//! Structural events (created, destroyed, changed, editors) arrive at once; parameter moves are
//! looked for every 30 ms while a watch exists (they come from the audio thread, which cannot
//! wake anything), and several moves in that time make one event.
//!
//! **[`editors`]** lists the instances of a plugin whose editor is open, in the order they
//! opened; [`SingletonView`] picks one of them to do the work every editor would otherwise
//! repeat (showing all instances, asking for analysis), hands it to the next editor when that
//! one closes, and lets the user move it.

use crate::instance::{InstanceHandle, InstanceId};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// What happened to an instance.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum InstanceEvent {
    /// An instance was created.
    Added(InstanceHandle),
    /// An instance was destroyed.
    Removed { id: InstanceId, plugin_id: &'static str },
    /// The instance's identity, track or store changed.
    Changed(InstanceHandle),
    /// Parameter values of the instance moved (the host, automation, the editor, the plugin).
    Params(InstanceHandle),
    /// An editor of the instance opened.
    EditorOpened(InstanceHandle),
    /// The instance's last open editor closed.
    EditorClosed(InstanceHandle),
    /// An editor of the instance got the keyboard focus (the user is working in it).
    EditorFocused(InstanceHandle),
}

impl InstanceEvent {
    /// The instance is hidden (the controller copy of `[vst3] controller_id`): no watch hears of it,
    /// except its `Removed` event.
    fn hidden(&self) -> bool {
        match self {
            Self::Removed { .. } => false,
            Self::Added(h) | Self::Changed(h) | Self::Params(h) | Self::EditorOpened(h) | Self::EditorClosed(h) | Self::EditorFocused(h) => h.is_hidden(),
        }
    }

    /// The plugin's `id` from the project file.
    pub fn plugin_id(&self) -> &'static str {
        match self {
            Self::Removed { plugin_id, .. } => plugin_id,
            Self::Added(h) | Self::Changed(h) | Self::Params(h) | Self::EditorOpened(h) | Self::EditorClosed(h) | Self::EditorFocused(h) => h.plugin_id(),
        }
    }

    /// The instance's identity.
    pub fn id(&self) -> InstanceId {
        match self {
            Self::Removed { id, .. } => *id,
            Self::Added(h) | Self::Changed(h) | Self::Params(h) | Self::EditorOpened(h) | Self::EditorClosed(h) | Self::EditorFocused(h) => h.id(),
        }
    }
}

type Callback = Box<dyn FnMut(&InstanceEvent) + Send>;

struct Hub {
    watchers: Mutex<Vec<(u64, Callback)>>,
    queue: Mutex<VecDeque<InstanceEvent>>,
    wake: Condvar,
}

static HUB: Hub = Hub { watchers: Mutex::new(Vec::new()), queue: Mutex::new(VecDeque::new()), wake: Condvar::new() };
/// Watches alive; events are queued only while there is one.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether any watch exists.
pub(crate) fn any() -> bool {
    ACTIVE.load(Ordering::Acquire) > 0
}

/// Queues an event for the watches (any thread except the audio thread).
pub(crate) fn emit(event: impl FnOnce() -> InstanceEvent) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    lock(&HUB.queue).push_back(event());
    HUB.wake.notify_one();
}

/// A running watch; dropping it stops the calls (also from inside the callback).
#[must_use = "dropping the Watch stops it"]
pub struct Watch(u64);

impl Drop for Watch {
    fn drop(&mut self) {
        let id = self.0;
        // The watch thread holds the list while it calls: a callback that drops a watch is
        // removed by the thread afterwards (see `serve`). Any other thread waits for the list,
        // so no call follows the drop.
        if std::thread::current().name() == Some("ferritron-watch") {
            lock(&DROPPED).push(id);
        } else {
            lock(&HUB.watchers).retain(|(i, _)| *i != id);
        }
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

static DROPPED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Calls `f` on Ferritron's watch thread for every [`InstanceEvent`] in the process, until the
/// returned [`Watch`] is dropped. `f` must not block for long (it delays the other watches).
pub fn instances_watch(f: impl FnMut(&InstanceEvent) + Send + 'static) -> Watch {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock(&HUB.watchers).push((id, Box::new(f)));
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    start();
    Watch(id)
}

fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // No threads on WebAssembly (the guest), and no module to pin there either.
        #[cfg(not(target_family = "wasm"))]
        {
            // The thread never ends: keep this code loaded, or no thread is started.
            if !crate::module::pin_current_module() {
                return;
            }
            let _ = std::thread::Builder::new().name("ferritron-watch".into()).spawn(serve);
        }
    });
}

/// Parameter revisions last seen, by instance.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
struct Seen(Vec<(usize, u64)>);

#[cfg_attr(target_family = "wasm", allow(dead_code))]
fn serve() {
    let mut seen = Seen(Vec::new());
    loop {
        let mut events: Vec<InstanceEvent> = {
            let queue = lock(&HUB.queue);
            let (mut queue, _) = HUB.wake.wait_timeout_while(queue, std::time::Duration::from_millis(30), |q| q.is_empty()).unwrap_or_else(PoisonError::into_inner);
            queue.drain(..).collect()
        };
        if ACTIVE.load(Ordering::Acquire) == 0 {
            seen.0.clear();
            continue;
        }
        // Parameter moves: compare each instance's counter with the last look.
        let live = crate::instance::all_live();
        let mut next = Vec::with_capacity(live.len());
        for handle in live {
            let key = handle.link_key();
            let now = handle.params_revision();
            let before = seen.0.iter().find(|(k, _)| *k == key).map(|e| e.1);
            if before.is_some_and(|b| b != now) {
                events.push(InstanceEvent::Params(handle));
            }
            next.push((key, now));
        }
        seen.0 = next;
        events.retain(|e| !e.hidden());
        if events.is_empty() {
            continue;
        }
        let mut list = lock(&HUB.watchers);
        for event in &events {
            for (_, f) in list.iter_mut() {
                f(event);
            }
        }
        let dropped: Vec<u64> = lock(&DROPPED).drain(..).collect();
        list.retain(|(i, _)| !dropped.contains(i));
    }
}

// ---- Editors (E5) ---------------------------------------------------------------------------

/// Instances whose editor is open, in the order they opened (plugin id, instance).
static OPEN: Mutex<Vec<InstanceHandle>> = Mutex::new(Vec::new());

/// Editors call this through `InstanceHandle::editor_opened`: the instance's first editor
/// opened (`true`) or its last one closed (`false`).
pub(crate) fn editor_open_changed(handle: &InstanceHandle, open: bool) {
    if handle.is_hidden() {
        return;
    }
    let mut list = lock(&OPEN);
    list.retain(|h| h.is_alive() && h != handle);
    if open {
        list.push(handle.clone());
    }
    drop(list);
    let h = handle.clone();
    emit(move || if open { InstanceEvent::EditorOpened(h) } else { InstanceEvent::EditorClosed(h) });
}

pub(crate) fn editor_focused(handle: &InstanceHandle) {
    let h = handle.clone();
    emit(move || InstanceEvent::EditorFocused(h));
}

/// The instances of plugin `P` whose editor is open, in the order the editors opened.
pub fn editors<P: crate::__private::PluginMeta>() -> Vec<InstanceHandle> {
    editors_of(P::INFO.id)
}

/// Like [`editors`], by the plugin's project-file `id`.
pub fn editors_of(plugin_id: &str) -> Vec<InstanceHandle> {
    lock(&OPEN).iter().filter(|h| h.is_alive() && h.plugin_id() == plugin_id && h.is_editor_open()).cloned().collect()
}

/// One editor of a plugin does the shared work ("singleton view"): the editor that opened first,
/// until it closes (then the next one) or another one [`claim`](Self::claim)s it. Every editor
/// asks [`is_owner`](Self::is_owner) when it draws and shows the full view or a short note
/// ("shown in the editor on track 3"). Watch [`InstanceEvent::EditorOpened`] / `EditorClosed`
/// (or redraw on every [`instances_watch`] event) to notice a hand-over.
pub struct SingletonView;

/// The instance that owns the view, by plugin id, when one claimed it.
static CLAIMED: Mutex<Vec<(&'static str, InstanceHandle)>> = Mutex::new(Vec::new());

impl SingletonView {
    /// The editor that does the work for plugin `plugin_id` now (`None`: no editor is open).
    pub fn owner_of(plugin_id: &str) -> Option<InstanceHandle> {
        let open = editors_of(plugin_id);
        let claimed = lock(&CLAIMED).iter().find(|(p, _)| *p == plugin_id).map(|(_, h)| h.clone());
        match claimed {
            Some(h) if open.contains(&h) => Some(h),
            _ => open.into_iter().next(),
        }
    }

    /// Whether `handle`'s editor does the work now.
    pub fn is_owner(handle: &InstanceHandle) -> bool {
        Self::owner_of(handle.plugin_id()).as_ref() == Some(handle)
    }

    /// Makes `handle`'s editor the one that does the work (say, the user clicked "show here").
    /// It stays so until its editor closes.
    pub fn claim(handle: &InstanceHandle) {
        let mut claimed = lock(&CLAIMED);
        claimed.retain(|(p, h)| *p != handle.plugin_id() && h.is_alive());
        claimed.push((handle.plugin_id(), handle.clone()));
        drop(claimed);
        let h = handle.clone();
        emit(move || InstanceEvent::EditorFocused(h));
    }
}
