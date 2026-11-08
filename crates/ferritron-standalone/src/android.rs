//! Android: the app's Java activity (`dev.ferritron.app.FerritronActivity`,
//! android/FerritronActivity.java) calls these native methods. Audio runs
//! through AAudio (low latency, float, stereo); the editor is a WebView
//! that talks to the plugin through the activity (android/bridge.js).

use crate::engine::{Renderer, Shared, set_data_dir};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jdouble, jint, jlong, jstring};
use std::ffi::c_void;

#[allow(non_camel_case_types)]
type aaudio_result_t = i32;
type Stream = *mut c_void;
type Builder = *mut c_void;
type DataCallback = unsafe extern "C" fn(Stream, *mut c_void, *mut c_void, i32) -> i32;

#[link(name = "aaudio")]
unsafe extern "C" {
    fn AAudio_createStreamBuilder(builder: *mut Builder) -> aaudio_result_t;
    fn AAudioStreamBuilder_setDirection(builder: Builder, direction: i32);
    fn AAudioStreamBuilder_setSampleRate(builder: Builder, rate: i32);
    fn AAudioStreamBuilder_setChannelCount(builder: Builder, count: i32);
    fn AAudioStreamBuilder_setFormat(builder: Builder, format: i32);
    fn AAudioStreamBuilder_setPerformanceMode(builder: Builder, mode: i32);
    fn AAudioStreamBuilder_setSharingMode(builder: Builder, mode: i32);
    fn AAudioStreamBuilder_setDataCallback(builder: Builder, callback: DataCallback, user: *mut c_void);
    fn AAudioStreamBuilder_openStream(builder: Builder, stream: *mut Stream) -> aaudio_result_t;
    fn AAudioStreamBuilder_delete(builder: Builder) -> aaudio_result_t;
    fn AAudioStream_getSampleRate(stream: Stream) -> i32;
    fn AAudioStream_getChannelCount(stream: Stream) -> i32;
    fn AAudioStream_requestStart(stream: Stream) -> aaudio_result_t;
    fn AAudioStream_requestStop(stream: Stream) -> aaudio_result_t;
    fn AAudioStream_close(stream: Stream) -> aaudio_result_t;
}

const DIRECTION_OUTPUT: i32 = 0;
const DIRECTION_INPUT: i32 = 1;
const FORMAT_FLOAT: i32 = 2;
const PERFORMANCE_LOW_LATENCY: i32 = 12;
const SHARING_SHARED: i32 = 1;
const CALLBACK_CONTINUE: i32 = 0;

/// The running app: the instance, its renderer and the streams.
struct App {
    shared: &'static Shared,
    renderer: Renderer,
    output: Stream,
    input: Stream,
    channels: usize,
    input_channels: usize,
}

unsafe extern "C" fn render(stream: Stream, user: *mut c_void, data: *mut c_void, frames: i32) -> i32 {
    let app = unsafe { &mut *(user as *mut App) };
    let channels = (unsafe { AAudioStream_getChannelCount(stream) }.max(1)) as usize;
    let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, frames.max(0) as usize * channels) };
    app.renderer.render(out, channels);
    CALLBACK_CONTINUE
}

unsafe extern "C" fn capture(_stream: Stream, user: *mut c_void, data: *mut c_void, frames: i32) -> i32 {
    let app = unsafe { &*(user as *const App) };
    let input = unsafe { std::slice::from_raw_parts(data as *const f32, frames.max(0) as usize * app.input_channels) };
    app.shared.push_input(input, app.input_channels);
    CALLBACK_CONTINUE
}

fn open(direction: i32, rate: i32, callback: DataCallback, user: *mut c_void) -> Option<Stream> {
    let mut builder: Builder = std::ptr::null_mut();
    if unsafe { AAudio_createStreamBuilder(&mut builder) } != 0 {
        return None;
    }
    unsafe {
        AAudioStreamBuilder_setDirection(builder, direction);
        AAudioStreamBuilder_setFormat(builder, FORMAT_FLOAT);
        AAudioStreamBuilder_setChannelCount(builder, 2);
        AAudioStreamBuilder_setPerformanceMode(builder, PERFORMANCE_LOW_LATENCY);
        AAudioStreamBuilder_setSharingMode(builder, SHARING_SHARED);
        if rate > 0 {
            AAudioStreamBuilder_setSampleRate(builder, rate);
        }
        AAudioStreamBuilder_setDataCallback(builder, callback, user);
    }
    let mut stream: Stream = std::ptr::null_mut();
    let ok = unsafe { AAudioStreamBuilder_openStream(builder, &mut stream) } == 0;
    unsafe { AAudioStreamBuilder_delete(builder) };
    ok.then_some(stream)
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn app<'a>(handle: jlong) -> Option<&'a mut App> {
    unsafe { (handle as *mut App).as_mut() }
}

/// Starts the plugin and its audio; returns the app's handle (0 on failure).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    language: JString,
    record: jboolean,
) -> jlong {
    let (dir, language) = (string(&mut env, &data_dir), string(&mut env, &language));
    set_data_dir(std::path::PathBuf::from(dir));
    if !language.is_empty() && std::env::var_os("FERRITRON_LANGUAGE").is_none() {
        // SAFETY: set once, before the plugin reads its language, while no other thread reads the environment.
        unsafe { std::env::set_var("FERRITRON_LANGUAGE", language) };
    }
    let Some(api) = crate::api() else { return 0 };
    let Some(shared) = Shared::new(api) else { return 0 };
    let shared: &'static Shared = Box::leak(shared);
    let app = Box::into_raw(Box::new(App {
        shared,
        renderer: Renderer::new(shared),
        output: std::ptr::null_mut(),
        input: std::ptr::null_mut(),
        channels: 2,
        input_channels: 2,
    }));
    let me = unsafe { &mut *app };
    let Some(output) = open(DIRECTION_OUTPUT, 0, render, app.cast()) else { return 0 };
    me.output = output;
    me.channels = unsafe { AAudioStream_getChannelCount(output) }.max(1) as usize;
    let rate = unsafe { AAudioStream_getSampleRate(output) };
    if !shared.activate(rate as f64) {
        return 0;
    }
    if !shared.instrument && record != 0 {
        if let Some(input) = open(DIRECTION_INPUT, rate, capture, app.cast()) {
            me.input = input;
            me.input_channels = unsafe { AAudioStream_getChannelCount(input) }.max(1) as usize;
            unsafe { AAudioStream_requestStart(input) };
        }
    }
    unsafe { AAudioStream_requestStart(output) };
    app as jlong
}

