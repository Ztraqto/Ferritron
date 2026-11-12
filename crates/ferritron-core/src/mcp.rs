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
use std::sync::{Arc, Mutex, PoisonError, RwLock};
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

/// The proof that a party holds `token`, for one step of the handshake: HMAC-SHA256 over
/// `role`, `a` and `b` (newline-separated), in lowercase hex. The token itself never crosses
/// the connection. `role` is "hub" or "client", so a proof for one direction is not one for the other.
pub fn proof(token: &str, role: &str, a: &str, b: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(format!("{role}\n{a}\n{b}").as_bytes());
    mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Checks a proof from the other side. The comparison looks at every byte, so its timing
/// does not show how many leading bytes were right.
pub fn verify(token: &str, role: &str, a: &str, b: &str, claimed: &str) -> bool {
    let expected = proof(token, role, a, b);
    expected.len() == claimed.len() && expected.bytes().zip(claimed.bytes()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// `bytes` random bytes from the operating system, in lowercase hex (`2 * bytes` characters).
/// Falls back to the hasher seeds of the standard library when the system's generator fails.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::fill(&mut buf).is_err() {
        return fallback_hex(bytes);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// `bytes` bytes of hex from the standard library's hasher seeds, used only if the system's
/// generator fails. Each `RandomState` is seeded from the operating system's randomness.
fn fallback_hex(bytes: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    let mut i = 0u64;
    while out.len() < bytes * 2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        out.push_str(&format!("{:016x}", h.finish()));
        i += 1;
    }
    out.truncate(bytes * 2);
    out
}

/// A fresh 128-bit nonce from the operating system's random source, in hex
/// (see `random_hex`). Each connection uses new ones, so a recorded proof cannot be replayed.
pub fn nonce() -> String {
    random_hex(16)
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
        // `len` is at least 1 (`Tap::new`). Copies in at most two spans; a later sample
        // overwrites an earlier one, as the per-sample loop did.
        let mut pos = self.written.load(Ordering::Relaxed) as usize % self.len;
        let mut rest = samples;
        while !rest.is_empty() {
            let n = rest.len().min(self.len - pos);
            ring[pos..pos + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            pos = if pos + n == self.len { 0 } else { pos + n };
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
    /// False once the instance is being freed (`unregister`). The agent holds this for reading
    /// while it touches the instance; `unregister` takes it for writing, so it waits for them.
    pub alive: RwLock<bool>,
    /// Taps replaced after a sample rate change. The audio thread may still write to one, so
    /// they are kept until the instance itself is dropped.
    retired: Mutex<Vec<Box<Tap>>>,
}

impl Shared {
    pub(crate) fn new(link: Arc<crate::instance::Link>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            handle: NEXT.fetch_add(1, Ordering::Relaxed),
            tap: AtomicPtr::new(std::ptr::null_mut()),
            link,
            alive: RwLock::new(true),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// The capture rings, for the audio thread (None until enabled).
    pub(crate) fn tap(&self) -> Option<&Tap> {
        unsafe { self.tap.load(Ordering::Acquire).as_ref() }
    }

    /// Makes a tap for `sample_rate` if there is none or the rate changed (agent thread only).
    fn ensure_tap(&self, sample_rate: f64) {
        let current = self.tap.load(Ordering::Acquire);
        if !current.is_null() && unsafe { (*current).sample_rate } == sample_rate {
            return;
        }
        let tap = Box::into_raw(Box::new(Tap::new(sample_rate)));
        self.tap.store(tap, Ordering::Release);
        if !current.is_null() {
            self.retired.lock().unwrap_or_else(PoisonError::into_inner).push(unsafe { Box::from_raw(current) });
        }
    }
}

/// Runs `f` while the instance is alive (holding its read lock); None once it was unregistered.
/// Any access to the instance itself (its switch, its sample rate) goes through here.
fn while_alive<T>(e: &dyn Endpoint, f: impl FnOnce() -> T) -> Option<T> {
    let alive = e.shared().alive.read().unwrap_or_else(PoisonError::into_inner);
    if *alive { Some(f()) } else { None }
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
/// Called from `destroy` (the host's thread, never the audio thread). It waits until the agent
/// has finished any request on the instance, so the instance can be freed after it returns.
pub(crate) fn unregister(handle: u64) {
    // The target is found and cloned under AGENT, which is released before the write lock
    // is taken (the agent takes AGENT, then the instance's read lock, never the other way).
    if let Some(target) = endpoints().into_iter().find(|e| e.shared().handle == handle) {
        *target.shared().alive.write().unwrap_or_else(PoisonError::into_inner) = false;
    }
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

/// What one read from the hub gave.
#[derive(Debug, PartialEq)]
enum HubLine {
    /// A whole request, with its newline.
    Line(String),
    /// The read timed out: the bytes of the line so far are kept in `pending`.
    Waiting,
    /// The hub closed the connection, or the read failed.
    Closed,
}

/// Reads up to the next newline. A read that times out keeps the bytes it already got in
/// `pending`, so the next call completes the same line instead of starting in its middle.
fn read_hub_line<R: BufRead>(reader: &mut R, pending: &mut Vec<u8>) -> HubLine {
    match reader.read_until(b'\n', pending) {
        Ok(0) => HubLine::Closed,
        Ok(_) if pending.last() == Some(&b'\n') => {
            let line = String::from_utf8_lossy(pending).into_owned();
            pending.clear();
            HubLine::Line(line)
        }
        // Without a newline, the stream ended; the next read reports it.
        Ok(_) => HubLine::Waiting,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => HubLine::Waiting,
        Err(_) => HubLine::Closed,
    }
}

fn run(stop: &AtomicBool) {
    let mut link: Option<(TcpStream, BufReader<TcpStream>)> = None;
    let mut announced = String::new();
    // The bytes of a request line read so far. It belongs to the connection, so it is emptied
    // whenever a connection is made.
    let mut pending: Vec<u8> = Vec::new();
    let mut retry_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let enabled: Vec<Arc<dyn Endpoint>> =
            endpoints().into_iter().filter(|e| while_alive(e.as_ref(), || e.enabled()) == Some(true)).collect();
        if enabled.is_empty() {
            // Nothing may be reached: drop the connection.
            link = None;
            announced.clear();
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        for e in &enabled {
            while_alive(e.as_ref(), || e.shared().ensure_tap(e.sample_rate()));
        }
        if link.is_none() && Instant::now() >= retry_at {
            link = connect();
            announced.clear();
            pending.clear();
            if link.is_none() {
                retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
        let Some((stream, reader)) = link.as_mut() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        // Tell the hub which instances can be reached, when that changes.
        let list = Value::Array(enabled.iter().filter_map(|e| while_alive(e.as_ref(), || describe(e.as_ref()))).collect()).to_string();
        if list != announced {
            let message = json!({ "type": "instances", "instances": serde_json::from_str::<Value>(&list).unwrap_or_default() });
            if writeln!(stream, "{message}").is_err() {
                link = None;
                continue;
            }
            announced = list;
        }
        // Serve requests (the read times out so the list stays current).
        match read_hub_line(reader, &mut pending) {
            HubLine::Line(line) => {
                let reply = serve(&line, &enabled);
                if writeln!(stream, "{reply}").is_err() {
                    link = None;
                }
            }
            HubLine::Waiting => {}
            HubLine::Closed => link = None,
        }
    }
}

/// Connects to the hub and proves both ways that each holds the token: the hub's challenge must
/// verify (so a program that merely listens on the port is refused), and our answer must verify
/// on the hub's side. Returns None on any failure; the agent retries later.
fn connect() -> Option<(TcpStream, BufReader<TcpStream>)> {
    let token = std::fs::read_to_string(token_path()?).ok()?;
    let token = token.trim();
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port()).into(), Duration::from_millis(300)).ok()?;
    // The handshake waits up to 2 s for each line; afterwards the run loop reads with 200 ms polls.
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let client_nonce = nonce();
    let hello = json!({ "type": "hello", "role": "plugins", "nonce": client_nonce, "process": process_info() });
    writeln!(stream, "{hello}").ok()?;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let challenge: Value = serde_json::from_str(&line).ok()?;
    let hub_nonce = challenge["nonce"].as_str()?;
    if challenge["type"].as_str() != Some("challenge") || !verify(token, "hub", &client_nonce, hub_nonce, challenge["proof"].as_str()?) {
        return None; // not the user's hub
    }
    let auth = json!({ "type": "auth", "proof": proof(token, "client", hub_nonce, &client_nonce) });
    writeln!(stream, "{auth}").ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
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
    // Held for the whole request: `unregister` waits for it before the instance is freed.
    let alive = endpoint.shared().alive.read().unwrap_or_else(PoisonError::into_inner);
    if !*alive {
        return reply(Err("no such instance with AI access on".into()));
    }
    let result = match request["op"].as_str() {
        Some("get_parameters") => Ok(json!({ "instance": describe(endpoint.as_ref()), "parameters": endpoint.parameters() })),
        Some("set_parameter") => match (request["parameter"].as_str(), request["value"].as_f64()) {
            (Some(p), Some(v)) => endpoint.set(p, v),
            _ => Err("set_parameter needs `parameter` and a numeric `value`".into()),
        },
        Some("capture_audio") => capture(endpoint.as_ref(), request["seconds"].as_f64().unwrap_or(2.0)),
        _ => Err("unknown operation".into()),
    };
    drop(alive);
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
    remove_old_captures(&dir);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut out = json!({ "sample_rate": tap.sample_rate, "frames": pre.first().map_or(0, Vec::len) });
    for (label, audio) in [("before", &pre), ("after", &post)] {
        let path = dir.join(format!("{}-{}-{label}.wav", e.shared().handle, stamp));
        write_wav(&path, audio, tap.sample_rate as u32).map_err(|e| e.to_string())?;
        out[label] = json!({ "wav": path.display().to_string(), "levels": levels(audio) });
    }
    Ok(out)
}

/// Deletes the WAV files of earlier captures that are over an hour old.
fn remove_old_captures(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3600));
        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_is_stable_and_depends_on_role_and_inputs() {
        let p = proof("token", "hub", "aa", "bb");
        assert_eq!(p.len(), 64, "HMAC-SHA256 in lowercase hex");
        assert_eq!(p, proof("token", "hub", "aa", "bb"));
        assert_ne!(p, proof("token", "client", "aa", "bb"), "a role changes the proof");
        assert_ne!(p, proof("token", "hub", "bb", "aa"), "the order of the nonces matters");
        assert_ne!(p, proof("other", "hub", "aa", "bb"), "another token gives another proof");
    }

    #[test]
    fn verify_accepts_only_the_right_proof() {
        let p = proof("token", "client", "n1", "n2");
        assert!(verify("token", "client", "n1", "n2", &p));
        assert!(!verify("token", "client", "n1", "n2", &p[1..]));
        assert!(!verify("token", "client", "n1", "n2", &proof("other", "client", "n1", "n2")));
    }

    /// A request that arrives in two parts, with a read timeout between them, is served whole.
    #[test]
    fn a_request_split_by_a_read_timeout_is_completed_not_cut() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut hub_side = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (plugin_side, _) = listener.accept().unwrap();
        plugin_side.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut reader = BufReader::new(plugin_side);
        let mut pending = Vec::new();

        hub_side.write_all(b"{\"id\":7,").unwrap();
        assert_eq!(read_hub_line(&mut reader, &mut pending), HubLine::Waiting, "the first part times out");
        assert_eq!(pending, b"{\"id\":7,", "the bytes read so far are kept");

        hub_side.write_all(b"\"op\":\"x\"}\n").unwrap();
        assert_eq!(read_hub_line(&mut reader, &mut pending), HubLine::Line("{\"id\":7,\"op\":\"x\"}\n".into()));
        assert!(pending.is_empty());

        drop(hub_side);
        assert_eq!(read_hub_line(&mut reader, &mut pending), HubLine::Closed, "the hub closed the connection");
    }

    /// `Tap::write` copies a block in at most two spans; it must give what the modulo loop gives.
    #[test]
    fn tap_write_matches_the_modulo_loop() {
        // At 1 Hz the ring is CAPTURE_SECONDS (10) samples long.
        let tap = Tap::new(1.0);
        assert_eq!(tap.len, 10);
        let mut reference = vec![0.0f32; tap.len];
        let mut total = 0usize;
        let mut value = 0.0f32;
        for n in [3usize, 10, 25, 3, 7] {
            let block: Vec<f32> = (0..n)
                .map(|_| {
                    value += 1.0;
                    value
                })
                .collect();
            for (i, &s) in block.iter().enumerate() {
                reference[(total + i) % tap.len] = s;
            }
            tap.write(0, false, &block);
            tap.advance(n);
            total += n;

            let frames = tap.len.min(total);
            let (pre, _post) = tap.read(frames);
            let expected: Vec<f32> = (total - frames..total).map(|i| reference[i % tap.len]).collect();
            assert_eq!(pre[0], expected, "after a block of {n} (total {total})");
        }
    }

    #[test]
    fn nonces_differ() {
        assert_eq!(nonce().len(), 32);
        assert_ne!(nonce(), nonce());
    }

    #[test]
    fn random_hex_is_hex_of_the_asked_length_and_differs() {
        let is_hex = |s: &str| s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let a = random_hex(16);
        assert_eq!(a.len(), 32, "16 bytes are 32 hex characters");
        assert!(is_hex(a.as_str()), "lowercase hex only");
        assert_ne!(a, random_hex(16), "two calls differ");
        assert_eq!(random_hex(0), "", "no bytes, no characters");
        let f = fallback_hex(16);
        assert_eq!(f.len(), 32, "the fallback gives the same length");
        assert!(is_hex(f.as_str()), "the fallback is lowercase hex too");
    }

    /// The plugin's side of the handshake against a hub that holds the token: it connects, and
    /// the hub sees no token on the wire, only a proof it can check.
    #[test]
    fn a_plugin_proves_the_token_to_the_hub() {
        let dir = std::env::temp_dir().join(format!("ferritron-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "hub-token\n").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        // SAFETY: only this test sets these variables, before the plugin connects.
        unsafe {
            std::env::set_var("FERRITRON_MCP_TOKEN", &token_file);
            std::env::set_var("FERRITRON_MCP_PORT", listener.local_addr().unwrap().port().to_string());
        }
        let hub = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
            let hello: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert!(hello.get("token").is_none(), "the token is not sent");
            let client_nonce = hello["nonce"].as_str().unwrap().to_string();
            let hub_nonce = nonce();
            let challenge = json!({ "type": "challenge", "nonce": hub_nonce, "proof": proof("hub-token", "hub", &client_nonce, &hub_nonce) });
            writeln!(stream, "{challenge}").unwrap();
            let auth: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert!(verify("hub-token", "client", &hub_nonce, &client_nonce, auth["proof"].as_str().unwrap()));
        });
        assert!(connect().is_some(), "a plugin with the right token is connected");
        hub.join().unwrap();
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("FERRITRON_MCP_TOKEN");
            std::env::remove_var("FERRITRON_MCP_PORT");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
