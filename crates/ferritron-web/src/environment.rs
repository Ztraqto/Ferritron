//! One WebView2 environment per UI thread, shared by every editor (Windows).
//!
//! WebView2 starts a browser process for its first web view and ends it with
//! the last one; starting it costs 300 ms or more (much more while a previous
//! one is still shutting down), while an editor that finds it running opens
//! in about 60 ms. So Ferritron starts it ahead of time, shortly after a plugin
//! instance is created (`[view] preload`, on by default; the short delay
//! spares hosts that create and destroy instances to scan them), and keeps it running
//! with a hidden, empty web view (the "keeper") while plugin instances or
//! open editors use it. Everything is asynchronous: WebView2 completes the
//! steps from the host's own message loop, so the host never waits for it.
//! When the last user goes, the keeper closes and the browser process ends.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
};
use webview2_com::{CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler};
use windows::Win32::Foundation::HWND;
use windows::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME};
use windows::Win32::System::Com::{APTTYPE, APTTYPE_MAINSTA, APTTYPE_STA, APTTYPEQUALIFIER, CoGetApartmentType};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, KillTimer, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, SetTimer, TranslateMessage, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

/// What the browser is started with; every editor of the process shares it.
/// The same as wry's defaults: no mini menu or SmartScreen, autoplay allowed.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";

/// How long an editor waits for an environment that is still starting.
const WAIT: Duration = Duration::from_secs(10);

/// How long after the first instance the browser starts: hosts that scan
/// plugins create and destroy instances within milliseconds.
const PRELOAD_DELAY_MS: u32 = 250;

/// The hidden web view that keeps the browser process running, in a hidden window.
struct Keeper {
    window: HWND,
    controller: Option<ICoreWebView2Controller>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        if let Some(controller) = self.controller.take() {
            let _ = unsafe { controller.Close() };
        }
        let _ = unsafe { DestroyWindow(self.window) };
    }
}

enum State {
    Idle,
    /// Started; the number tells this attempt from a later one.
    Pending(u64),
    Ready { attempt: u64, environment: ICoreWebView2Environment, keeper: Option<Keeper> },
    Failed,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State::Idle) };
    /// The timer that starts the environment after `PRELOAD_DELAY_MS`.
    static DELAYED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Plugin instances (with `preload`) and open editors that use the environment.
static USERS: AtomicUsize = AtomicUsize::new(0);
static ATTEMPT: AtomicU64 = AtomicU64::new(0);

fn trace(started: Instant, what: &str) {
    if std::env::var_os("FERRITRON_TRACE").is_some() {
        eprintln!("ferritron: web environment {what} ({:.1} ms)", started.elapsed().as_secs_f64() * 1e3);
    }
}

/// WebView2 needs a single-threaded apartment: only the host's UI thread
/// (initialized by the host) starts anything, never a worker thread.
fn on_ui_thread() -> bool {
    let (mut kind, mut qualifier) = (APTTYPE::default(), APTTYPEQUALIFIER::default());
    unsafe { CoGetApartmentType(&mut kind, &mut qualifier) }.is_ok() && (kind == APTTYPE_STA || kind == APTTYPE_MAINSTA)
}

/// One more user (a plugin instance): starts the environment soon.
pub fn retain() {
    USERS.fetch_add(1, Ordering::AcqRel);
    if DELAYED.try_with(|d| d.get()).unwrap_or(1) == 0 && on_ui_thread() {
        let id = unsafe { SetTimer(None, 0, PRELOAD_DELAY_MS, Some(delayed_start)) };
        let _ = DELAYED.try_with(|d| d.set(id));
    }
}

/// One more user that needs the environment now (an editor opening).
pub fn retain_now() {
    USERS.fetch_add(1, Ordering::AcqRel);
    start();
}

unsafe extern "system" fn delayed_start(_window: HWND, _message: u32, _id: usize, _time: u32) {
    cancel_delayed();
    if USERS.load(Ordering::Acquire) > 0 {
        start();
    }
}

fn cancel_delayed() {
    let id = DELAYED.try_with(|d| d.replace(0)).unwrap_or(0);
    if id != 0 {
        let _ = unsafe { KillTimer(None, id) };
    }
}

/// One user less; after the last one the browser process ends.
pub fn release() {
    let mut users = USERS.load(Ordering::Acquire);
    while users > 0 {
        match USERS.compare_exchange_weak(users, users - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(now) => users = now,
        }
    }
    if users == 1 {
        cancel_delayed();
        // COM objects go on the thread that made them; editors still open
        // hold their own reference to the environment.
        let old = STATE.try_with(|s| s.try_borrow_mut().map(|mut s| std::mem::replace(&mut *s, State::Idle)).ok()).ok().flatten();
        if matches!(old, Some(State::Ready { .. })) {
            trace(Instant::now(), "released");
        }
        drop(old);
    }
}

