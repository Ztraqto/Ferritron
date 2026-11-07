//! A repeating timer on the host's UI thread, for editors (refreshing
//! values, animation). Windows: `SetTimer`; macOS: a `CFRunLoopTimer` on
//! the main run loop. Create and drop it on the UI thread.

/// Calls `tick` every `ms` milliseconds on the UI thread until dropped.
pub struct UiTimer {
    #[cfg(any(windows, target_os = "macos"))]
    inner: imp::Timer,
}

impl UiTimer {
    /// `None` where the platform has no UI timer yet (Linux).
    pub fn new(ms: u32, tick: Box<dyn FnMut()>) -> Option<Self> {
        #[cfg(any(windows, target_os = "macos"))]
        return imp::Timer::new(ms, tick).map(|inner| Self { inner });
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (ms, tick);
            None
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl Drop for UiTimer {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetTimer(hwnd: *mut c_void, id: usize, ms: u32, proc_: Option<unsafe extern "system" fn(*mut c_void, u32, usize, u32)>) -> usize;
        fn KillTimer(hwnd: *mut c_void, id: usize) -> i32;
    }

    type Tick = std::rc::Rc<RefCell<Box<dyn FnMut()>>>;

    thread_local! {
        /// Ticks by timer ID. Timers fire on the thread that created them.
        static TICKS: RefCell<HashMap<usize, Tick>> = RefCell::new(HashMap::new());
    }

    pub struct Timer(usize);

    unsafe extern "system" fn on_timer(_hwnd: *mut c_void, _msg: u32, id: usize, _time: u32) {
        let tick = TICKS.with(|t| t.borrow().get(&id).cloned());
        if let Some(tick) = tick
            && let Ok(mut tick) = tick.try_borrow_mut()
        {
            let _ = catch_unwind(AssertUnwindSafe(|| (*tick)()));
        }
    }

    impl Timer {
        pub fn new(ms: u32, tick: Box<dyn FnMut()>) -> Option<Self> {
            let id = unsafe { SetTimer(std::ptr::null_mut(), 0, ms, Some(on_timer)) };
            if id == 0 {
                return None;
            }
            TICKS.with(|t| t.borrow_mut().insert(id, std::rc::Rc::new(RefCell::new(tick))));
            Some(Self(id))
        }

        pub fn stop(&mut self) {
            unsafe { KillTimer(std::ptr::null_mut(), self.0) };
            let tick = TICKS.with(|t| t.borrow_mut().remove(&self.0));
            drop(tick);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    type CFTypeRef = *const c_void;
    type CFRunLoopRef = *mut c_void;
    type CFRunLoopTimerRef = *mut c_void;
    type CFStringRef = *const c_void;

    #[repr(C)]
    struct CFRunLoopTimerContext {
        version: isize,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: CFStringRef;
        fn CFAbsoluteTimeGetCurrent() -> f64;
        fn CFRunLoopGetMain() -> CFRunLoopRef;
        fn CFRunLoopTimerCreate(
            allocator: *const c_void,
            fire_date: f64,
            interval: f64,
            flags: usize,
            order: isize,
            callout: extern "C" fn(CFRunLoopTimerRef, *mut c_void),
            context: *mut CFRunLoopTimerContext,
        ) -> CFRunLoopTimerRef;
        fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
        fn CFRunLoopTimerInvalidate(timer: CFRunLoopTimerRef);
        fn CFRelease(cf: CFTypeRef);
    }

    type Tick = std::cell::RefCell<Box<dyn FnMut()>>;

    pub struct Timer {
        timer: CFRunLoopTimerRef,
        /// From `Rc::into_raw`; each tick holds a reference while it runs,
        /// so a tick may stop its own timer.
        tick: *const Tick,
    }

    extern "C" fn on_timer(_timer: CFRunLoopTimerRef, info: *mut c_void) {
        let tick = unsafe {
            std::rc::Rc::increment_strong_count(info as *const Tick);
            std::rc::Rc::from_raw(info as *const Tick)
        };
        if let Ok(mut tick) = tick.try_borrow_mut() {
            let _ = catch_unwind(AssertUnwindSafe(|| (*tick)()));
        }
    }

    impl Timer {
        pub fn new(ms: u32, tick: Box<dyn FnMut()>) -> Option<Self> {
            let tick = std::rc::Rc::into_raw(std::rc::Rc::new(std::cell::RefCell::new(tick)));
            let interval = ms.max(1) as f64 / 1000.0;
            let mut context = CFRunLoopTimerContext { version: 0, info: tick as *mut c_void, retain: None, release: None, copy_description: None };
            let timer = unsafe {
                CFRunLoopTimerCreate(std::ptr::null(), CFAbsoluteTimeGetCurrent() + interval, interval, 0, 0, on_timer, &mut context)
            };
            if timer.is_null() {
                drop(unsafe { std::rc::Rc::from_raw(tick) });
                return None;
            }
            // Common modes: keeps ticking while the host tracks a menu or a drag.
            unsafe { CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes) };
            Some(Self { timer, tick })
        }

        pub fn stop(&mut self) {
            if self.timer.is_null() {
                return;
            }
            unsafe {
                CFRunLoopTimerInvalidate(self.timer);
                CFRelease(self.timer);
                drop(std::rc::Rc::from_raw(self.tick));
            }
            self.timer = std::ptr::null_mut();
        }
    }
}
