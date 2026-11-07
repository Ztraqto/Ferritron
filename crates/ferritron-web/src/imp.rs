//! The web view inside the host's window: a WebView2 child window of the
//! host's HWND (Windows), or a WKWebView subview of the host's NSView (macOS).

use super::{BRIDGE, WebAssets, json_str, mime, param_json};
use ferritron_core::__private::{PluginMeta, UiTimer};
use ferritron_core::abi::ViewHost;
use ferritron_core::{ParamInfo, ParamSet, Process};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};
use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use wry::http::{Request, Response};
use wry::{WebContext, WebView, WebViewId};

/// Traces editor timings to stderr when `FERRITRON_TRACE` is set.
fn trace(started: std::time::Instant, what: &str) {
    if std::env::var_os("FERRITRON_TRACE").is_some() {
        eprintln!("ferritron: web view {what} {:.1} ms after open", started.elapsed().as_secs_f64() * 1e3);
    }
}

/// How often the UI is refreshed with values changed by the host or automation.
const REFRESH_MS: u32 = 33;

/// The page, served from the custom `ferritron` protocol: WebView2 maps
/// custom protocols to `http://<scheme>.localhost`, WebKit keeps the scheme.
#[cfg(windows)]
const START_URL: &str = "http://ferritron.localhost/index.html";
#[cfg(not(windows))]
const START_URL: &str = "ferritron://localhost/index.html";

#[cfg(windows)]
mod platform {
    use raw_window_handle::{RawWindowHandle, Win32WindowHandle};
    use std::ffi::c_void;
    use std::num::NonZeroIsize;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    }

    pub fn handle(parent: *mut c_void) -> Option<RawWindowHandle> {
        Some(RawWindowHandle::Win32(Win32WindowHandle::new(NonZeroIsize::new(parent as isize)?)))
    }

    /// WebView2 already renders at the window's DPI. When the host asks for a
    /// different scale, zoom by the remaining factor.
    pub fn zoom_for(parent: *mut c_void, scale: f64) -> f64 {
        let dpi = unsafe { GetDpiForWindow(parent) };
        let window_scale = if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 };
        if scale > 0.0 { scale / window_scale } else { 1.0 }
    }

    /// Sizes are physical pixels.
    pub fn bounds(width: u32, height: u32) -> wry::Rect {
        wry::Rect { position: wry::dpi::PhysicalPosition::new(0, 0).into(), size: wry::dpi::PhysicalSize::new(width, height).into() }
    }

    /// One folder per host program. WebView2 shares a browser process per
    /// folder, which keeps later editors fast, but refuses
    /// (`ERROR_INVALID_STATE`) when two programs with different options use
    /// the same folder, for example two DAWs open at once.
    pub(crate) fn data_directory() -> Option<std::path::PathBuf> {
        let base = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Ferritron").join("WebView2");
        let host = std::env::current_exe().ok().and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()));
        let host: String = host.unwrap_or_else(|| "host".into()).chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
        Some(base.join(host))
    }

    /// Editors share the environment started ahead of time (see
    /// `environment`). Without one, wry starts its own, in a folder of this
    /// process only, as the shared folder is then probably in use with other
    /// options.
    pub fn builder(context: &mut Option<wry::WebContext>) -> wry::WebViewBuilder<'_> {
        use wry::WebViewBuilderExtWindows;
        let builder = match crate::environment::get() {
            Some(env) => wry::WebViewBuilder::new().with_environment(env),
            None => {
                let folder = data_directory()
                    .map(|d| d.with_file_name(format!("{}-{}", d.file_name().unwrap_or_default().to_string_lossy(), std::process::id())));
                wry::WebViewBuilder::new_with_web_context(context.insert(wry::WebContext::new(folder)))
            }
        };
        // Reload, print, find, ...: browser keys that have no place in an editor.
        builder.with_browser_accelerator_keys(false)
    }

    /// A plugin instance: the browser starts shortly.
    pub fn retain() {
        crate::environment::retain();
    }

    /// An editor opening: the browser starts now.
    pub fn retain_now() {
        crate::environment::retain_now();
    }

    pub fn release() {
        crate::environment::release();
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use raw_window_handle::{AppKitWindowHandle, RawWindowHandle};
    use std::ffi::c_void;
    use std::ptr::NonNull;

    pub fn handle(parent: *mut c_void) -> Option<RawWindowHandle> {
        Some(RawWindowHandle::AppKit(AppKitWindowHandle::new(NonNull::new(parent)?)))
    }

    /// WebKit renders at the screen's backing scale; sizes are points.
    pub fn zoom_for(_parent: *mut c_void, _scale: f64) -> f64 {
        1.0
    }

    /// Sizes are points (logical).
    pub fn bounds(width: u32, height: u32) -> wry::Rect {
        wry::Rect { position: wry::dpi::LogicalPosition::new(0, 0).into(), size: wry::dpi::LogicalSize::new(width, height).into() }
    }

    /// WKWebView keeps its data per app (the host's): nothing to choose or share.
    pub fn builder(_context: &mut Option<wry::WebContext>) -> wry::WebViewBuilder<'_> {
        wry::WebViewBuilder::new()
    }

    pub fn retain() {}

    pub fn retain_now() {}

    pub fn release() {}
}

