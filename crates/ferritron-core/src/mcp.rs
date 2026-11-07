//! MCP-native plugins (design doc, Phase 6): plugin side.
//!
//! One Ferritron MCP hub runs per computer (`ferritron-mcp`, the server AI
//! clients such as Claude Code or Codex are configured with). Plugins that
//! declare `[mcp]` in the project file get an "AI Access" switch, off for
//! every new instance (and saved with the project). While an instance has it on,
//! this process's agent connects to the hub on localhost, announces the
//! instance (which DAW process, which plugin, which track) and serves the
//! hub's requests: read and set parameters (reported to the DAW like edits
//! from the editor), and capture the audio before and after the plugin.
//!
//! One agent thread serves every instance in the process; it exists while
//! MCP-capable instances exist and stops with the last one (no thread
//! outlives the plugin library). The audio thread only copies samples into
//! capture rings, allocated when the switch is first turned on.

use serde_json::{Value, json};
use std::cell::UnsafeCell;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default hub port on 127.0.0.1 (`FERRITRON_MCP_PORT` overrides).
pub const DEFAULT_PORT: u16 = 47_801;
/// Seconds of audio kept for `capture`.
const CAPTURE_SECONDS: f64 = 10.0;
/// Channels kept for `capture`.
const CAPTURE_CHANNELS: usize = 2;

/// Where the hub keeps the token that plugins and bridges must present:
/// `FERRITRON_MCP_TOKEN` (a path), else `<Ferritron data folder>/mcp-token`.
pub fn token_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FERRITRON_MCP_TOKEN") {
        return Some(PathBuf::from(path));
    }
    crate::i18n::data_dir().map(|d| d.join("mcp-token"))
}

