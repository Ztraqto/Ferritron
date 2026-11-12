//! Parallel work inside `process` (design doc, chapter 10).
//!
//! `ctx.parallel_for` splits work into independent tasks. They run on, in
//! order of preference:
//! 1. the host's thread pool, when the format offers one (CLAP `thread-pool`);
//! 2. the Ferritron Runtime's worker pool, if the plugin sets `parallel = true`;
//! 3. the calling thread, one after another.
//!
//! The result is the same in every case. The audio thread never allocates or
//! blocks: it publishes a job, works on it itself and spins until the last
//! task is done. If the pool is busy with another instance's job, the work
//! simply runs on the calling thread.
//!
//! The Runtime is shared by every Ferritron plugin in the process, even ones
//! in different DLLs: the first library to need it publishes its function
//! table (see [`RuntimeApi`]) and pins itself; the others find and use it.
//!
//! Workers run with the priority of audio threads: MMCSS "Pro Audio" on
//! Windows; on macOS a real-time (time constraint) policy, and they join the
//! host's Audio Workgroup when a format adapter hands one over (AU's render
//! context observer), so Apple Silicon keeps them on performance cores with
//! the host's audio thread.

use crate::abi::Executor;
use std::cell::Cell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

/// How long idle workers keep spinning for the next block before parking.
const SPIN: Duration = Duration::from_micros(50);

/// A task callable across DLL boundaries. It must not unwind.
type Task = unsafe extern "C" fn(*mut c_void, u32);

/// The Runtime's C ABI, shared between plugin libraries of the same process.
/// Only grows at the end; `version` changes on incompatible changes.
#[repr(C)]
pub(crate) struct RuntimeApi {
    struct_size: u32,
    version: u32,
    /// Starts the workers on first use. Main thread.
    acquire: unsafe extern "C" fn(),
    /// Stops them when the last user is gone. Main thread.
    release: unsafe extern "C" fn(),
    /// Runs `task(data, 0..count)` on the workers and returns once all are
    /// done, or returns false (pool busy or absent) without running anything.
    run: unsafe extern "C" fn(u32, Task, *mut c_void) -> bool,
    /// The host's Audio Workgroup (`os_workgroup_t`, macOS) the workers
    /// join, or null. Retained while in use. Any thread.
    set_workgroup: unsafe extern "C" fn(*mut c_void),
}

const RUNTIME_VERSION: u32 = 1;

static LOCAL_RUNTIME: RuntimeApi = RuntimeApi {
    struct_size: size_of::<RuntimeApi>() as u32,
    version: RUNTIME_VERSION,
    acquire: local_acquire,
    release: local_release,
    run: local_run,
    set_workgroup: local_set_workgroup,
};

static RUNTIME: OnceLock<&'static RuntimeApi> = OnceLock::new();

/// Finds the process's Runtime, or makes this library's the one. Main thread.
fn runtime() -> &'static RuntimeApi {
    RUNTIME.get_or_init(|| sys::find_or_publish(&LOCAL_RUNTIME))
}

/// Makes sure the process has a Runtime, without starting any threads. The
/// hot reload proxy calls this so the library the host loaded (which lives
/// as long as the plugin is in use) owns it, not a temporary copy.
#[cfg_attr(not(all(any(windows, unix), debug_assertions)), allow(dead_code))]
pub(crate) fn register() {
    runtime();
}

/// Hands the host's Audio Workgroup (`os_workgroup_t`) to the workers, or
/// null when it is gone. macOS; ignored elsewhere.
///
/// # Safety
/// `workgroup` must be null or a valid `os_workgroup_t` (it is retained).
pub unsafe fn set_audio_workgroup(workgroup: *mut c_void) {
    let rt = runtime();
    // A Runtime from an older library has no such entry.
    if rt.struct_size as usize >= size_of::<RuntimeApi>() {
        unsafe { (rt.set_workgroup)(workgroup) }
    }
}

unsafe extern "C" fn local_set_workgroup(workgroup: *mut c_void) {
    sys::set_workgroup(workgroup);
}

