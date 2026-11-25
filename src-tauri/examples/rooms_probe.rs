//! A room with real guys in it: Claude Code and Codex on this computer, each in its own
//! session, taking turns the way rooms give them (src/runtime/rooms).
//!
//! `LITTLEGUYS_CLAUDE_PLUGIN=skip LITTLEGUYS_CODEX_ONLY=<folder> PROBE_FOLDER=<folder> cargo run --example rooms_probe`
//!
//! - `PROBE_FOLDER`   where both guys' sessions start: a folder Claude Code trusts (required)
//! - `PROBE_SAY`      what you write, `|` between messages; each waits for the room to go quiet
//! - `PROBE_LISTEN=1` Codex listens (gets a look when the room goes quiet)
//! - `PROBE_DATA`     littleguys data dir (default `$TMPDIR/littleguys-rooms-probe-<pid>`)
//! - `PROBE_WAIT`     seconds to wait for each round (default 240)
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use littleguys_lib::gateway::client::{Hooks, Phase, RpcError};
use littleguys_lib::gateways::{ClaudeCodeConfig, CodexConfig};
use littleguys_lib::runtime::claudecode::ClaudeCodeClient;
use littleguys_lib::runtime::codex::CodexClient;
use littleguys_lib::runtime::rooms::{Bots, RoomsClient};
use littleguys_lib::runtime::Runtime;
use serde_json::{json, Value};

const CLAUDE: &str = "claude-probe";
const CODEX: &str = "codex-probe";

/// The two bots, with what Activity keeps for a room: who's connected and which threads are working.
#[derive(Default)]
struct Probe {
    bots: Mutex<HashMap<String, Runtime>>,
    running: Mutex<HashSet<(String, String)>>,
    rooms: OnceLock<RoomsClient>,
}

impl Probe {
    fn hooks(self: &Arc<Self>, gateway: &'static str) -> Hooks {
        let me = self.clone();
        Hooks {
            status: Box::new(move |s| eprintln!("[{gateway}] {:?} {}", s.phase, s.message.clone().unwrap_or_default())),
            event: Box::new(move |name, payload, _| {
                let key = payload.get("sessionKey").and_then(Value::as_str).unwrap_or_default().to_string();
                let phase = payload.get("state").or_else(|| payload.get("phase")).and_then(Value::as_str).unwrap_or_default();
                if name == "chat" || name == "sessions.changed" {
                    eprintln!("[{gateway}] {name} {phase} {key} {}", payload.get("errorMessage").and_then(Value::as_str).unwrap_or_default());
                    let mut running = me.running.lock().unwrap();
                    match phase {
                        "start" | "status" | "delta" => {
                            running.insert((gateway.to_string(), key));
                        }
                        "end" | "final" | "error" | "aborted" => {
                            running.remove(&(gateway.to_string(), key));
                        }
                        _ => {}
                    }
                }
                if name == "exec.approval.requested" {
                    eprintln!("[{gateway}] wants approval: {}", payload["presentation"]["commandText"]);
                }
                if let Some(rooms) = me.rooms.get() {
                    rooms.on_bot_event(gateway, name, payload);
                }
            }),
            connected: Box::new(move || eprintln!("[{gateway}] connected")),
        }
    }
}

struct ProbeBots(Arc<Probe>);

impl Bots for ProbeBots {
    fn call(&self, gateway: &str, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let client = self.0.bots.lock().unwrap().get(gateway).cloned();
        let method = method.to_string();
        if method == "chat.send" {
            println!("\n---- to {gateway} ----\n{}\n----", params["message"].as_str().unwrap_or_default());
        }
        Box::pin(async move {
            match client {
                Some(client) => client.call(&method, params).await,
                None => Err(RpcError::new("UNAVAILABLE", "no such bot")),
            }
        })
    }

    fn guy(&self, gateway: &str, _agent: &str) -> Option<String> {
        self.0.bots.lock().unwrap().contains_key(gateway).then(|| if gateway == CLAUDE { "Claude".to_string() } else { "Codex".to_string() })
    }

    fn online(&self, gateway: &str) -> bool {
        self.0.bots.lock().unwrap().get(gateway).is_some_and(|c| c.status().phase == Phase::Connected)
    }

    fn steers(&self, gateway: &str) -> bool {
        self.0.bots.lock().unwrap().get(gateway).is_some_and(|c| c.status().capabilities.steer)
    }

    fn is_running(&self, gateway: &str, key: &str) -> bool {
        self.0.running.lock().unwrap().contains(&(gateway.to_string(), key.to_string()))
    }
}

