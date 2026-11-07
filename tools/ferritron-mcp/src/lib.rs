//! The Ferritron MCP hub (design doc, Phase 6).
//!
//! One hub runs per computer. AI clients (Claude Code, Codex, ...) start
//! `ferritron-mcp` as an MCP server over stdio; the first one becomes the
//! hub and listens on 127.0.0.1 for Ferritron plugins, later ones relay to
//! it. So an AI client is configured with this one server, whatever DAWs
//! and plugins are running.
//!
//! Plugins connect only while their "AI Access" switch is on, and report
//! which DAW process, plugin and track they are. Connections must present
//! the token the hub keeps in the user's Ferritron folder, so only the
//! user's own processes can take part.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long a plugin may take to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads the token, creating a random one if there is none yet.
pub fn ensure_token(path: &Path) -> std::io::Result<String> {
    if let Ok(token) = std::fs::read_to_string(path)
        && !token.trim().is_empty() {
            return Ok(token.trim().to_string());
        }
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::new();
    for i in 0..4u64 {
        // RandomState is seeded from the operating system's randomness.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i ^ std::process::id() as u64);
        token.push_str(&format!("{:016x}", h.finish()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, &token)?;
    Ok(token)
}

/// One DAW process with Ferritron plugins that have AI access on.
struct Connection {
    process: Value,
    instances: Mutex<Vec<Value>>,
    writer: Mutex<TcpStream>,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
}

#[derive(Default)]
struct State {
    connections: Mutex<HashMap<u64, Arc<Connection>>>,
    next: AtomicU64,
}

/// The hub: plugin connections, and the MCP tools on top of them.
#[derive(Clone)]
pub struct Hub {
    state: Arc<State>,
    token: Arc<String>,
}

impl Hub {
    /// Listens on 127.0.0.1:`port`. Fails if another hub already does.
    pub fn start(port: u16, token: String) -> std::io::Result<Hub> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let hub = Hub { state: Arc::new(State::default()), token: Arc::new(token) };
        let h = hub.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let h = h.clone();
                std::thread::spawn(move || h.serve(stream));
            }
        });
        Ok(hub)
    }

    /// One incoming connection: plugins of a DAW process, or a relay.
    fn serve(&self, stream: TcpStream) {
        let Ok(read) = stream.try_clone() else { return };
        let mut lines = BufReader::new(read).lines();
        let Some(Ok(first)) = lines.next() else { return };
        let Ok(hello) = serde_json::from_str::<Value>(&first) else { return };
        if hello["token"].as_str() != Some(self.token.as_str()) {
            return; // not one of the user's processes
        }
        match hello["role"].as_str() {
            Some("plugins") => self.serve_plugins(stream, hello["process"].clone(), lines),
            Some("bridge") => self.serve_bridge(stream, lines),
            _ => {}
        }
    }

    fn serve_plugins(&self, stream: TcpStream, process: Value, lines: std::io::Lines<BufReader<TcpStream>>) {
        let id = self.state.next.fetch_add(1, Ordering::Relaxed);
        let connection = Arc::new(Connection {
            process,
            instances: Mutex::new(Vec::new()),
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
        });
        self.state.connections.lock().unwrap_or_else(PoisonError::into_inner).insert(id, connection.clone());
        for line in lines {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            match message["type"].as_str() {
                Some("instances") => {
                    *connection.instances.lock().unwrap_or_else(PoisonError::into_inner) =
                        message["instances"].as_array().cloned().unwrap_or_default();
                }
                Some("reply") => {
                    let waiter = message["id"].as_u64().and_then(|r| connection.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&r));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(message);
                    }
                }
                _ => {}
            }
        }
        self.state.connections.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
    }

    fn serve_bridge(&self, mut stream: TcpStream, lines: std::io::Lines<BufReader<TcpStream>>) {
        for line in lines {
            let Ok(line) = line else { break };
            let Ok(call) = serde_json::from_str::<Value>(&line) else { continue };
            let reply = match self.call_tool(call["name"].as_str().unwrap_or(""), &call["arguments"]) {
                Ok(result) => json!({ "id": call["id"], "ok": true, "result": result }),
                Err(error) => json!({ "id": call["id"], "ok": false, "error": error }),
            };
            if writeln!(stream, "{reply}").is_err() {
                break;
            }
        }
    }

    /// Every reachable instance, with its DAW and track.
    fn list(&self) -> Vec<Value> {
        let connections = self.state.connections.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        for c in connections.values() {
            for i in c.instances.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                out.push(json!({
                    "handle": format!("{}:{}", c.process["pid"], i["instance"]),
                    "daw": c.process["name"],
                    "daw_pid": c.process["pid"],
                    "plugin": i["plugin"],
                    "plugin_id": i["plugin_id"],
                    "vendor": i["vendor"],
                    "track": i["track"],
                }));
            }
        }
        out.sort_by_key(|v| v["handle"].as_str().unwrap_or_default().to_string());
        out
    }

    /// Sends `op` to the instance behind `handle` ("<pid>:<instance>") and waits.
    fn request(&self, handle: &str, op: &str, mut extra: Value) -> Result<Value, String> {
        let (pid, instance) = handle.split_once(':').ok_or("handle must look like <pid>:<instance>; see list_plugins")?;
        let (pid, instance) = (pid.parse::<u64>().map_err(|_| "bad handle")?, instance.parse::<u64>().map_err(|_| "bad handle")?);
        let connection = {
            let connections = self.state.connections.lock().unwrap_or_else(PoisonError::into_inner);
            connections
                .values()
                .find(|c| {
                    c.process["pid"].as_u64() == Some(pid)
                        && c.instances.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|i| i["instance"].as_u64() == Some(instance))
                })
                .cloned()
                .ok_or("no such plugin with AI access on (see list_plugins)")?
        };
        let id = self.state.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        connection.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        extra["type"] = json!("call");
        extra["id"] = json!(id);
        extra["instance"] = json!(instance);
        extra["op"] = json!(op);
        writeln!(connection.writer.lock().unwrap_or_else(PoisonError::into_inner), "{extra}").map_err(|e| e.to_string())?;
        let reply = rx.recv_timeout(REPLY_TIMEOUT).map_err(|_| "the plugin did not answer in time")?;
        if reply["ok"].as_bool() == Some(true) { Ok(reply["result"].clone()) } else { Err(reply["error"].as_str().unwrap_or("failed").to_string()) }
    }

    /// Runs one MCP tool.
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        let handle = || args["handle"].as_str().ok_or("`handle` is required (see list_plugins)");
        match name {
            "list_plugins" => Ok(Value::Array(self.list())),
            "get_parameters" => self.request(handle()?, "get_parameters", json!({})),
            "set_parameter" => {
                let parameter = args["parameter"].as_str().ok_or("`parameter` is required")?;
                let value = args["value"].as_f64().ok_or("`value` must be a number (in the parameter's own unit)")?;
                self.request(handle()?, "set_parameter", json!({ "parameter": parameter, "value": value }))
            }
            "capture_audio" => self.request(handle()?, "capture_audio", json!({ "seconds": args["seconds"].as_f64().unwrap_or(2.0) })),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }
}