/// Starts the workers (or adds a user). Call off the audio thread.
pub(crate) fn acquire() {
    unsafe { (runtime().acquire)() }
}

/// Removes a user; the workers stop with the last one. Call off the audio thread.
pub(crate) fn release() {
    if let Some(rt) = RUNTIME.get() {
        unsafe { (rt.release)() }
    }
}

struct Job {
    task: Task,
    data: *mut c_void,
    count: usize,
    next: AtomicUsize,
    done: AtomicUsize,
}

impl Job {
    /// Claims and runs tasks until none are left.
    fn work(&self) {
        loop {
            let index = self.next.fetch_add(1, Relaxed);
            if index >= self.count {
                return;
            }
            unsafe { (self.task)(self.data, index as u32) };
            self.done.fetch_add(1, Release);
        }
    }
}

struct Shared {
    job: AtomicPtr<Job>,
    generation: AtomicU64,
    /// A job is being dispatched; others run inline instead of waiting.
    busy: AtomicBool,
    /// Workers currently allowed to touch `job`.
    active: AtomicUsize,
    stop: AtomicBool,
}

struct Pool {
    shared: Arc<Shared>,
    threads: Vec<Thread>,
    handles: Vec<JoinHandle<()>>,
}

thread_local! {
    static IN_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Worker count: leave room for the host's own threads.
/// `FERRITRON_WORKERS=0` disables the pool (work then runs on the calling thread).
fn worker_count() -> usize {
    if let Some(n) = std::env::var("FERRITRON_WORKERS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    cores.saturating_sub(2).clamp(1, 16)
}

fn worker(shared: Arc<Shared>) {
    IN_WORKER.with(|w| w.set(true));
    sys::raise_priority();
    let mut member = sys::Member::default();
    let mut seen = shared.generation.load(Acquire);
    let mut idle_since = Instant::now();
    while !shared.stop.load(Acquire) {
        let generation = shared.generation.load(SeqCst);
        let job = shared.job.load(SeqCst);
        if generation != seen && !job.is_null() {
            seen = generation;
            // Register, then make sure the job is still the one we saw: once
            // registered, the dispatcher waits for us before retiring it.
            shared.active.fetch_add(1, SeqCst);
            if shared.job.load(SeqCst) == job && shared.generation.load(SeqCst) == generation {
                member.follow();
                unsafe { (*job).work() };
            }
            shared.active.fetch_sub(1, SeqCst);
            idle_since = Instant::now();
            continue;
        }
        if idle_since.elapsed() < SPIN {
            std::hint::spin_loop();
        } else {
            std::thread::park_timeout(Duration::from_millis(100));
        }
    }
}

/// Gives the calling thread the priority of an audio thread (see `sys::raise_priority`).
/// For threads a host's audio thread waits for.
#[cfg_attr(not(any(windows, unix)), allow(dead_code))]
pub(crate) fn raise_audio_priority() {
    sys::raise_priority();
}

static POOL: Mutex<Option<(Pool, usize)>> = Mutex::new(None);
/// The running pool's shared state, for lock-free use on the audio thread.
static SHARED: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());
static THREADS: AtomicPtr<Vec<Thread>> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" fn local_acquire() {
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, users)) = pool.as_mut() {
        *users += 1;
        return;
    }
    let shared = Arc::new(Shared {
        job: AtomicPtr::new(std::ptr::null_mut()),
        generation: AtomicU64::new(0),
        busy: AtomicBool::new(false),
        active: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    });
    let mut handles = Vec::new();
    for n in 0..worker_count() {
        let s = shared.clone();
        if let Ok(h) = std::thread::Builder::new().name(format!("ferritron-worker-{n}")).stack_size(256 * 1024).spawn(move || worker(s)) {
            handles.push(h);
        }
    }
    let threads: Vec<Thread> = handles.iter().map(|h| h.thread().clone()).collect();
    *pool = Some((Pool { shared, threads, handles }, 1));
    // Publish addresses only now that the pool sits at its final place in the static.
    let (created, _) = pool.as_ref().unwrap();
    SHARED.store(Arc::as_ptr(&created.shared) as *mut Shared, SeqCst);
    THREADS.store(&created.threads as *const Vec<Thread> as *mut Vec<Thread>, SeqCst);
}