pub(crate) use platform::{release, retain, retain_now};
#[cfg(windows)]
pub(crate) use platform::data_directory;
use platform::{bounds, zoom_for};

struct Parent(RawWindowHandle);

impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: the host keeps the parent window alive while the view is open.
        Ok(unsafe { WindowHandle::borrow_raw(self.0) })
    }
}

struct View {
    /// Dropped first: no refresh may run while the web view goes away.
    timer: Option<UiTimer>,
    webview: WebView,
    parent: *mut c_void,
    zoom: f64,
    _context: Option<WebContext>,
    host: ViewHost,
    /// What the page sees: the plugin's parameters and built-in switches, by index.
    params: Vec<(usize, &'static ParamInfo)>,
    last: Vec<f64>,
    describe: fn(usize, &ParamInfo, f64) -> String,
    /// The editor's instance (messages, track), when the shim links it.
    instance: Option<ferritron_core::InstanceHandle>,
    /// Its revision as last shown to the page.
    revision: u64,
}

impl View {
    /// Pushes parameters that changed since the last refresh to the page.
    fn refresh(&mut self) {
        let mut script = String::new();
        for (slot, &(index, info)) in self.params.iter().enumerate() {
            let normalized = unsafe { (self.host.get_param)(self.host.ctx, info.id) };
            if normalized == self.last[slot] {
                continue;
            }
            self.last[slot] = normalized;
            let state = (self.describe)(index, info, normalized);
            script.push_str(&format!("(p=>ferritron._update({},p.value,p.normalized,p.text))({state});", json_str(info.key)));
        }
        if let Some(instance) = &self.instance {
            let revision = instance.revision();
            if revision != self.revision {
                self.revision = revision;
                script.push_str(&format!("ferritron._instance({});", super::instance_json(Some(instance))));
            }
            for message in instance.take_posted() {
                script.push_str(&format!("ferritron._message({});", json_str(&message)));
            }
        }
        if !script.is_empty() {
            let _ = self.webview.evaluate_script(&script);
        }
    }

