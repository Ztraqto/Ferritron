//! Test plugins without a host.
//!
//! [`Harness`] drives a plugin through the same processing path the format
//! shims use (block splitting at parameter changes, bypass, notes, panic
//! guard), so tests see exactly what a DAW would hear.
//!
//! ```ignore
//! let mut h = ferritron::testing::Harness::<MyPlugin>::new(48_000.0, 512);
//! h.set_param("gain", -6.0);
//! let mut left = [1.0; 64];
//! let mut right = [1.0; 64];
//! assert!(h.process(&mut [&mut left, &mut right]));
//! ```

use crate::__private::PluginMeta;
use crate::abi::{self, Event};
use crate::{ParamSet, Process};
use std::ffi::c_void;
use std::marker::PhantomData;

pub struct Harness<P: Process + PluginMeta> {
    instance: *mut c_void,
    events: Vec<Event>,
    _plugin: PhantomData<P>,
}

impl<P: Process + PluginMeta> Harness<P> {
    /// Creates and activates an instance for real-time processing.
    pub fn new(sample_rate: f64, max_block_size: usize) -> Self {
        Self::with_mode(sample_rate, max_block_size, false)
    }

    /// Creates and activates an instance for offline rendering, as a host
    /// does for a bounce ([`Setup::offline`](crate::Setup::offline)).
    pub fn offline(sample_rate: f64, max_block_size: usize) -> Self {
        Self::with_mode(sample_rate, max_block_size, true)
    }

    fn with_mode(sample_rate: f64, max_block_size: usize, offline: bool) -> Self {
        let instance = unsafe { abi::create::<P>() };
        assert!(!instance.is_null(), "creating the plugin failed (it panicked in Default)");
        let render = unsafe { abi::get_extension::<P>(abi::EXT_RENDER.as_ptr()) } as *const abi::RenderApi;
        if let Some(render) = unsafe { render.as_ref() } {
            unsafe { (render.set_offline)(instance, offline) };
        }
        unsafe { abi::activate::<P>(instance, sample_rate, max_block_size as u32) };
        Self { instance, events: Vec::new(), _plugin: PhantomData }
    }

    /// The host starts (`true`) or ends an offline render (export) from the
    /// next block on: `ctx.quality` follows at once (see [`Quality`](crate::Quality)).
    pub fn set_offline(&mut self, offline: bool) {
        let render = unsafe { abi::get_extension::<P>(abi::EXT_RENDER.as_ptr()) } as *const abi::RenderApi;
        if let Some(render) = unsafe { render.as_ref() } {
            unsafe { (render.set_offline)(self.instance, offline) };
        }
    }