/// Stops the audio (the app goes to the background) or starts it again.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeSuspend(_env: JNIEnv, _class: JClass, handle: jlong, suspend: jboolean) {
    let Some(app) = app(handle) else { return };
    for stream in [app.output, app.input].into_iter().filter(|s| !s.is_null()) {
        unsafe {
            if suspend != 0 {
                AAudioStream_requestStop(stream);
            } else {
                AAudioStream_requestStart(stream);
            }
        };
    }
    if suspend != 0 {
        app.shared.save();
    }
}

/// Stops everything and saves the plugin's state.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeStop(_env: JNIEnv, _class: JClass, handle: jlong) {
    let Some(app) = app(handle) else { return };
    for stream in [app.output, app.input].into_iter().filter(|s| !s.is_null()) {
        unsafe {
            AAudioStream_requestStop(stream);
            AAudioStream_close(stream);
        }
    }
    app.shared.save();
    // The instance stays for the process: Android may start the activity again.
}

/// The plugin described as JSON (`ferritron.describe.v1`), for the editor.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeDescribe(env: JNIEnv, _class: JClass, _handle: jlong) -> jstring {
    let json = crate::api().map(describe).unwrap_or_else(|| "{}".into());
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn describe(api: &ferritron_core::abi::PluginApi) -> String {
    let ext = unsafe { (api.get_extension)(ferritron_core::abi::EXT_DESCRIBE.as_ptr()) } as *const ferritron_core::abi::DescribeApi;
    let Some(ext) = (unsafe { ext.as_ref() }) else { return "{}".into() };
    let len = unsafe { (ext.describe)(std::ptr::null_mut(), 0) };
    let mut buf = vec![0u8; len as usize];
    unsafe { (ext.describe)(buf.as_mut_ptr(), len) };
    String::from_utf8(buf).unwrap_or_else(|_| "{}".into())
}

/// Every parameter's current value: `[[id, normalized, plain, "text"], ...]`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeValues(env: JNIEnv, _class: JClass, handle: jlong) -> jstring {
    let Some(app) = app(handle) else { return std::ptr::null_mut() };
    let (api, instance) = (app.shared.api, app.shared.instance);
    let mut out = String::from("[");
    for i in 0..unsafe { (api.param_count)() } {
        let mut info = std::mem::MaybeUninit::<ferritron_core::abi::ParamInfoC>::uninit();
        if !unsafe { (api.param_info)(i, info.as_mut_ptr()) } {
            continue;
        }
        let id = unsafe { info.assume_init() }.id;
        let normalized = unsafe { (api.get_param)(instance, id) };
        let plain = unsafe { (api.to_plain)(id, normalized) };
        let mut text = [0u8; 128];
        let n = unsafe { (api.format_param)(id, normalized, text.as_mut_ptr(), text.len() as u32) }.max(0) as usize;
        let text = String::from_utf8_lossy(&text[..n]).replace('\\', "\\\\").replace('"', "\\\"");
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(&format!("[{id},{normalized},{plain},\"{text}\"]"));
    }
    out.push(']');
    env.new_string(out).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// An edit from the editor: a plain value (dB, an index, ...).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeSet(_env: JNIEnv, _class: JClass, handle: jlong, id: jint, plain: jdouble) {
    let Some(app) = app(handle) else { return };
    let (api, instance) = (app.shared.api, app.shared.instance);
    let normalized = unsafe { (api.to_normalized)(id as u32, plain) };
    unsafe { (api.set_param)(instance, id as u32, normalized) };
}

/// MIDI bytes from a MIDI device or the on-screen keyboard.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ferritron_app_FerritronActivity_nativeMidi(env: JNIEnv, _class: JClass, handle: jlong, data: JByteArray, offset: jint, count: jint) {
    let Some(app) = app(handle) else { return };
    let Ok(bytes) = env.convert_byte_array(&data) else { return };
    let (start, end) = (offset.max(0) as usize, (offset + count).max(0) as usize);
    let Some(bytes) = bytes.get(start..end.min(bytes.len())) else { return };
    // A buffer may hold several messages: notes are 3 bytes each.
    let mut i = 0;
    while i < bytes.len() {
        let status = bytes[i];
        let len = match status & 0xf0 {
            0xc0 | 0xd0 => 2,
            0xf0 => 1,
            s if s >= 0x80 => 3,
            _ => 1,
        };
        if let Some(message) = bytes.get(i..i + len) {
            app.shared.midi(message);
        }
        i += len;
    }
}