unsafe extern "C" fn local_release() {
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((_, users)) = pool.as_mut() else { return };
    *users -= 1;
    if *users > 0 {
        return;
    }
    let (stopped, _) = pool.take().unwrap();
    SHARED.store(std::ptr::null_mut(), SeqCst);
    THREADS.store(std::ptr::null_mut(), SeqCst);
    stopped.shared.stop.store(true, SeqCst);
    for t in &stopped.threads {
        t.unpark();
    }
    for h in stopped.handles {
        let _ = h.join();
    }
}

unsafe extern "C" fn local_run(count: u32, task: Task, data: *mut c_void) -> bool {
    let count = count as usize;
    let shared = SHARED.load(Acquire);
    let threads = THREADS.load(Acquire);
    if count == 0 || shared.is_null() || threads.is_null() || IN_WORKER.with(Cell::get) {
        return false;
    }
    let shared = unsafe { &*shared };
    if shared.busy.swap(true, AcqRel) {
        return false;
    }
    let job = Job { task, data, count, next: AtomicUsize::new(0), done: AtomicUsize::new(0) };
    shared.job.store(&job as *const Job as *mut Job, SeqCst);
    shared.generation.fetch_add(1, SeqCst);
    for t in unsafe { &*threads }.iter().take(count - 1) {
        t.unpark();
    }
    job.work();
    while job.done.load(Acquire) < count {
        std::hint::spin_loop();
    }
    // Retire the job and wait until no worker can still be looking at it.
    shared.job.store(std::ptr::null_mut(), SeqCst);
    while shared.active.load(SeqCst) != 0 {
        std::hint::spin_loop();
    }
    shared.busy.store(false, Release);
    true
}

/// Runs a Rust closure through a C-ABI executor (host pool or Runtime).
/// Panics are caught on the worker and re-raised here.
fn dispatch(
    count: usize,
    task: &(dyn Fn(usize) + Sync),
    run: impl FnOnce(u32, Task, *mut c_void) -> bool,
) -> bool {
    struct Data<'a> {
        task: &'a (dyn Fn(usize) + Sync),
        panicked: AtomicBool,
    }
    unsafe extern "C" fn trampoline(data: *mut c_void, index: u32) {
        let data = unsafe { &*(data as *const Data) };
        if catch_unwind(AssertUnwindSafe(|| (data.task)(index as usize))).is_err() {
            data.panicked.store(true, Relaxed);
        }
    }
    let data = Data { task, panicked: AtomicBool::new(false) };
    let ran = run(count as u32, trampoline, &data as *const Data as *mut c_void);
    if ran && data.panicked.load(Relaxed) {
        panic!("a parallel task panicked");
    }
    ran
}

pub(crate) fn run(executor: Option<&Executor>, count: usize, task: &(dyn Fn(usize) + Sync)) {
    if count == 0 {
        return;
    }
    if count == 1 {
        return task(0);
    }
    if let Some(executor) = executor
        && dispatch(count, task, |n, t, d| unsafe { (executor.run)(executor.ctx, n, t, d) }) {
            return;
        }
    // Only an already-resolved Runtime: resolving it may block (main thread only).
    if let Some(rt) = RUNTIME.get()
        && dispatch(count, task, |n, t, d| unsafe { (rt.run)(n, t, d) }) {
            return;
        }
    (0..count).for_each(task);
}