    /// Whether the last `process` call slept (silence, see [`Tail`](crate::Tail)).
    pub fn sleeping(&self) -> bool {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_SLEEP.as_ptr()) } as *const abi::SleepApi;
        unsafe { ext.as_ref() }.is_some_and(|s| unsafe { (s.silent)(self.instance) })
    }

    /// The latency the plugin reports to hosts, in samples.
    pub fn latency(&self) -> u32 {
        let ext = unsafe { abi::get_extension::<P>(abi::EXT_LATENCY.as_ptr()) } as *const abi::LatencyApi;
        unsafe { ext.as_ref() }.map_or(0, |l| unsafe { (l.get)(self.instance) })
    }

    fn id(key: &str) -> u32 {
        // The built-in switches: "ferritron.bypass", "ferritron.mcp" (AI Access),
        // "ferritron.quality" (0 Auto, 1 Normal, 2 Best).
        if key.starts_with("ferritron.") {
            return crate::__private::param_id(key);
        }
        let infos = P::Params::infos();
        infos.iter().find(|i| i.key == key).unwrap_or_else(|| panic!("no parameter `{key}`")).id
    }

    /// Sets a parameter immediately, in plain units (e.g. dB).
    pub fn set_param(&mut self, key: &str, plain: f64) {
        let id = Self::id(key);
        unsafe { abi::set_param::<P>(self.instance, id, abi::to_normalized::<P>(id, plain)) };
    }

    /// Current value of a parameter in plain units.
    pub fn param(&self, key: &str) -> f64 {
        let id = Self::id(key);
        unsafe { abi::to_plain::<P>(id, abi::get_param::<P>(self.instance, id)) }
    }

    /// Schedules a parameter change at `offset` within the next `process` call.
    pub fn automate(&mut self, offset: u32, key: &str, plain: f64) {
        let id = Self::id(key);
        self.events.push(Event::param(offset, id, unsafe { abi::to_normalized::<P>(id, plain) }));
    }

    pub fn set_bypass(&mut self, on: bool) {
        unsafe { abi::set_param::<P>(self.instance, crate::__private::BYPASS_ID, if on { 1.0 } else { 0.0 }) };
    }

    /// Schedules a note on at `offset` within the next `process` call.
    pub fn note_on(&mut self, offset: u32, key: u8, velocity: f32) {
        self.events.push(Event::note(offset, true, key as i16, 0, velocity, -1));
    }

    pub fn note_off(&mut self, offset: u32, key: u8) {
        self.events.push(Event::note(offset, false, key as i16, 0, 0.0, -1));
    }

    /// Processes the channels in place with the scheduled events. Returns
    /// `false` if the plugin panicked (it then stays silent, as in a host).
    pub fn process(&mut self, channels: &mut [&mut [f32]]) -> bool {
        let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
        let ptrs: Vec<*mut f32> = channels.iter_mut().map(|c| c.as_mut_ptr()).collect();
        self.events.sort_by_key(|e| e.offset);
        let events = std::mem::take(&mut self.events);
        unsafe {
            abi::process::<P>(
                self.instance,
                ptrs.as_ptr(),
                ptrs.len() as u32,
                frames as u32,
                events.as_ptr(),
                events.len() as u32,
                std::ptr::null(),
            )
        }
    }

    /// The plugin's typed parameters (`h.params().gain.value()`).
    pub fn params(&self) -> &P::Params {
        unsafe { abi::params_of::<P>(self.instance) }
    }

    /// The instance's handle: identity, track, store, edits, messages.
    pub fn instance(&self) -> &crate::InstanceHandle {
        unsafe { abi::handle_of::<P>(self.instance) }
    }

    /// Tells the instance the track it sits on, as a host does.
    pub fn set_track(&mut self, track: crate::TrackInfo) {
        use crate::instance::{TRACK_HAS_COLOR, TRACK_HAS_INDEX, TRACK_HAS_NAME, TrackInfoC};
        let name = track.name.clone().unwrap_or_default();
        let info = TrackInfoC {
            flags: if track.name.is_some() { TRACK_HAS_NAME } else { 0 }
                | if track.color.is_some() { TRACK_HAS_COLOR } else { 0 }
                | if track.index.is_some() { TRACK_HAS_INDEX } else { 0 },
            name: abi::FerritronStr { ptr: name.as_ptr(), len: name.len() as u32 },
            color: track.color.unwrap_or_default(),
            index: track.index.unwrap_or_default(),
        };
        let api = unsafe { abi::get_extension::<P>(abi::EXT_TRACK.as_ptr()) } as *const abi::TrackApi;
        if let Some(api) = unsafe { api.as_ref() } {
            unsafe { (api.set_track_info)(self.instance, &info) };
        }
    }

    /// Sends a message to the plugin as its web editor does
    /// (`ferritron.send`), and returns what the plugin posted back
    /// (`InstanceHandle::post`) so far.
    pub fn send_message(&mut self, json: &str) -> Vec<String> {
        let handle = self.instance().clone();
        handle.deliver_message(json);
        handle.take_posted()
    }

    /// Saves the plugin state, as a host does when storing a project.
    pub fn save_state(&self) -> Vec<u8> {
        let size = unsafe { abi::save_state::<P>(self.instance, std::ptr::null_mut(), 0) };
        let mut data = vec![0; size as usize];
        unsafe { abi::save_state::<P>(self.instance, data.as_mut_ptr(), size) };
        data
    }

    pub fn load_state(&mut self, data: &[u8]) -> bool {
        unsafe { abi::load_state::<P>(self.instance, data.as_ptr(), data.len() as u32) }
    }
}

impl<P: Process + PluginMeta> Drop for Harness<P> {
    fn drop(&mut self) {
        unsafe { abi::destroy::<P>(self.instance) };
    }
}
