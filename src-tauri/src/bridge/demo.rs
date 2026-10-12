//! Pretend bots for `examples/bridge_demo.rs` and the bridge tests: an
//! OpenClaw gateway with Jefe and Fox (whose `chat.send` streams a canned
//! reply, and Jefe waits on an approval), a Hermes and a Claude Code bot.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::host::{BridgeHost, HostEvent, MediaFetch};
use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;

pub const OPENCLAW: &str = "openclaw";
pub const HERMES: &str = "hermes-demo";
pub const CLAUDE: &str = "claudecode";

/// A 1×1 PNG (the guy's favorite color).
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct State {
    sessions: HashMap<String, Vec<Value>>,
    history: HashMap<String, Vec<Value>>,
    approvals: Vec<Value>,
    enabled: HashMap<String, bool>,
}

pub struct DemoHost {
    events: broadcast::Sender<HostEvent>,
    state: Mutex<State>,
    /// How long between streamed words.
    pub word_delay: Duration,
}

fn oc(id: &str, extra: Value) -> Value {
    let mut meta = json!({ "id": id });
    if let (Some(m), Some(e)) = (meta.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    meta
}

fn text_message(role: &str, text: &str, ts: u64, id: &str) -> Value {
    json!({ "role": role, "content": [{ "type": "text", "text": text }], "timestamp": ts, "__openclaw": oc(id, json!({})) })
}

impl Default for DemoHost {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoHost {
    pub fn new() -> Self {
        let now = now_ms();
        let row = |key: &str, agent: &str, preview: &str, ago_min: u64, unread: bool| {
            json!({ "key": key, "agentId": agent, "kind": "direct", "lastMessagePreview": preview,
                    "updatedAt": now - ago_min * 60_000, "unread": unread })
        };
        let mut sessions = HashMap::new();
        sessions.insert(
            OPENCLAW.to_string(),
            vec![
                row("agent:main:main", "main", "I need your OK to clean up old builds.", 3, true),
                row("agent:fox:main", "fox", "Found three flights under $900.", 25, false),
                json!({ "key": "agent:fox:dashboard:trip", "agentId": "fox", "kind": "direct", "label": "Tokyo trip",
                        "lastMessagePreview": "Booked the ryokan for the 14th.", "updatedAt": now - 2 * 3_600_000 }),
            ],
        );
        sessions.insert(HERMES.to_string(), vec![row("agent:default:main", "default", "The weekly report is ready.", 60, false)]);
        sessions.insert(CLAUDE.to_string(), vec![row("agent:claude:dashboard:s1", "claude", "Tests pass. Want me to commit?", 8, true)]);
        let mut history = HashMap::new();
        history.insert(
            format!("{OPENCLAW}|agent:fox:main"),
            vec![
                text_message("user", "Any cheap flights to Tokyo in March?", now - 30 * 60_000, "h1"),
                text_message("assistant", "Found three flights under $900. The best is GRU→NRT on March 3 for $842.", now - 25 * 60_000, "h2"),
            ],
        );
        history.insert(
            format!("{OPENCLAW}|agent:main:main"),
            vec![text_message("assistant", "I need your OK to clean up old builds.", now - 3 * 60_000, "h3")],
        );
        let approvals = vec![json!({
            "id": "appr-1", "agentId": "main", "sessionKey": "agent:main:main", "createdAtMs": now - 3 * 60_000,
            "presentation": { "commandText": "rm -rf ~/Downloads/old-builds", "warningText": "Deletes files.",
                              "allowedDecisions": ["allow-once", "allow-always", "deny"] },
        })];
        let enabled = [OPENCLAW, HERMES, CLAUDE].iter().map(|id| (id.to_string(), true)).collect();
        Self {
            events: broadcast::channel(4096).0,
            state: Mutex::new(State { sessions, history, approvals, enabled }),
            word_delay: Duration::from_millis(80),
        }
    }

    /// Turn a bot off or on, as Settings would.
    pub fn set_enabled(&self, gateway: &str, on: bool) {
        self.state.lock().unwrap().enabled.insert(gateway.to_string(), on);
        let _ = self.events.send(HostEvent::BotsChanged);
    }

    /// Send an event as if a bot did.
    pub fn emit(&self, gateway: &str, name: &str, payload: Value) {
        let _ = self.events.send(HostEvent::Event { gateway: gateway.into(), name: name.into(), payload });
    }

    fn status(id: &str) -> Value {
        let caps = |manage: bool, attachments: bool, files: bool| {
            json!({ "manageGuys": manage, "routines": true, "routinesLocal": false, "approvals": true,
                    "threadActions": true, "newThreads": true, "attachments": attachments, "models": manage,
                    "files": files, "skills": true, "commands": true, "tools": true, "plugins": manage,
                    "browser": false, "pastSessions": false })
        };
        match id {
            OPENCLAW => json!({
                "gatewayId": OPENCLAW, "gatewayName": "Studio", "kind": "openclaw", "capabilities": caps(true, true, true),
                "phase": "connected", "mode": "local", "url": "ws://127.0.0.1:18789", "deviceId": "mac-device-id",
                "configPath": "/Users/you/.openclaw/openclaw.json", "serverVersion": "2026.9.7",
                "scopes": ["operator.read", "operator.write", "operator.admin", "operator.approvals"],
                "hello": { "policy": { "maxPayload": 26_214_400, "attachments": { "maxBytes": 20_000_000, "maxImageBytes": 6_000_000 } },
                           "features": { "methods": ["agents.list", "sessions.list", "chat.history", "chat.send", "chat.abort",
                                                      "exec.approval.list", "exec.approval.resolve", "skills.status", "commands.list",
                                                      "agents.workspace.list", "cron.list"] },
                           "snapshot": { "presence": [] } },
            }),
            HERMES => json!({
                "gatewayId": HERMES, "gatewayName": "Hermes", "kind": "hermes", "capabilities": caps(false, false, true),
                "phase": "connected", "url": "http://127.0.0.1:9119", "serverVersion": "0.21.5", "scopes": [],
            }),
            _ => json!({
                "gatewayId": CLAUDE, "gatewayName": "Claude Code", "kind": "claudecode", "capabilities": caps(false, true, false),
                "phase": "connected", "url": null, "scopes": [],
            }),
        }
    }

    fn agents(gateway: &str) -> Value {
        match gateway {
            OPENCLAW => json!({ "defaultId": "main", "agents": [
                { "id": "main", "name": "Jefe", "identity": { "name": "Jefe", "emoji": "🤠" }, "model": { "primary": "claude-opus-5" } },
                { "id": "fox", "name": "Fox", "identity": { "name": "Fox", "emoji": "🦊" }, "model": { "primary": "claude-sonnet-5" } },
            ] }),
            HERMES => json!({ "defaultId": "default", "agents": [{ "id": "default", "name": "Hermes", "identity": { "name": "Hermes", "emoji": "🪽" } }] }),
            _ => json!({ "defaultId": "claude", "agents": [{ "id": "claude", "name": "Claude", "identity": { "name": "Claude", "emoji": "✳️" } }] }),
        }
    }

    fn answer(&self, gateway: &str, method: &str, p: &Value) -> Result<Value, RpcError> {
        let mut state = self.state.lock().unwrap();
        if !state.enabled.get(gateway).copied().unwrap_or(false) {
            return Err(RpcError::new("UNKNOWN_GATEWAY", format!("no bot {gateway}")));
        }
        let key = p["sessionKey"].as_str().or(p["key"].as_str()).unwrap_or_default().to_string();
        Ok(match method {
            "agents.list" => Self::agents(gateway),
            "sessions.list" => {
                let rows = state.sessions.get(gateway).cloned().unwrap_or_default();
                let agent = p["agentId"].as_str();
                let archived = p["archived"] == true;
                json!({ "sessions": rows.into_iter()
                    .filter(|r| agent.is_none_or(|a| r["agentId"] == a))
                    .filter(|r| (r["archived"] == true) == archived)
                    .collect::<Vec<_>>() })
            }
            "sessions.subscribe" | "sessions.messages.subscribe" | "sessions.messages.unsubscribe" => json!({ "subscribed": true }),
            "chat.history" => json!({
                "sessionKey": key,
                "messages": state.history.get(&format!("{gateway}|{key}")).cloned().unwrap_or_default(),
                "hasMore": false,
            }),
            "sessions.create" => {
                let agent = p["agentId"].as_str().unwrap_or("main");
                let key = format!("agent:{agent}:dashboard:{}", uuid::Uuid::new_v4().simple());
                let row = json!({ "key": key, "agentId": agent, "kind": "direct", "label": p["label"], "updatedAt": now_ms() });
                state.sessions.entry(gateway.to_string()).or_default().push(row);
                json!({ "ok": true, "key": key })
            }
            "sessions.patch" => {
                if let Some(row) = state.sessions.get_mut(gateway).into_iter().flatten().find(|r| r["key"] == key.as_str()) {
                    for flag in ["label", "pinned", "archived", "unread"] {
                        if let Some(v) = p.get(flag) {
                            row[flag] = v.clone();
                        }
                    }
                }
                json!({ "ok": true, "key": key })
            }
            "sessions.delete" => {
                if let Some(rows) = state.sessions.get_mut(gateway) {
                    rows.retain(|r| r["key"] != key.as_str());
                }
                json!({ "ok": true })
            }
            "exec.approval.list" => {
                json!({ "approvals": if gateway == OPENCLAW { state.approvals.clone() } else { Vec::new() } })
            }
            "exec.approval.resolve" => {
                let id = p["id"].as_str().unwrap_or_default().to_string();
                state.approvals.retain(|a| a["id"] != id.as_str());
                drop(state);
                self.emit(gateway, "exec.approval.resolved", json!({ "id": id, "decision": p["decision"] }));
                json!({ "ok": true })
            }
            "chat.abort" => json!({ "ok": true, "aborted": false }),
            "cron.list" => json!({ "jobs": [], "total": 0 }),
            "skills.status" => json!({ "skills": [] }),
            "commands.list" => json!({ "commands": [] }),
            "tools.catalog" | "tools.effective" => json!({ "groups": [] }),
            "plugins.list" => json!({ "plugins": [] }),
            "mcp.servers.list" => json!({ "servers": [] }),
            "models.list" => json!({ "models": [] }),
            "agents.workspace.list" => json!({ "agentId": p["agentId"], "path": "", "entries": [], "totalEntries": 0, "offset": 0 }),
            "echo" => p.clone(),
            _ => return Err(RpcError::new("UNSUPPORTED", format!("{method} isn't supported by this bot"))),
        })
    }

    /// `chat.send`: the user's message, then a reply word by word, then final.
    fn send(&'static self, gateway: &str, p: &Value) -> Result<Value, RpcError> {
        let key = p["sessionKey"].as_str().ok_or_else(|| RpcError::new("INVALID_PARAMS", "sessionKey is required"))?.to_string();
        let text = p["message"].as_str().unwrap_or_default().to_string();
        let run_id = p["idempotencyKey"].as_str().map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let gateway = gateway.to_string();
        let agent = key.split(':').nth(1).unwrap_or("main").to_string();
        let user = text_message("user", &text, now_ms(), &format!("{run_id}:user"));
        self.state.lock().unwrap().history.entry(format!("{gateway}|{key}")).or_default().push(user.clone());
        let reply = format!("You said: “{text}”. On it! 🫡 This reply comes from bridge_demo on the Mac.");
        let id = run_id.clone();
        tokio::spawn(async move {
            let ev = |name: &str, payload: Value| self.emit(&gateway, name, payload);
            ev("sessions.changed", json!({ "sessionKey": key, "agentId": agent, "phase": "start" }));
            ev("session.message", json!({ "sessionKey": key, "message": user }));
            ev("chat", json!({ "sessionKey": key, "runId": id, "state": "status", "phase": "starting_model" }));
            let words: Vec<&str> = reply.split(' ').collect();
            for i in (2..=words.len()).step_by(2).chain(std::iter::once(words.len())) {
                tokio::time::sleep(self.word_delay).await;
                let partial = words[..i].join(" ");
                ev("chat", json!({ "sessionKey": key, "runId": id, "state": "delta",
                                   "message": { "role": "assistant", "content": [{ "type": "text", "text": partial }] } }));
            }
            let mut message = text_message("assistant", &reply, now_ms(), &uuid::Uuid::new_v4().to_string());
            message["__openclaw"]["runId"] = json!(id);
            self.state.lock().unwrap().history.entry(format!("{gateway}|{key}")).or_default().push(message.clone());
            ev("session.message", json!({ "sessionKey": key, "runId": id, "message": message }));
            ev("chat", json!({ "sessionKey": key, "runId": id, "state": "final", "message": message }));
            ev("sessions.changed", json!({ "sessionKey": key, "agentId": agent, "phase": "end" }));
        });
        Ok(json!({ "runId": run_id, "status": "started" }))
    }

    /// Leak one for the process (the demo and tests keep it for good).
    pub fn leak(self) -> &'static DemoHost {
        Box::leak(Box::new(self))
    }
}

/// The demo host behind an `Arc<dyn BridgeHost>` (it must be `'static` to stream replies).
pub struct Demo(pub &'static DemoHost);

impl BridgeHost for Demo {
    fn statuses(&self) -> Vec<Value> {
        let enabled = self.0.state.lock().unwrap().enabled.clone();
        [OPENCLAW, HERMES, CLAUDE].iter().filter(|id| enabled.get(**id).copied().unwrap_or(false)).map(|id| DemoHost::status(id)).collect()
    }

    fn call(&self, gateway: &str, method: &str, params: Value, _timeout: Option<Duration>) -> BoxFuture<'static, Result<Value, RpcError>> {
        let host = self.0;
        let result = if method == "chat.send" && host.state.lock().unwrap().enabled.get(gateway).copied().unwrap_or(false) {
            host.send(gateway, &params)
        } else {
            host.answer(gateway, method, &params)
        };
        Box::pin(async move { result })
    }

    fn reconnect(&self, _gateway: &str) -> Result<(), RpcError> {
        Ok(())
    }

    fn media(&self, gateway: &str, source: &str, _key: Option<&str>, _agent: Option<&str>) -> BoxFuture<'static, Result<MediaFetch, RpcError>> {
        let fetch = if gateway != OPENCLAW {
            MediaFetch::Unavailable { code: Some("UNSUPPORTED".into()), reason: Some("only OpenClaw serves files by path".into()) }
        } else if source.ends_with(".png") {
            let body = futures_util::stream::iter(vec![Ok(PIXEL_PNG.to_vec())]).boxed();
            MediaFetch::Available { mime_type: Some("image/png".into()), size_bytes: Some(PIXEL_PNG.len() as u64), body }
        } else if source.ends_with(".bin") {
            // 200 KB in uneven chunks, to exercise re-chunking.
            let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
            let chunks: Vec<Result<Vec<u8>, String>> = data.chunks(7_777).map(|c| Ok(c.to_vec())).collect();
            MediaFetch::Available { mime_type: Some("application/octet-stream".into()), size_bytes: Some(200_000), body: futures_util::stream::iter(chunks).boxed() }
        } else {
            MediaFetch::Unavailable { code: Some("not-found".into()), reason: Some(format!("{source} isn't there")) }
        };
        Box::pin(async move { Ok(fetch) })
    }

    fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.0.events.subscribe()
    }
}