fn fail(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Set for everything this probe starts. A copy of the probe that finds it was started by
/// the probe, not by you, and stops: a probe must never be able to start probes.
const STARTED_BY_PROBE: &str = "LITTLEGUYS_ROOMS_PROBE";

fn main() -> anyhow::Result<()> {
    // The runtimes build their search index in place outside the app (search.rs). If one ever
    // starts this binary for it anyway, that copy builds the index and exits, like the app's.
    if let Some(code) = littleguys_lib::runtime::search::child_main() {
        std::process::exit(code);
    }
    if std::env::var_os(STARTED_BY_PROBE).is_some() {
        eprintln!("rooms_probe was started by a rooms_probe: not running.");
        std::process::exit(2);
    }
    std::env::set_var(STARTED_BY_PROBE, "1");
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=warn")).init();
    let folder = std::env::var("PROBE_FOLDER").map_err(|_| anyhow::anyhow!("set PROBE_FOLDER to a folder Claude Code trusts"))?;
    let data = std::env::var("PROBE_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("littleguys-rooms-probe-{}", std::process::id())));
    std::fs::create_dir_all(&data)?;
    let wait = Duration::from_secs(std::env::var("PROBE_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(240));

    let probe = Arc::new(Probe::default());
    let claude = Runtime::ClaudeCode(ClaudeCodeClient::new(CLAUDE.into(), "Claude Code".into(), ClaudeCodeConfig { approvals: false }, data.clone(), probe.hooks(CLAUDE)));
    let codex = Runtime::Codex(CodexClient::new(CODEX.into(), "Codex".into(), CodexConfig { approvals: false }, data.clone(), probe.hooks(CODEX)));
    for (id, bot) in [(CLAUDE, &claude), (CODEX, &codex)] {
        probe.bots.lock().unwrap().insert(id.to_string(), bot.clone());
        bot.start();
    }
    for _ in 0..150 {
        if claude.status().phase == Phase::Connected && codex.status().phase == Phase::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for bot in [&claude, &codex] {
        if bot.status().phase != Phase::Connected {
            anyhow::bail!("{} didn't connect: {:?} {:?}", bot.id(), bot.status().phase, bot.status().message);
        }
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let rooms = RoomsClient::new(
        data.clone(),
        Hooks {
            status: Box::new(|_| {}),
            event: Box::new(move |name, payload, _| {
                let _ = tx.send((name.to_string(), payload.clone()));
            }),
            connected: Box::new(|| {}),
        },
    );
    let _ = probe.rooms.set(rooms.clone());
    rooms.set_bots(Arc::new(ProbeBots(probe.clone())));
    rooms.start();

    let (claude_uid, codex_uid) = (format!("{CLAUDE}/claude"), format!("{CODEX}/codex"));
    let made = rooms.call("rooms.create", json!({ "name": "Probe", "members": [claude_uid, codex_uid] })).await.map_err(fail)?;
    let id = made["id"].as_str().unwrap_or_default().to_string();
    let key = made["key"].as_str().unwrap_or_default().to_string();
    let mut update = json!({ "id": id, "folder": { claude_uid.clone(): folder, codex_uid.clone(): folder } });
    if std::env::var("PROBE_LISTEN").is_ok_and(|v| v == "1") {
        update["attention"] = json!({ codex_uid.clone(): "listening" });
    }
    rooms.call("rooms.update", update).await.map_err(fail)?;

    let script = std::env::var("PROBE_SAY").unwrap_or_else(|_| "What is 17 times 23? Answer, then ask @Codex to double-check it.".into());
    for (i, text) in script.split('|').map(str::trim).filter(|t| !t.is_empty()).enumerate() {
        println!("\n==== You: {text}");
        let started = std::time::Instant::now();
        rooms.call("chat.send", json!({ "sessionKey": key, "message": text, "idempotencyKey": format!("probe-{i}") })).await.map_err(fail)?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let Ok(Some((name, payload))) = tokio::time::timeout_at(deadline, rx.recv()).await else {
                println!("==== gave up waiting for the room to go quiet");
                break;
            };
            match name.as_str() {
                "session.message" if payload["message"]["role"] != "user" => {
                    let m = &payload["message"];
                    let who = m["room"]["from"].as_str().unwrap_or("room");
                    let text = m["content"].as_array().and_then(|c| c[0]["text"].as_str()).or_else(|| m["content"].as_str()).unwrap_or_default();
                    println!("==== [{:>5.1}s] {who} → {}: {text}", started.elapsed().as_secs_f32(), m["room"]["to"]);
                }
                "room.state" => println!("==== [{:>5.1}s] working: {} paused: {}", started.elapsed().as_secs_f32(), payload["working"], payload["paused"]),
                "chat" if payload["state"] == "final" || payload["state"] == "aborted" => {
                    println!("==== [{:>5.1}s] the room is quiet ({})", started.elapsed().as_secs_f32(), payload["state"]);
                    break;
                }
                _ => {}
            }
        }
    }
    let agents = rooms.call("agents.list", json!({})).await.map_err(fail)?;
    println!("\n==== seats: {}", agents["agents"][0]["room"]["members"]);
    claude.stop();
    codex.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}