    fn handle_message(&self, message: &str) {
        if let Some(json) = message.strip_prefix("msg:") {
            if let Some(instance) = &self.instance {
                instance.deliver_message(json);
            }
            return;
        }
        let mut parts = message.split(':');
        let (Some(kind), Some(id)) = (parts.next(), parts.next().and_then(|s| s.parse::<u32>().ok())) else { return };
        let Some(&(_, info)) = self.params.iter().find(|(_, info)| info.id == id) else { return };
        let host = &self.host;
        unsafe {
            match kind {
                "begin" => (host.begin_edit)(host.ctx, id),
                "end" => (host.end_edit)(host.ctx, id),
                "set" => {
                    if let Some(value) = parts.next().and_then(|s| s.parse::<f64>().ok()) {
                        (host.perform_edit)(host.ctx, id, info.normalize(value));
                    }
                }
                _ => {}
            }
        }
    }
}

fn serve<P: WebAssets>(_id: WebViewId, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let path = request.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match P::ASSETS.iter().find(|(p, _)| *p == path) {
        Some((_, data)) => Response::builder().header("Content-Type", mime(path)).body(Cow::Borrowed(*data)).unwrap(),
        None => Response::builder().status(404).body(Cow::Borrowed(&b"not found"[..])).unwrap(),
    }
}

fn create<P: Process + PluginMeta + WebAssets>(parent: *mut c_void, host: ViewHost, width: u32, height: u32, scale: f64) -> Option<Box<View>> {
    let started = std::time::Instant::now();
    let parent_handle = Parent(platform::handle(parent)?);
    // Built-in switches the editor may show: bypass and (with `[quality]`)
    // quality; never AI Access, which only the user turns on.
    let params: Vec<(usize, &'static ParamInfo)> =
        ferritron_core::abi::all_params::<P>().filter(|(_, info)| info.key != "ferritron.mcp").collect();
    let own = P::Params::infos().len();
    // The shim's object for the instance is the view host's context.
    let instance = ferritron_core::InstanceHandle::for_shim_ctx(host.ctx);
    let revision = instance.as_ref().map_or(0, |i| i.revision());
    let last: Vec<f64> = params.iter().map(|(_, i)| unsafe { (host.get_param)(host.ctx, i.id) }).collect();
    let json = |range: &dyn Fn(usize) -> bool| -> String {
        let entries: Vec<String> = params
            .iter()
            .zip(&last)
            .filter(|((index, _), _)| range(*index))
            .map(|(&(index, info), &normalized)| format!("{}:{}", json_str(info.key), param_json::<P>(index, info, normalized)))
            .collect();
        format!("{{{}}}", entries.join(","))
    };
    let localized = P::localized();
    let messages: Vec<String> = localized.ui_messages().iter().map(|(k, v)| format!("{}:{}", json_str(k), json_str(v))).collect();
    let bridge = BRIDGE
        .replace("__PARAMS__", &json(&|index| index < own))
        .replace("__BUILTINS__", &json(&|index| index >= own))
        .replace("__MESSAGES__", &format!("{{{}}}", messages.join(",")))
        .replace("__LANGUAGE__", &json_str(&localized.language))
        .replace("__INSTANCE__", &super::instance_json(instance.as_ref()));

    let view_cell: std::rc::Rc<RefCell<*const View>> = std::rc::Rc::new(RefCell::new(std::ptr::null()));
    let ipc_target = view_cell.clone();
    let mut context = None;
    let mut builder = platform::builder(&mut context)
        .with_initialization_script(bridge)
        .with_custom_protocol("ferritron".into(), serve::<P>)
        .with_ipc_handler(move |request: Request<String>| {
            let view = *ipc_target.borrow();
            if !view.is_null() {
                let _ = catch_unwind(AssertUnwindSafe(|| unsafe { (*view).handle_message(request.body()) }));
            }
        })
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, wry::PageLoadEvent::Finished) {
                trace(started, "page loaded");
            }
        })
        .with_url(START_URL)
        .with_bounds(bounds(width, height));
    // Painted until the page draws itself: no white flash in a dark editor.
    if let Some([r, g, b]) = P::BACKGROUND {
        builder = builder.with_background_color((r, g, b, 255));
    }
    let webview = builder
        .build_as_child(&parent_handle)
        .map_err(|e| trace(started, &format!("failed ({e})")))
        .ok()?;
    trace(started, "built");
    let zoom = zoom_for(parent, scale);
    if zoom != 1.0 {
        let _ = webview.zoom(zoom);
    }

    let mut view = Box::new(View { timer: None, webview, parent, zoom, _context: context, host, params, last, describe: param_json::<P>, instance, revision });
    *view_cell.borrow_mut() = &*view;
    let target: *mut View = &mut *view;
    // SAFETY: the timer is the view's first field: it stops before the view goes.
    view.timer = UiTimer::new(REFRESH_MS, Box::new(move || unsafe { (*target).refresh() }));
    Some(view)
}

pub(crate) unsafe extern "C" fn open<P: Process + PluginMeta + WebAssets>(
    parent: *mut c_void,
    host: *const ViewHost,
    width: u32,
    height: u32,
    scale: f64,
) -> *mut c_void {
    if parent.is_null() || host.is_null() {
        return std::ptr::null_mut();
    }
    let host = unsafe { *host };
    // An open editor keeps the shared browser environment.
    retain_now();
    match catch_unwind(AssertUnwindSafe(|| create::<P>(parent, host, width, height, scale))) {
        Ok(Some(view)) => Box::into_raw(view).cast(),
        _ => {
            release();
            std::ptr::null_mut()
        }
    }
}

pub(crate) unsafe extern "C" fn set_size(view: *mut c_void, width: u32, height: u32, scale: f64) {
    let Some(view) = (unsafe { (view as *mut View).as_mut() }) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = view.webview.set_bounds(bounds(width, height));
        let zoom = zoom_for(view.parent, scale);
        if zoom != view.zoom {
            view.zoom = zoom;
            let _ = view.webview.zoom(zoom);
        }
    }));
}

pub(crate) unsafe extern "C" fn close(view: *mut c_void) {
    if view.is_null() {
        return;
    }
    let mut view = unsafe { Box::from_raw(view as *mut View) };
    view.timer = None;
    let _ = catch_unwind(AssertUnwindSafe(move || drop(view)));
    release();
}