fn start() {
    let attempt = STATE.try_with(|s| {
        let mut state = s.try_borrow_mut().ok()?;
        if !matches!(*state, State::Idle) || !on_ui_thread() {
            return None;
        }
        let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        *state = State::Pending(attempt);
        Some(attempt)
    });
    if let Ok(Some(attempt)) = attempt {
        let started = Instant::now();
        if let Err(e) = unsafe { create(attempt, started) } {
            trace(started, &format!("failed to start: {e}"));
            set(attempt, State::Failed);
        }
    }
}

fn pending(attempt: u64) -> bool {
    STATE.try_with(|s| matches!(*s.borrow(), State::Pending(a) if a == attempt)).unwrap_or(false)
}

/// The result of `attempt`, unless the environment was released meanwhile.
fn set(attempt: u64, next: State) {
    let _ = STATE.try_with(|s| {
        let Ok(mut state) = s.try_borrow_mut() else { return };
        if matches!(*state, State::Pending(a) if a == attempt) {
            *state = next;
        }
    });
}

unsafe fn create(attempt: u64, started: Instant) -> windows::core::Result<()> {
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        options.set_additional_browser_arguments(BROWSER_ARGS.into());
        let mut lang = [0u16; MAX_LOCALE_NAME as usize];
        let len = LCIDToLocaleName(GetUserDefaultUILanguage() as u32, Some(&mut lang), LOCALE_ALLOW_NEUTRAL_NAMES);
        if len > 1 {
            options.set_language(String::from_utf16_lossy(&lang[..len as usize - 1]));
        }
    }
    let folder = super::imp::data_directory().map(|d| HSTRING::from(d.as_os_str())).unwrap_or_default();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        match result.ok().and(environment) {
            // Released meanwhile: nothing to keep.
            Some(_) if !pending(attempt) => {}
            Some(environment) => {
                trace(started, "created");
                let keeper = unsafe { keep(&environment, attempt, started) };
                set(attempt, State::Ready { attempt, environment, keeper });
            }
            None => {
                trace(started, "failed");
                set(attempt, State::Failed);
            }
        }
        Ok(())
    }));
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &folder, &ICoreWebView2EnvironmentOptions::from(options), &handler) }
}

/// Starts the keeper: a web view in a hidden window, never shown.
unsafe fn keep(environment: &ICoreWebView2Environment, attempt: u64, started: Instant) -> Option<Keeper> {
    let window = unsafe {
        CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("STATIC"), w!("Ferritron"), WS_POPUP, 0, 0, 1, 1, None, None, None, None)
    }
    .ok()?;
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        let Some(controller) = result.ok().and(controller) else { return Ok(()) };
        let _ = unsafe { controller.SetIsVisible(false) };
        trace(started, "kept running");
        let _ = STATE.try_with(|s| {
            let Ok(mut state) = s.try_borrow_mut() else { return };
            match &mut *state {
                State::Ready { attempt: a, keeper: Some(keeper), .. } if *a == attempt => keeper.controller = Some(controller.clone()),
                // Released meanwhile.
                _ => {
                    let _ = unsafe { controller.Close() };
                }
            }
        });
        Ok(())
    }));
    if unsafe { environment.CreateCoreWebView2Controller(window, &handler) }.is_err() {
        let _ = unsafe { DestroyWindow(window) };
        return None;
    }
    Some(Keeper { window, controller: None })
}

/// The shared environment, waiting (and running the thread's messages, as
/// WebView2 requires) while it is still starting. `None` if it cannot be
/// created on this thread; the caller then lets wry create its own.
pub fn get() -> Option<ICoreWebView2Environment> {
    cancel_delayed();
    start();
    let deadline = Instant::now() + WAIT;
    loop {
        let ready = STATE.try_with(|s| match &*s.borrow() {
            State::Ready { environment, .. } => Some(Some(environment.clone())),
            State::Pending(_) => None,
            State::Idle | State::Failed => Some(None),
        });
        match ready {
            Ok(Some(environment)) => return environment,
            Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => return None,
            Ok(None) => pump(),
        }
    }
}

/// Runs pending messages of this thread, or waits briefly for one.
fn pump() {
    unsafe {
        let mut msg = MSG::default();
        if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        } else {
            MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
}