#[cfg(windows)]
mod sys {
    use super::{RUNTIME_VERSION, RuntimeApi};
    use std::ffi::c_void;

    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> *mut c_void;
        fn AvSetMmThreadPriority(handle: *mut c_void, priority: i32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn ReleaseMutex(handle: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn CreateFileMappingW(file: *mut c_void, attributes: *mut c_void, protect: u32, size_high: u32, size_low: u32, name: *const u16) -> *mut c_void;
        fn MapViewOfFile(mapping: *mut c_void, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
    }

    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Looks up the process's Runtime in a named, process-local shared memory
    /// slot; publishes `local` there if there is none (or it is incompatible
    /// and nobody else claimed the slot). The publishing library pins itself
    /// so the code others call into stays loaded until the process exits.
    pub fn find_or_publish(local: &'static RuntimeApi) -> &'static RuntimeApi {
        let pid = unsafe { GetCurrentProcessId() };
        let lock = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide(&format!("Local\\ferritron-runtime-lock-{pid}")).as_ptr()) };
        if lock.is_null() {
            return local;
        }
        unsafe { WaitForSingleObject(lock, INFINITE) };
        let found = (|| {
            let name = wide(&format!("Local\\ferritron-runtime-v{RUNTIME_VERSION}-{pid}"));
            let mapping = unsafe {
                CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null_mut(), PAGE_READWRITE, 0, size_of::<usize>() as u32, name.as_ptr())
            };
            if mapping.is_null() {
                return local;
            }
            // The mapping handle stays open: the slot must outlive this call.
            let slot = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<usize>()) } as *mut *const RuntimeApi;
            if slot.is_null() {
                return local;
            }
            let existing = unsafe { *slot };
            if let Some(api) = unsafe { existing.as_ref() } {
                if api.version == RUNTIME_VERSION && api.struct_size as usize >= size_of::<RuntimeApi>() {
                    return api;
                }
                return local; // incompatible owner: keep our own pool
            }
            let mut module = std::ptr::null_mut();
            let flags = GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS;
            if unsafe { GetModuleHandleExW(flags, find_or_publish as *const u16, &mut module) } == 0 {
                return local; // cannot pin: do not let others depend on us
            }
            unsafe { *slot = local };
            local
        })();
        unsafe {
            ReleaseMutex(lock);
            CloseHandle(lock);
        }
        found
    }

    pub fn set_workgroup(_workgroup: *mut c_void) {}

    /// Audio Workgroups are macOS only.
    #[derive(Default)]
    pub struct Member(());

    impl Member {
        pub fn follow(&mut self) {}
    }

    /// Registers the worker with MMCSS as "Pro Audio", like host audio threads.
    pub fn raise_priority() {
        let task = wide("Pro Audio");
        let mut index = 0;
        unsafe {
            let handle = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index);
            if !handle.is_null() {
                AvSetMmThreadPriority(handle, 1 /* AVRT_PRIORITY_HIGH */);
            }
        }
    }
}

#[cfg(unix)]
mod sys {
    use super::{RUNTIME_VERSION, RuntimeApi};
    use std::ffi::c_void;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;

    /// Whether `api` is a live Runtime table in this process: a stale slot
    /// (left by a crashed process with the same ID) points nowhere.
    fn valid(api: *const RuntimeApi) -> bool {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        if api.is_null() || unsafe { libc::dladdr(api as *const c_void, &mut info) } == 0 {
            return false;
        }
        let api = unsafe { &*api };
        api.version == RUNTIME_VERSION
            && api.struct_size as usize >= size_of::<usize>() * 4
            && unsafe { libc::dladdr(api.run as *const c_void, &mut info) } != 0
    }