/// A relay to the hub of this computer, for a second MCP server process.
pub struct Bridge {
    writer: Mutex<TcpStream>,
    reader: Mutex<std::io::Lines<BufReader<TcpStream>>>,
    next: AtomicU64,
}

impl Bridge {
    pub fn connect(port: u16, token: &str) -> std::io::Result<Bridge> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        writeln!(stream, "{}", json!({ "type": "hello", "role": "bridge", "token": token }))?;
        let reader = BufReader::new(stream.try_clone()?).lines();
        Ok(Bridge { writer: Mutex::new(stream), reader: Mutex::new(reader), next: AtomicU64::new(1) })
    }

    pub fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        writeln!(self.writer.lock().unwrap_or_else(PoisonError::into_inner), "{}", json!({ "id": id, "name": name, "arguments": args }))
            .map_err(|e| e.to_string())?;
        let line = self.reader.lock().unwrap_or_else(PoisonError::into_inner).next().ok_or("the hub went away")?.map_err(|e| e.to_string())?;
        let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if reply["ok"].as_bool() == Some(true) { Ok(reply["result"].clone()) } else { Err(reply["error"].as_str().unwrap_or("failed").to_string()) }
    }
}

/// The MCP tools, as listed to AI clients.
pub fn tools() -> Value {
    let handle = json!({ "type": "string", "description": "The instance, from list_plugins (\"<daw pid>:<instance>\")" });
    json!([
        {
            "name": "list_plugins",
            "description": "Lists the Ferritron plugins that currently allow AI access, in every DAW on this computer: DAW process, plugin, and track. Plugins appear only while the user has turned their \"AI Access\" switch on.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_parameters",
            "description": "Gets a plugin's parameters: key, name, unit, range and current value (plain units) with its display text.",
            "inputSchema": { "type": "object", "properties": { "handle": handle }, "required": ["handle"] }
        },
        {
            "name": "set_parameter",
            "description": "Sets a plugin parameter (by key, name or id) to a value in its own unit (e.g. dB). The DAW is told, as if the user had changed it.",
            "inputSchema": { "type": "object", "properties": {
                "handle": handle,
                "parameter": { "type": "string" },
                "value": { "type": "number" }
            }, "required": ["handle", "parameter", "value"] }
        },
        {
            "name": "capture_audio",
            "description": "Captures the last seconds of audio before and after the plugin (up to 10 s, while AI access is on): writes two WAV files and returns their paths with peak and RMS levels per channel.",
            "inputSchema": { "type": "object", "properties": {
                "handle": handle,
                "seconds": { "type": "number", "default": 2 }
            }, "required": ["handle"] }
        }
    ])
}

/// Answers one MCP (JSON-RPC 2.0) message; None for notifications.
pub fn handle_message(message: &Value, call: &dyn Fn(&str, &Value) -> Result<Value, String>) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "ferritron-mcp", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Controls Ferritron audio plugins running in DAWs on this computer. Call list_plugins first; only plugins whose AI Access switch the user turned on are listed."
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let empty = json!({});
            let args = message["params"].get("arguments").unwrap_or(&empty);
            match call(name, args) {
                Ok(result) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }] }),
                Err(error) => json!({ "content": [{ "type": "text", "text": error }], "isError": true }),
            }
        }
        other => return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {other}") } })),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Serves MCP over stdio until stdin closes.
pub fn serve_stdio(call: &dyn Fn(&str, &Value) -> Result<Value, String>) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(&message, call),
            Err(_) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })),
        };
        if let Some(reply) = reply
            && writeln!(stdout, "{reply}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
    }
}