pub fn port() -> u16 {
    std::env::var("FERRITRON_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

pub use crate::instance::HostCallbacks;

/// Pre- and post-processing audio of one instance, a ring per channel.
/// Written only by the audio thread; read (approximately) by the agent.
pub(crate) struct Tap {
    rings: UnsafeCell<Vec<Vec<f32>>>, // pre channels, then post channels
    len: usize,
    written: AtomicU64,
    pub sample_rate: f64,
}

// SAFETY: one writer (the audio thread); readers accept torn samples.
unsafe impl Sync for Tap {}

impl Tap {
    fn new(sample_rate: f64) -> Self {
        let len = (sample_rate * CAPTURE_SECONDS) as usize;
        Self { rings: UnsafeCell::new(vec![vec![0.0; len.max(1)]; CAPTURE_CHANNELS * 2]), len: len.max(1), written: AtomicU64::new(0), sample_rate }
    }

    /// Stores one channel of a block (audio thread). `post` selects the ring.
    pub(crate) fn write(&self, channel: usize, post: bool, samples: &[f32]) {
        if channel >= CAPTURE_CHANNELS {
            return;
        }
        // SAFETY: only the audio thread writes.
        let rings = unsafe { &mut *self.rings.get() };
        let ring = &mut rings[channel + if post { CAPTURE_CHANNELS } else { 0 }];
        let start = self.written.load(Ordering::Relaxed) as usize;
        for (i, &s) in samples.iter().enumerate() {
            ring[(start + i) % self.len] = s;
        }
    }

    /// Marks a block of `frames` as written (after both pre and post).
    pub(crate) fn advance(&self, frames: usize) {
        self.written.fetch_add(frames as u64, Ordering::Release);
    }

    /// The last `frames` frames: (pre channels, post channels).
    fn read(&self, frames: usize) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let end = self.written.load(Ordering::Acquire) as usize;
        let frames = frames.min(self.len).min(end);
        // SAFETY: reading while the audio thread writes may tear a sample at
        // the edge; acceptable for analysis.
        let rings = unsafe { &*self.rings.get() };
        let take = |ring: &Vec<f32>| (end - frames..end).map(|i| ring[i % self.len]).collect::<Vec<f32>>();
        let (pre, post) = rings.split_at(CAPTURE_CHANNELS);
        (pre.iter().map(take).collect(), post.iter().map(take).collect())
    }
}

/// Per-instance data shared between the plugin instance and the agent.
pub(crate) struct Shared {
    pub handle: u64,
    pub tap: AtomicPtr<Tap>,
    /// The instance's link: track name, host edits.
    pub link: Arc<crate::instance::Link>,
}

impl Shared {
    pub(crate) fn new(link: Arc<crate::instance::Link>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self { handle: NEXT.fetch_add(1, Ordering::Relaxed), tap: AtomicPtr::new(std::ptr::null_mut()), link }
    }

    /// The capture rings, for the audio thread (None until enabled).
    pub(crate) fn tap(&self) -> Option<&Tap> {
        unsafe { self.tap.load(Ordering::Acquire).as_ref() }
    }

    fn ensure_tap(&self, sample_rate: f64) {
        if self.tap.load(Ordering::Acquire).is_null() {
            let tap = Box::into_raw(Box::new(Tap::new(sample_rate)));
            if self.tap.compare_exchange(std::ptr::null_mut(), tap, Ordering::AcqRel, Ordering::Acquire).is_err() {
                drop(unsafe { Box::from_raw(tap) });
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let tap = self.tap.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !tap.is_null() {
            drop(unsafe { Box::from_raw(tap) });
        }
    }
}

/// What the agent needs from an instance (implemented in `abi` for each plugin type).
pub(crate) trait Endpoint: Send + Sync {
    fn shared(&self) -> &Shared;
    /// The "AI Access" switch.
    fn enabled(&self) -> bool;
    fn sample_rate(&self) -> f64;
    /// Plugin id, name, vendor.
    fn identity(&self) -> (&'static str, &'static str, &'static str);
    /// Every parameter the AI may touch: key, name, unit, min, max, value, text.
    fn parameters(&self) -> Vec<Value>;
    /// Sets a parameter (by key, name or id) to a plain value; returns its new state.
    fn set(&self, parameter: &str, value: f64) -> Result<Value, String>;
}

struct Agent {
    endpoints: Vec<Arc<dyn Endpoint>>,
    thread: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

static AGENT: Mutex<Agent> = Mutex::new(Agent { endpoints: Vec::new(), thread: None });

/// Registers an MCP-capable instance; starts the agent thread with the first.
pub(crate) fn register(endpoint: Arc<dyn Endpoint>) {
    let mut agent = AGENT.lock().unwrap_or_else(PoisonError::into_inner);
    agent.endpoints.push(endpoint);
    if agent.thread.is_none() {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        if let Ok(thread) = std::thread::Builder::new().name("ferritron-mcp".into()).spawn(move || run(&s)) {
            agent.thread = Some((stop, thread));
        }
    }
}

/// Unregisters an instance before it is freed; the last one stops and joins the thread.
pub(crate) fn unregister(handle: u64) {
    let thread = {
        let mut agent = AGENT.lock().unwrap_or_else(PoisonError::into_inner);
        agent.endpoints.retain(|e| e.shared().handle != handle);
        if agent.endpoints.is_empty() { agent.thread.take() } else { None }
    };
    if let Some((stop, thread)) = thread {
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }
}

fn endpoints() -> Vec<Arc<dyn Endpoint>> {
    AGENT.lock().unwrap_or_else(PoisonError::into_inner).endpoints.clone()
}

/// The DAW this plugin runs in.
fn process_info() -> Value {
    let exe = std::env::current_exe().ok();
    let name = exe.as_ref().and_then(|p| p.file_stem()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    json!({ "pid": std::process::id(), "name": name, "path": exe.map(|p| p.display().to_string()) })
}

fn describe(e: &dyn Endpoint) -> Value {
    let (id, name, vendor) = e.identity();
    let track = e.shared().link.track_name();
    json!({ "instance": e.shared().handle, "plugin_id": id, "plugin": name, "vendor": vendor, "track": track })
}

fn run(stop: &AtomicBool) {
    let mut link: Option<(TcpStream, BufReader<TcpStream>)> = None;
    let mut announced = String::new();
    let mut retry_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let enabled: Vec<Arc<dyn Endpoint>> = endpoints().into_iter().filter(|e| e.enabled()).collect();
        if enabled.is_empty() {
            // Nothing may be reached: drop the connection.
            link = None;
            announced.clear();
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        for e in &enabled {
            e.shared().ensure_tap(e.sample_rate());
        }
        if link.is_none() && Instant::now() >= retry_at {
            link = connect();
            announced.clear();
            if link.is_none() {
                retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
        let Some((stream, reader)) = link.as_mut() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        // Tell the hub which instances can be reached, when that changes.
        let list = Value::Array(enabled.iter().map(|e| describe(e.as_ref())).collect()).to_string();
        if list != announced {
            let message = json!({ "type": "instances", "instances": serde_json::from_str::<Value>(&list).unwrap_or_default() });
            if writeln!(stream, "{message}").is_err() {
                link = None;
                continue;
            }
            announced = list;
        }
        // Serve requests (the read times out so the list stays current).
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => link = None,
            Ok(_) => {
                let reply = serve(&line, &enabled);
                if writeln!(stream, "{reply}").is_err() {
                    link = None;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => link = None,
        }
    }
}

fn connect() -> Option<(TcpStream, BufReader<TcpStream>)> {
    let token = std::fs::read_to_string(token_path()?).ok()?;
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port()).into(), Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
    let hello = json!({ "type": "hello", "role": "plugins", "token": token.trim(), "process": process_info() });
    writeln!(stream, "{hello}").ok()?;
    let reader = BufReader::new(stream.try_clone().ok()?);
    Some((stream, reader))
}

/// Handles one request from the hub.
fn serve(line: &str, enabled: &[Arc<dyn Endpoint>]) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(line) else { return json!({ "type": "reply", "ok": false, "error": "bad request" }) };
    let id = request["id"].clone();
    let reply = |result: Result<Value, String>| match result {
        Ok(result) => json!({ "type": "reply", "id": id, "ok": true, "result": result }),
        Err(error) => json!({ "type": "reply", "id": id, "ok": false, "error": error }),
    };
    let instance = request["instance"].as_u64();
    // Only instances whose switch is on are reachable.
    let Some(endpoint) = enabled.iter().find(|e| Some(e.shared().handle) == instance) else {
        return reply(Err("no such instance with AI access on".into()));
    };
    let result = match request["op"].as_str() {
        Some("get_parameters") => Ok(json!({ "instance": describe(endpoint.as_ref()), "parameters": endpoint.parameters() })),
        Some("set_parameter") => match (request["parameter"].as_str(), request["value"].as_f64()) {
            (Some(p), Some(v)) => endpoint.set(p, v),
            _ => Err("set_parameter needs `parameter` and a numeric `value`".into()),
        },
        Some("capture_audio") => capture(endpoint.as_ref(), request["seconds"].as_f64().unwrap_or(2.0)),
        _ => Err("unknown operation".into()),
    };
    reply(result)
}

/// Writes the last `seconds` of audio before and after the plugin to WAV
/// files and returns their paths and levels.
fn capture(e: &dyn Endpoint, seconds: f64) -> Result<Value, String> {
    let tap = e.shared().tap().ok_or("nothing captured yet")?;
    let frames = (seconds.clamp(0.05, CAPTURE_SECONDS) * tap.sample_rate) as usize;
    let (pre, post) = tap.read(frames);
    let dir = std::env::temp_dir().join("ferritron-mcp");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut out = json!({ "sample_rate": tap.sample_rate, "frames": pre.first().map_or(0, Vec::len) });
    for (label, audio) in [("before", &pre), ("after", &post)] {
        let path = dir.join(format!("{}-{}-{label}.wav", e.shared().handle, stamp));
        write_wav(&path, audio, tap.sample_rate as u32).map_err(|e| e.to_string())?;
        out[label] = json!({ "wav": path.display().to_string(), "levels": levels(audio) });
    }
    Ok(out)
}

/// Peak and RMS per channel, in dBFS.
fn levels(audio: &[Vec<f32>]) -> Value {
    let db = |v: f64| if v > 0.0 { (20.0 * v.log10() * 100.0).round() / 100.0 } else { -200.0 };
    Value::Array(
        audio
            .iter()
            .map(|ch| {
                let peak = ch.iter().fold(0.0f32, |m, s| m.max(s.abs())) as f64;
                let rms = (ch.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / ch.len().max(1) as f64).sqrt();
                json!({ "peak_db": db(peak), "rms_db": db(rms) })
            })
            .collect(),
    )
}

fn write_wav(path: &std::path::Path, audio: &[Vec<f32>], rate: u32) -> std::io::Result<()> {
    let channels = audio.len().max(1) as u16;
    let frames = audio.first().map_or(0, Vec::len);
    let data_len = (frames * channels as usize * 4) as u32;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * channels as u32 * 4).to_le_bytes())?;
    w.write_all(&(channels * 4).to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for i in 0..frames {
        for ch in audio {
            w.write_all(&ch[i].to_le_bytes())?;
        }
    }
    w.flush()
}
