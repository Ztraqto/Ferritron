//! macOS and iOS: Core Audio (the default output device, or RemoteIO on
//! iOS, with the input for effects), Core MIDI (every source), and the app's
//! window with the plugin's editor (AppKit; UIKit full screen on iOS).
//!
//! Written and type-checked, never built or run on a Mac.

#![allow(non_upper_case_globals, non_snake_case)]

use crate::engine::{Renderer, Shared, view_host};
use ferritron_core::abi::{EXT_VIEW, PluginApi, ViewApi, ViewHost};
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use std::ffi::{CString, c_char, c_void};
use std::sync::OnceLock;

type OSStatus = i32;
type AudioUnit = *mut c_void;
type AudioComponent = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    kind: u32,
    subtype: u32,
    manufacturer: u32,
    flags: u32,
    mask: u32,
}

#[repr(C)]
#[derive(Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList2 {
    count: u32,
    buffers: [AudioBuffer; 2],
}

type RenderProc = unsafe extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList2) -> OSStatus;

#[repr(C)]
struct AURenderCallbackStruct {
    proc_: RenderProc,
    refcon: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: AudioComponent, desc: *const AudioComponentDescription) -> AudioComponent;
    fn AudioComponentInstanceNew(component: AudioComponent, unit: *mut AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OSStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitRender(unit: AudioUnit, flags: *mut u32, time: *const c_void, bus: u32, frames: u32, data: *mut AudioBufferList2) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(object: u32, address: *const [u32; 3], qualifier_size: u32, qualifier: *const c_void, size: *mut u32, data: *mut c_void) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: *const c_void, text: *const c_char, encoding: u32) -> *const c_void;
}

#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    fn MIDIClientCreate(name: *const c_void, notify: *const c_void, refcon: *mut c_void, client: *mut u32) -> OSStatus;
    fn MIDIInputPortCreate(client: u32, name: *const c_void, read: unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void), refcon: *mut c_void, port: *mut u32) -> OSStatus;
    fn MIDIGetNumberOfSources() -> usize;
    fn MIDIGetSource(index: usize) -> u32;
    fn MIDIPortConnectSource(port: u32, source: u32, refcon: *mut c_void) -> OSStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const kAudioUnitType_Output: u32 = fourcc(b"auou");
const kAudioUnitManufacturer_Apple: u32 = fourcc(b"appl");
#[cfg(target_os = "macos")]
const kAudioUnitSubType_HALOutput: u32 = fourcc(b"ahal");
#[cfg(target_os = "macos")]
const kAudioUnitSubType_DefaultOutput: u32 = fourcc(b"def ");
#[cfg(target_os = "ios")]
const kAudioUnitSubType_RemoteIO: u32 = fourcc(b"rioc");
const kAudioUnitProperty_StreamFormat: u32 = 8;
const kAudioUnitProperty_SetRenderCallback: u32 = 23;
const kAudioUnitProperty_MaximumFramesPerSlice: u32 = 14;
const kAudioOutputUnitProperty_EnableIO: u32 = 2003;
#[cfg(target_os = "macos")]
const kAudioOutputUnitProperty_CurrentDevice: u32 = 2000;
#[cfg(target_os = "macos")]
const kAudioOutputUnitProperty_SetInputCallback: u32 = 2005;
const kAudioUnitScope_Global: u32 = 0;
const kAudioUnitScope_Input: u32 = 1;
const kAudioUnitScope_Output: u32 = 2;
const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
const kAudioFormatFlagsFloatNonInterleaved: u32 = 1 | 8 | 32; // float, packed, non-interleaved
const kCFStringEncodingUTF8: u32 = 0x0800_0100;

fn cf_string(text: &str) -> *const c_void {
    let c = CString::new(text).unwrap_or_default();
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) }
}

/// What the audio callbacks get.
struct Audio {
    shared: &'static Shared,
    renderer: Renderer,
    /// The unit whose input bus holds the input (iOS RemoteIO), or the input unit (macOS).
    input: AudioUnit,
    scratch: [Vec<f32>; 2],
}

fn float_format(sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate,
        format: kAudioFormatLinearPCM,
        flags: kAudioFormatFlagsFloatNonInterleaved,
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels: 2,
        bits: 32,
        reserved: 0,
    }
}