    /// Looks up the process's Runtime in a process-local named shared memory
    /// slot, claiming it for `local` when it is empty. The publishing library
    /// pins itself so the code others call into stays loaded.
    pub fn find_or_publish(local: &'static RuntimeApi) -> &'static RuntimeApi {
        let key = format!("v{RUNTIME_VERSION}-{}", std::process::id());
        let Ok(map) = crate::shmem::Mapping::open(&crate::shmem::name_for("runtime", &key), size_of::<usize>()) else {
            return local;
        };
        let slot = unsafe { &*(map.data() as *const AtomicUsize) };
        std::mem::forget(map); // the slot must outlive this call
        let mine = local as *const RuntimeApi as usize;
        loop {
            let existing = slot.load(SeqCst);
            if existing != 0 && valid(existing as *const RuntimeApi) {
                return unsafe { &*(existing as *const RuntimeApi) };
            }
            if !crate::module::pin_current_module() {
                return local; // cannot pin: do not let others depend on us
            }
            if slot.compare_exchange(existing, mine, SeqCst, SeqCst).is_ok() {
                return local;
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    mod apple {
        use std::ffi::c_void;
        use std::sync::Mutex;

        #[repr(C)]
        struct TimeConstraint {
            period: u32,
            computation: u32,
            constraint: u32,
            preemptible: i32,
        }

        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }

        /// `os_workgroup_join_token_s` (40 bytes), with room to spare.
        #[repr(C, align(8))]
        pub struct Token([u8; 64]);

        unsafe extern "C" {
            fn pthread_mach_thread_np(thread: libc::pthread_t) -> u32;
            fn thread_policy_set(thread: u32, flavor: i32, policy: *const TimeConstraint, count: u32) -> i32;
            fn mach_timebase_info(info: *mut Timebase) -> i32;
            fn os_retain(object: *mut c_void) -> *mut c_void;
            fn os_release(object: *mut c_void);
            fn os_workgroup_join(workgroup: *mut c_void, token: *mut Token) -> i32;
            fn os_workgroup_leave(workgroup: *mut c_void, token: *mut Token);
        }

        const THREAD_TIME_CONSTRAINT_POLICY: i32 = 2;

        /// A real-time policy like audio threads': about 1 ms of work every
        /// 2.9 ms (128 frames at 44.1 kHz), preemptible.
        pub fn raise_priority() {
            let mut timebase = Timebase { numer: 0, denom: 0 };
            unsafe { mach_timebase_info(&mut timebase) };
            if timebase.numer == 0 {
                return;
            }
            let ticks = |ns: u64| (ns * timebase.denom as u64 / timebase.numer as u64) as u32;
            let policy = TimeConstraint { period: ticks(2_900_000), computation: ticks(1_000_000), constraint: ticks(2_900_000), preemptible: 1 };
            unsafe { thread_policy_set(pthread_mach_thread_np(libc::pthread_self()), THREAD_TIME_CONSTRAINT_POLICY, &policy, 4) };
        }

        /// The workgroup workers should be in (retained), and a version
        /// number so workers notice changes cheaply.
        static WORKGROUP: Mutex<(usize, u64)> = Mutex::new((0, 0));

        pub fn set_workgroup(workgroup: *mut c_void) {
            let mut current = WORKGROUP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.0 == workgroup as usize {
                return;
            }
            if !workgroup.is_null() {
                unsafe { os_retain(workgroup) };
            }
            if current.0 != 0 {
                unsafe { os_release(current.0 as *mut c_void) };
            }
            *current = (workgroup as usize, current.1 + 1);
        }

        /// A worker's membership in the host's workgroup.
        pub struct Member {
            version: u64,
            joined: *mut c_void,
            token: Token,
        }

        impl Default for Member {
            fn default() -> Self {
                Self { version: 0, joined: std::ptr::null_mut(), token: Token([0; 64]) }
            }
        }

        impl Member {
            /// Joins the current workgroup (leaving an earlier one). Called
            /// before each job; only locks when the workgroup changed.
            pub fn follow(&mut self) {
                let Ok(current) = WORKGROUP.try_lock() else { return };
                if current.1 == self.version {
                    return;
                }
                self.leave();
                self.version = current.1;
                let workgroup = current.0 as *mut c_void;
                if !workgroup.is_null() && unsafe { os_workgroup_join(workgroup, &mut self.token) } == 0 {
                    unsafe { os_retain(workgroup) };
                    self.joined = workgroup;
                }
            }

            fn leave(&mut self) {
                if !self.joined.is_null() {
                    unsafe {
                        os_workgroup_leave(self.joined, &mut self.token);
                        os_release(self.joined);
                    }
                    self.joined = std::ptr::null_mut();
                }
            }
        }

        impl Drop for Member {
            fn drop(&mut self) {
                self.leave();
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    pub use apple::{Member, raise_priority, set_workgroup};

    #[cfg(not(target_vendor = "apple"))]
    pub fn raise_priority() {}

    #[cfg(not(target_vendor = "apple"))]
    pub fn set_workgroup(_workgroup: *mut c_void) {}

    #[cfg(not(target_vendor = "apple"))]
    #[derive(Default)]
    pub struct Member(());

    #[cfg(not(target_vendor = "apple"))]
    impl Member {
        pub fn follow(&mut self) {}
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::RuntimeApi;
    use std::ffi::c_void;

    pub fn find_or_publish(local: &'static RuntimeApi) -> &'static RuntimeApi {
        local
    }

    pub fn raise_priority() {}

    pub fn set_workgroup(_workgroup: *mut c_void) {}

    #[derive(Default)]
    pub struct Member(());

    impl Member {
        pub fn follow(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    // The pool is global; run the pool tests one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn every_task_runs_exactly_once_on_the_pool() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        acquire();
        let hits: Vec<AtomicU32> = (0..1000).map(|_| AtomicU32::new(0)).collect();
        for _ in 0..200 {
            run(None, hits.len(), &|i| {
                hits[i].fetch_add(1, Relaxed);
            });
        }
        assert!(hits.iter().all(|h| h.load(Relaxed) == 200));
        release();
        assert!(SHARED.load(SeqCst).is_null(), "pool stopped after its last user");
    }

    #[test]
    fn works_without_a_pool_and_from_many_threads_at_once() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let sum = AtomicU32::new(0);
        run(None, 10, &|i| {
            sum.fetch_add(i as u32, Relaxed);
        });
        assert_eq!(sum.load(Relaxed), 45);

        acquire();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..100 {
                        let local = AtomicU32::new(0);
                        run(None, 64, &|_| {
                            local.fetch_add(1, Relaxed);
                        });
                        assert_eq!(local.load(Relaxed), 64); // busy pool -> inline, still complete
                    }
                });
            }
        });
        release();
    }

    /// A fake host pool: runs the tasks on scoped threads and counts its calls.
    struct FakeHost {
        calls: AtomicU32,
        accept: bool,
    }

    unsafe extern "C" fn fake_run(
        ctx: *mut c_void,
        count: u32,
        task: unsafe extern "C" fn(*mut c_void, u32),
        data: *mut c_void,
    ) -> bool {
        let host = unsafe { &*(ctx as *const FakeHost) };
        host.calls.fetch_add(1, Relaxed);
        if !host.accept {
            return false;
        }
        struct Data(*mut c_void);
        unsafe impl Sync for Data {}
        let data = Data(data);
        std::thread::scope(|s| {
            for i in 0..count {
                let data = &data;
                s.spawn(move || unsafe { task(data.0, i) });
            }
        });
        true
    }

    #[test]
    fn the_host_pool_is_preferred_and_declining_falls_back() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        for accept in [true, false] {
            let host = FakeHost { calls: AtomicU32::new(0), accept };
            let executor = Executor { ctx: &host as *const FakeHost as *mut c_void, run: fake_run };
            let hits: Vec<AtomicU32> = (0..16).map(|_| AtomicU32::new(0)).collect();
            run(Some(&executor), hits.len(), &|i| {
                hits[i].fetch_add(1, Relaxed);
            });
            assert_eq!(host.calls.load(Relaxed), 1, "host asked once (accept = {accept})");
            assert!(hits.iter().all(|h| h.load(Relaxed) == 1), "every task ran once (accept = {accept})");
        }
    }

    #[test]
    fn task_panics_reach_the_caller() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        acquire();
        let result = catch_unwind(|| run(None, 8, &|i| assert!(i != 5, "boom")));
        assert!(result.is_err());
        // The pool keeps working afterwards.
        let n = AtomicU32::new(0);
        run(None, 8, &|_| {
            n.fetch_add(1, Relaxed);
        });
        assert_eq!(n.load(Relaxed), 8);
        release();
    }
}