/// Pulls `frames` of input from `unit`'s bus 1 into the ring.
unsafe fn pull_input(audio: &mut Audio, flags: *mut u32, time: *const c_void, frames: u32) {
    let n = (frames as usize).min(audio.scratch[0].len());
    let [left, right] = &mut audio.scratch;
    let mut list = AudioBufferList2 {
        count: 2,
        buffers: [
            AudioBuffer { channels: 1, size: n as u32 * 4, data: left.as_mut_ptr().cast() },
            AudioBuffer { channels: 1, size: n as u32 * 4, data: right.as_mut_ptr().cast() },
        ],
    };
    if unsafe { AudioUnitRender(audio.input, flags, time, 1, n as u32, &mut list) } == 0 {
        for i in 0..n {
            audio.shared.push_input(&[left[i], right[i]], 2);
        }
    }
}

unsafe extern "C" fn render(refcon: *mut c_void, flags: *mut u32, time: *const c_void, _bus: u32, frames: u32, data: *mut AudioBufferList2) -> OSStatus {
    let audio = unsafe { &mut *(refcon as *mut Audio) };
    if cfg!(target_os = "ios") && !audio.input.is_null() {
        unsafe { pull_input(audio, flags, time, frames) };
    }
    let list = unsafe { &mut *data };
    if list.count < 2 {
        return 0;
    }
    let n = frames as usize;
    let left = unsafe { std::slice::from_raw_parts_mut(list.buffers[0].data as *mut f32, n) };
    let right = unsafe { std::slice::from_raw_parts_mut(list.buffers[1].data as *mut f32, n) };
    audio.renderer.render_split(left, right);
    0
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn capture(refcon: *mut c_void, flags: *mut u32, time: *const c_void, _bus: u32, frames: u32, _data: *mut AudioBufferList2) -> OSStatus {
    let audio = unsafe { &mut *(refcon as *mut Audio) };
    unsafe { pull_input(audio, flags, time, frames) };
    0
}

fn unit(subtype: u32) -> Option<AudioUnit> {
    let desc = AudioComponentDescription { kind: kAudioUnitType_Output, subtype, manufacturer: kAudioUnitManufacturer_Apple, flags: 0, mask: 0 };
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
    let mut unit: AudioUnit = std::ptr::null_mut();
    (!component.is_null() && unsafe { AudioComponentInstanceNew(component, &mut unit) } == 0).then_some(unit)
}

fn set<T>(unit: AudioUnit, id: u32, scope: u32, element: u32, value: &T) -> bool {
    unsafe { AudioUnitSetProperty(unit, id, scope, element, (value as *const T).cast(), size_of::<T>() as u32) == 0 }
}

/// Opens the output (and input, for effects) and starts processing.
fn start_audio(shared: &'static Shared) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let output = unit(kAudioUnitSubType_DefaultOutput).ok_or("no audio output device")?;
    #[cfg(target_os = "ios")]
    let output = unit(kAudioUnitSubType_RemoteIO).ok_or("no audio output")?;
    let mut device_format = AudioStreamBasicDescription::default();
    let mut size = size_of::<AudioStreamBasicDescription>() as u32;
    unsafe { AudioUnitGetProperty(output, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0, (&raw mut device_format).cast(), &mut size) };
    let sample_rate = if device_format.sample_rate > 0.0 { device_format.sample_rate } else { 48_000.0 };
    if !shared.activate(sample_rate) {
        return Err("the plugin could not be activated".into());
    }
    let format = float_format(sample_rate);
    set(output, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &format);
    set(output, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0, &4096u32);
    let audio: &'static mut Audio =
        Box::leak(Box::new(Audio { shared, renderer: Renderer::new(shared), input: std::ptr::null_mut(), scratch: [vec![0.0; 4096], vec![0.0; 4096]] }));
    let refcon: *mut c_void = (&raw mut *audio).cast();
    if !shared.instrument {
        #[cfg(target_os = "ios")]
        {
            // RemoteIO's bus 1 is the microphone.
            if set(output, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, 1, &1u32) {
                set(output, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 1, &format);
                audio.input = output;
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(input) = unit(kAudioUnitSubType_HALOutput) {
            let mut device = 0u32;
            let mut size = 4u32;
            let address = [fourcc(b"dIn "), fourcc(b"glob"), 0];
            unsafe { AudioObjectGetPropertyData(1, &address, 0, std::ptr::null(), &mut size, (&raw mut device).cast()) };
            let callback = AURenderCallbackStruct { proc_: capture, refcon };
            if set(input, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, 1, &1u32)
                && set(input, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output, 0, &0u32)
                && set(input, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &device)
                && set(input, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 1, &format)
                && set(input, kAudioOutputUnitProperty_SetInputCallback, kAudioUnitScope_Global, 0, &callback)
                && unsafe { AudioUnitInitialize(input) } == 0
            {
                audio.input = input;
                unsafe { AudioOutputUnitStart(input) };
            }
        }
    }
    let callback = AURenderCallbackStruct { proc_: render, refcon };
    set(output, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &callback);
    if unsafe { AudioUnitInitialize(output) } != 0 || unsafe { AudioOutputUnitStart(output) } != 0 {
        return Err("the audio output could not be started".into());
    }
    Ok(())
}

/// Notes from every Core MIDI source.
fn start_midi(shared: &'static Shared) {
    unsafe extern "C" fn read(list: *const c_void, refcon: *mut c_void, _source: *mut c_void) {
        let shared = unsafe { &*(refcon as *const Shared) };
        // MIDIPacketList: numPackets, then packets (timeStamp u64, length u16, data), 4-byte aligned on arm64.
        let count = unsafe { *(list as *const u32) };
        let mut at = unsafe { (list as *const u8).add(4) };
        for _ in 0..count {
            let length = unsafe { std::ptr::read_unaligned(at.add(8) as *const u16) } as usize;
            let data = unsafe { std::slice::from_raw_parts(at.add(10), length) };
            for message in data.chunks(3) {
                shared.midi(message);
            }
            let next = at as usize + 10 + length;
            at = (if cfg!(target_arch = "aarch64") { (next + 3) & !3 } else { next }) as *const u8;
        }
    }
    let (mut client, mut port) = (0u32, 0u32);
    unsafe {
        if MIDIClientCreate(cf_string("Ferritron"), std::ptr::null(), std::ptr::null_mut(), &mut client) != 0 {
            return;
        }
        if MIDIInputPortCreate(client, cf_string("in"), read, shared as *const Shared as *mut c_void, &mut port) != 0 {
            return;
        }
        for i in 0..MIDIGetNumberOfSources() {
            MIDIPortConnectSource(port, MIDIGetSource(i), std::ptr::null_mut());
        }
    }
}

static SHARED: OnceLock<&'static Shared> = OnceLock::new();

/// Opens the plugin's editor in `parent` (NSView / UIView) of `width` x `height` points.
fn open_editor(shared: &'static Shared, parent: *mut c_void, width: u32, height: u32) {
    let Some(api) = (unsafe { ((shared.api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi).as_ref() }) else { return };
    let host: &'static ViewHost = Box::leak(Box::new(view_host::for_shared(shared)));
    // Editors take points on Apple platforms (the scale is the screen's).
    unsafe { (api.open)(parent, host, width, height, 1.0) };
}

fn class(name: &std::ffi::CStr) -> &'static AnyClass {
    AnyClass::get(name).unwrap_or_else(|| panic!("no class {name:?}"))
}

pub fn run(api: &'static PluginApi) -> Result<(), String> {
    let shared: &'static Shared = Box::leak(Shared::new(api).ok_or("the plugin could not create an instance")?);
    let _ = SHARED.set(shared);
    #[cfg(target_os = "ios")]
    return ios::run();
    #[cfg(target_os = "macos")]
    return mac::run(shared, api);
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    unsafe extern "C" fn should_quit(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject) -> Bool {
        Bool::YES
    }

    unsafe extern "C" fn will_terminate(_this: *mut AnyObject, _cmd: Sel, _note: *mut AnyObject) {
        if let Some(shared) = SHARED.get() {
            shared.save();
        }
    }

    fn delegate() -> *mut AnyObject {
        let class = AnyClass::get(c"FerritronAppDelegate").unwrap_or_else(|| {
            let mut builder = ClassBuilder::new(c"FerritronAppDelegate", class(c"NSObject")).expect("class");
            unsafe {
                builder.add_method(sel!(applicationShouldTerminateAfterLastWindowClosed:), should_quit as unsafe extern "C" fn(_, _, _) -> _);
                builder.add_method(sel!(applicationWillTerminate:), will_terminate as unsafe extern "C" fn(_, _, _));
            }
            builder.register()
        });
        unsafe { msg_send![class, new] }
    }

    pub fn run(shared: &'static Shared, api: &'static PluginApi) -> Result<(), String> {
        unsafe {
            let app: *mut AnyObject = msg_send![class(c"NSApplication"), sharedApplication];
            let _: Bool = msg_send![app, setActivationPolicy: 0isize]; // regular
            let _: () = msg_send![app, setDelegate: delegate()];
            let view = ((api.get_extension)(EXT_VIEW.as_ptr()) as *const ViewApi).as_ref();
            let (w, h) = view.map_or((420, 160), |v| (v.width, v.height));
            let resizable = view.is_some_and(|v| v.min_width != v.max_width || v.min_height != v.max_height);
            let rect = CGRect { origin: CGPoint { x: 200.0, y: 200.0 }, size: CGSize { width: w as f64, height: h as f64 } };
            let style: usize = 1 | 2 | 4 | if resizable { 8 } else { 0 }; // titled, closable, miniaturizable, resizable
            let window: *mut AnyObject = msg_send![class(c"NSWindow"), alloc];
            let window: *mut AnyObject = msg_send![window, initWithContentRect: rect, styleMask: style, backing: 2usize, defer: Bool::NO];
            let title: *mut AnyObject = msg_send![class(c"NSString"), stringWithUTF8String: CString::new(crate::plugin_name(api)).unwrap_or_default().as_ptr()];
            let _: () = msg_send![window, setTitle: title];
            let _: () = msg_send![window, setReleasedWhenClosed: Bool::NO];
            let content: *mut AnyObject = msg_send![window, contentView];
            super::start_audio(shared)?;
            super::start_midi(shared);
            super::open_editor(shared, content.cast(), w, h);
            let _: () = msg_send![window, center];
            let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![app, activateIgnoringOtherApps: Bool::YES];
            let _: () = msg_send![app, run];
        }
        shared.save();
        Ok(())
    }
}

#[cfg(target_os = "ios")]
mod ios {
    use super::*;

    #[link(name = "UIKit", kind = "framework")]
    unsafe extern "C" {
        fn UIApplicationMain(argc: i32, argv: *mut *mut c_char, principal: *const c_void, delegate: *const c_void) -> i32;
    }

    #[link(name = "AVFAudio", kind = "framework")]
    unsafe extern "C" {
        static AVAudioSessionCategoryPlayAndRecord: *mut AnyObject;
        static AVAudioSessionCategoryPlayback: *mut AnyObject;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    unsafe impl objc2::Encode for Rect {
        const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
            "CGRect",
            &[
                objc2::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
                objc2::Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
            ],
        );
    }

    unsafe extern "C" fn did_finish(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject, _options: *mut AnyObject) -> Bool {
        let Some(&shared) = SHARED.get() else { return Bool::NO };
        unsafe {
            let session: *mut AnyObject = msg_send![class(c"AVAudioSession"), sharedInstance];
            let category = if shared.instrument { AVAudioSessionCategoryPlayback } else { AVAudioSessionCategoryPlayAndRecord };
            let _: Bool = msg_send![session, setCategory: category, error: std::ptr::null_mut::<*mut AnyObject>()];
            let _: Bool = msg_send![session, setActive: Bool::YES, error: std::ptr::null_mut::<*mut AnyObject>()];
            let screen: *mut AnyObject = msg_send![class(c"UIScreen"), mainScreen];
            let bounds: Rect = msg_send![screen, bounds];
            let window: *mut AnyObject = msg_send![class(c"UIWindow"), alloc];
            let window: *mut AnyObject = msg_send![window, initWithFrame: bounds];
            let controller: *mut AnyObject = msg_send![class(c"UIViewController"), new];
            let _: () = msg_send![window, setRootViewController: controller];
            let view: *mut AnyObject = msg_send![controller, view];
            let _: () = msg_send![window, makeKeyAndVisible];
            if let Err(e) = super::start_audio(shared) {
                eprintln!("ferritron: {e}");
            }
            super::start_midi(shared);
            super::open_editor(shared, view.cast(), bounds.w as u32, bounds.h as u32);
            // The window stays: it was allocated here (+1) and is never released.
        }
        Bool::YES
    }

    unsafe extern "C" fn background(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject) {
        if let Some(shared) = SHARED.get() {
            shared.save();
        }
    }

    pub fn run() -> Result<(), String> {
        let mut builder = ClassBuilder::new(c"FerritronAppDelegate", class(c"UIResponder")).ok_or("cannot define the app delegate")?;
        unsafe {
            builder.add_method(sel!(application:didFinishLaunchingWithOptions:), did_finish as unsafe extern "C" fn(_, _, _, _) -> _);
            builder.add_method(sel!(applicationDidEnterBackground:), background as unsafe extern "C" fn(_, _, _));
            builder.add_method(sel!(applicationWillTerminate:), background as unsafe extern "C" fn(_, _, _));
        }
        builder.register();
        unsafe { UIApplicationMain(0, std::ptr::null_mut(), std::ptr::null(), cf_string("FerritronAppDelegate")) };
        Ok(())
    }
}
