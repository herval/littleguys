//! Hermes Agent (Nous Research) adapter: every Hermes **profile** is a little guy.
//!
//! Two transports, picked by [`HermesConfig::mode`]:
//!
//! - **local** — the machine's `hermes serve` backend (what Hermes Desktop drives; one
//!   process serves every profile). Turns, live events and approval prompts travel over one
//!   newline-delimited JSON-RPC 2.0 WebSocket (`/api/ws?token=…`; the per-process session
//!   token is scraped from `GET /` or read from the host rendezvous token file). The same
//!   backend's REST API (`X-Hermes-Session-Token`) serves cross-profile session lists,
//!   transcripts and cron jobs. Approvals are server→client JSON-RPC requests, which Hermes
//!   only sends after `client.capabilities {server_requests:true}`.
//! - **api** — Hermes' OpenAI-compatible API server (`API_SERVER_KEY` bearer) for remote
//!   hosts: `/api/sessions`, `/v1/runs` (+ SSE events and approvals) and `/api/jobs`. One
//!   profile (`config.profile`, else the server's default) is the guy.
//!
//! Everything is translated into the OpenClaw-shaped surface documented in
//! [`crate::runtime`]: session keys `agent:<profile>:…`, `chat`/`session.message`/`agent`
//! events, `exec.approval.*` and OpenClaw `CronJob`s.
//!
//! Session keys: chat-ish sources (tui, cli, desktop, api…) → `keys::thread`, cron runs
//! (`cron_<job>_<ts>`) → `keys::routine_run`, messaging platforms → `keys::channel`. The
//! guy's main chat (`agent:<profile>:main`) is Hermes' canonical per-profile "Bot Chat"
//! (the session titled exactly "Bot Chat"), created lazily on first send; the mapping is
//! also remembered in `<data_dir>/hermes-<gatewayId>.json`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::{now_ms, write_private};
use crate::gateways::{HermesConfig, HermesMode};
use crate::openclaw::config::{Credential, GatewayMode};
use crate::runtime::{keys, unsupported, Capabilities, RuntimeKind};

const DEFAULT_LOCAL_PORT: u16 = 9119;
const DEFAULT_API_PORT: u16 = 8642;
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(45);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT: Duration = Duration::from_secs(15);
/// Hermes' own clients give up after 45 s without a frame; heartbeats answer every 15 s.
const STALE_AFTER_MS: u64 = 50_000;
const API_POLL: Duration = Duration::from_secs(30);
/// Hermes' canonical per-profile "forever chat" (agent/system_prompt.py `BOT_CHAT_TITLE`).
const MAIN_TITLE: &str = "Bot Chat";
/// Session source for sessions littleguys creates. Not "desktop": that would grant the
/// desktop-only GUI toolsets (preview/window reads) littleguys can't serve.
const SESSION_SOURCE: &str = "tui";

/// Sources that are messaging platforms (threads live in another app).
const MESSAGING_SOURCES: &[&str] = &[
    "telegram", "discord", "slack", "whatsapp", "whatsapp_cloud", "signal", "matrix", "email", "sms",
    "mattermost", "homeassistant", "imessage", "bluebubbles", "weixin", "wechat", "wecom", "feishu", "lark",
    "dingtalk", "qqbot", "qq", "yuanbao", "line", "teams", "msgraph", "twilio", "webhook", "irc", "xmpp",
    "googlechat", "google_chat", "messenger", "instagram", "zalo", "kakao", "viber", "nostr", "rocketchat",
    "zulip", "revolt", "x", "twitter", "voice",
];

// ---- discovery helpers (used by the app's "add a bot" flow) -------------------------

/// What answers at a URL that looks like Hermes.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HermesProbe {
    pub url: String,
    pub version: Option<String>,
    /// "local" (`hermes serve` / `hermes dashboard` backend) or "api" (API server).
    pub mode: String,
    pub auth_required: bool,
}

/// Recognize a Hermes backend (`/api/health`) or API server (`/health` + `/v1/capabilities`).
pub async fn probe(base_url: &str) -> Option<HermesProbe> {
    let url = normalize_http(base_url, DEFAULT_LOCAL_PORT).ok()?;
    let mut builder = reqwest::Client::builder().timeout(Duration::from_millis(2500));
    if is_loopback_url(&url) {
        builder = builder.no_proxy();
    }
    let http = builder.build().ok()?;
    if let Ok(resp) = http.get(format!("{url}/api/health")).send().await {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<Value>().await {
                if v.get("ok").and_then(Value::as_bool) == Some(true) && v.get("auth_required").is_some() {
                    return Some(HermesProbe {
                        url,
                        version: v.get("version").and_then(Value::as_str).map(str::to_string),
                        mode: "local".into(),
                        auth_required: v.get("auth_required").and_then(Value::as_bool).unwrap_or(false),
                    });
                }
            }
        }
    }
    let health: Value = http.get(format!("{url}/health")).send().await.ok()?.json().await.ok()?;
    if health.get("status").and_then(Value::as_str) != Some("ok")
        || health.get("platform").and_then(Value::as_str) != Some("hermes-agent")
    {
        return None;
    }
    let caps = http.get(format!("{url}/v1/capabilities")).send().await.ok()?;
    let auth_required = match caps.status().as_u16() {
        401 | 403 => true,
        200 => {
            let v: Value = caps.json().await.ok()?;
            v.get("object").and_then(Value::as_str).is_some_and(|o| o.starts_with("hermes."))
                && v.pointer("/auth/required").and_then(Value::as_bool).unwrap_or(false)
        }
        _ => return None,
    };
    Some(HermesProbe {
        url,
        version: health.get("version").and_then(Value::as_str).map(str::to_string),
        mode: "api".into(),
        auth_required,
    })
}

/// The live `hermes serve` / `hermes dashboard` on this machine, from Hermes' spawn ledger
/// (`~/.hermes/spawn-ledger.json`, honoring `HERMES_HOME`), else its host rendezvous record.
pub fn local_backend_url() -> Option<String> {
    let mut candidates: Vec<(f64, String, u16)> = Vec::new();
    if let Some(root) = hermes_root() {
        if let Ok(raw) = std::fs::read_to_string(root.join("spawn-ledger.json")) {
            if let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&raw) {
                for e in entries {
                    let purpose = e.get("purpose").and_then(Value::as_str).unwrap_or_default();
                    if purpose != "serve" && purpose != "dashboard" {
                        continue;
                    }
                    let Some(port) = e.get("port").and_then(Value::as_u64).filter(|p| *p > 0 && *p < 65536) else {
                        continue;
                    };
                    let at = e.get("registered_at").and_then(Value::as_f64).unwrap_or(0.0);
                    candidates.push((at, dial_host(e.get("host").and_then(Value::as_str).unwrap_or("")), port as u16));
                }
            }
        }
    }
    if let Some((host, port)) = host_record().map(|(h, p, _)| (h, p)) {
        candidates.push((0.0, host, port));
    }
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    candidates.into_iter().find(|(_, host, port)| port_open(host, *port)).map(|(_, host, port)| format_host_url(&host, port))
}

/// Hermes' install root: `$HERMES_HOME` (or its `<root>` when it points at
/// `<root>/profiles/<name>`), else `~/.hermes`.
fn hermes_root() -> Option<PathBuf> {
    let native = dirs::home_dir()?.join(".hermes");
    match std::env::var("HERMES_HOME").ok().filter(|h| !h.trim().is_empty()) {
        Some(home) => {
            let path = PathBuf::from(home.trim());
            if path.starts_with(&native) {
                return Some(native);
            }
            match path.parent() {
                Some(parent) if parent.file_name().and_then(|n| n.to_str()) == Some("profiles") => {
                    parent.parent().map(Path::to_path_buf)
                }
                _ => Some(path),
            }
        }
        None => Some(native),
    }
}

/// Per-OS-user rendezvous dir where `hermes serve` publishes `host-serve.json` + its token.
fn host_state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HERMES_GATEWAY_LOCK_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(state.join("hermes").join("gateway-locks"))
}

/// (host, port, token) published by the host's `hermes serve` owner.
fn host_record() -> Option<(String, u16, Option<String>)> {
    let dir = host_state_dir()?;
    for role in ["serve", "desktop-serve"] {
        let Ok(raw) = std::fs::read_to_string(dir.join(format!("host-{role}.json"))) else { continue };
        let Ok(rec) = serde_json::from_str::<Value>(&raw) else { continue };
        let Some(port) = rec.get("port").and_then(Value::as_u64).filter(|p| *p > 0 && *p < 65536) else { continue };
        let token = std::fs::read_to_string(dir.join(format!("host-{role}.token")))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        return Some((dial_host(rec.get("host").and_then(Value::as_str).unwrap_or("")), port as u16, token));
    }
    None
}

fn dial_host(host: &str) -> String {
    match host.trim() {
        "" | "0.0.0.0" | "::" | "*" | "localhost" => "127.0.0.1".into(),
        h => h.to_string(),
    }
}

fn format_host_url(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn port_open(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addrs) = (host.trim_matches(['[', ']']), port).to_socket_addrs() else { return false };
    addrs.next().is_some_and(|addr| std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or("").trim_start_matches('[') } else { host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host) };
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Accept `127.0.0.1:9119`, `http://host:8642/`, `ws://…/api/ws` and return `http(s)://host:port[/prefix]`.
pub fn normalize_http(input: &str, default_port: u16) -> Result<String, String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Enter Hermes' address.".into());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some(("http", r)) | Some(("ws", r)) => ("http", r),
        Some(("https", r)) | Some(("wss", r)) => ("https", r),
        Some((other, _)) => return Err(format!("{other}:// isn't a Hermes address.")),
        None => ("", raw),
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("That doesn't look like a Hermes address.".into());
    }
    let (authority, mut path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    for suffix in ["/api/ws", "/api/health", "/health", "/v1"] {
        if let Some(p) = path.strip_suffix(suffix) {
            path = p.to_string();
        }
    }
    let has_port = authority.rsplit_once(':').is_some_and(|(h, p)| !h.ends_with(':') && p.parse::<u16>().is_ok())
        && !(authority.starts_with('[') && authority.ends_with(']'));
    let scheme = if scheme.is_empty() {
        let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority);
        let private = host == "localhost" || host.ends_with(".local") || host.parse::<std::net::IpAddr>().is_ok();
        if has_port || private { "http" } else { "https" }
    } else {
        scheme
    };
    let authority = if has_port || scheme == "https" { authority.to_string() } else { format!("{authority}:{default_port}") };
    Ok(format!("{scheme}://{authority}{path}"))
}

// ---- the client ----------------------------------------------------------------------

#[derive(Clone)]
pub struct HermesClient {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    config: HermesConfig,
    data_dir: PathBuf,
    hooks: Hooks,
    http: reqwest::Client,
    status: Mutex<ConnStatus>,
    credential: Mutex<Option<Credential>>,
    started: AtomicBool,
    stopped: AtomicBool,
    wake: Notify,
    drop_conn: Notify,
    conn: Mutex<Option<Conn>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    next_id: AtomicU64,
    last_rx: AtomicU64,
    auth_retries: AtomicU32,
    st: Mutex<State>,
    saved: Mutex<Saved>,
}

#[derive(Clone)]
struct Conn {
    /// `http(s)://host:port` (+ `/p/<profile>` in api mode).
    base: String,
    /// Local session token, or the API server key.
    token: Option<String>,
    /// Local mode: the WebSocket writer.
    writer: Option<mpsc::UnboundedSender<Message>>,
}

/// A session attached on this connection (runtime id → stored session).
#[derive(Debug, Clone)]
struct Live {
    profile: String,
    stored: String,
    key: String,
}

/// One turn in flight, keyed by runtime session id (local) or run id (api).
#[derive(Debug, Clone, Default)]
struct Turn {
    run_id: String,
    key: String,
    agent_id: String,
    /// Text of the segment being streamed (since the last sealed message).
    text: String,
    reasoning: String,
    reasoning_available: String,
    seq: u32,
    emitted_text: bool,
    phase: String,
    /// tool id → name.
    tools: HashMap<String, String>,
    /// api mode: tool events carry no ids; pair started/completed FIFO per name.
    api_tools: VecDeque<(String, String)>,
    /// local mode: Hermes session id for reattaching after a reconnect.
    profile: String,
    stored: String,
}

#[derive(Debug, Clone)]
struct Approval {
    payload: Value,
    choices: Vec<String>,
    /// Local: the open server→client request id (`srq-…`) and the runtime session.
    srq_id: Option<String>,
    sid: Option<String>,
    /// Hermes' approval queue id.
    request_id: Option<String>,
    /// Api: the run waiting on it.
    run_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CronSeen {
    last_run_at: Option<String>,
    fingerprint: String,
}

#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
    by_session: HashMap<(String, String), String>,
    turns: HashMap<String, Turn>,
    queued_runs: HashMap<String, VecDeque<String>>,
    approvals: HashMap<String, Approval>,
    /// profile → canonical "Bot Chat" session (from profiles.list).
    canonical: HashMap<String, String>,
    /// Threads created here that Hermes hasn't persisted yet (no first prompt).
    drafts: HashMap<String, Value>,
    profiles: Vec<String>,
    job_profiles: HashMap<String, String>,
    cron: HashMap<String, CronSeen>,
    cron_primed: bool,
    cron_check_scheduled: bool,
    sessions_emit_scheduled: bool,
    gateway_running: Option<bool>,
    /// (profile, stored) of sessions to re-attach after a reconnect.
    reattach: Vec<(String, String, String)>,
    /// profiles.list (and so the canonical main chats) loaded on this connection.
    profiles_loaded: bool,
    /// key → (latest message text seen here, ms): Hermes' row preview is the *first* message.
    last_text: HashMap<String, (String, u64)>,
    /// profile ("" = Hermes' default) → (workspace root, fetched at ms).
    workspace_roots: HashMap<String, (String, u64)>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    /// profile → stored session id of the guy's main chat.
    #[serde(default)]
    main: HashMap<String, String>,
}

enum Outcome {
    Closed { was_connected: bool, reason: String },
    NotRunning(String),
    Unreachable(String),
    Auth(String),
    Fatal(String),
    Retry(Duration),
}

type Emits = Vec<(String, Value)>;

fn spawn<F: Future<Output = ()> + Send + 'static>(fut: F) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(fut);
        }
        Err(_) => {
            tauri::async_runtime::spawn(fut);
        }
    }
}

fn capabilities(mode: HermesMode) -> Capabilities {
    let local = mode == HermesMode::Local;
    Capabilities {
        manage_guys: false,
        routines: true,
        routines_local: false,
        approvals: true,
        thread_actions: true,
        new_threads: true,
        attachments: false,
        models: false,
        files: local,
        skills: true,
        commands: local,
        tools: true,
        plugins: local,
        browser: false,
        past_sessions: false,
    }
}

impl HermesClient {
    pub fn new(
        id: String,
        name: String,
        config: HermesConfig,
        credential: Option<Credential>,
        data_dir: PathBuf,
        hooks: Hooks,
    ) -> Self {
        let status = ConnStatus {
            gateway_id: id.clone(),
            gateway_name: name,
            kind: RuntimeKind::Hermes,
            capabilities: capabilities(config.mode),
            mode: Some(match config.mode {
                HermesMode::Local => GatewayMode::Local,
                HermesMode::Api => GatewayMode::Remote,
            }),
            url: config.url.clone(),
            ..Default::default()
        };
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(5));
        let local_target = config.mode == HermesMode::Local
            && config.url.as_deref().is_none_or(|u| normalize_http(u, DEFAULT_LOCAL_PORT).is_ok_and(|u| is_loopback_url(&u)));
        if local_target {
            builder = builder.no_proxy();
        }
        let http = builder.build().unwrap_or_default();
        let saved: Saved = std::fs::read_to_string(data_dir.join(format!("hermes-{id}.json")))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                id,
                config,
                data_dir,
                hooks,
                http,
                status: Mutex::new(status),
                credential: Mutex::new(credential),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                wake: Notify::new(),
                drop_conn: Notify::new(),
                conn: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                last_rx: AtomicU64::new(0),
                auth_retries: AtomicU32::new(0),
                st: Mutex::new(State::default()),
                saved: Mutex::new(saved),
            }),
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn status(&self) -> ConnStatus {
        self.inner.status.lock().unwrap().clone()
    }

    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        spawn(async move { run(inner).await });
    }

    /// Disconnect for good (bot removed or disabled).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.drop_conn.notify_one();
        self.inner.wake.notify_one();
    }

    pub fn reconnect_now(&self) {
        self.inner.drop_conn.notify_one();
        self.inner.wake.notify_one();
    }

    pub fn set_name(&self, name: String) {
        set_status(&self.inner, |s| s.gateway_name = name);
    }

    /// Api mode: the API server key (a `Token`).
    pub fn set_credential_override(&self, credential: Option<Credential>) {
        *self.inner.credential.lock().unwrap() = credential;
        self.inner.auth_retries.store(0, Ordering::SeqCst);
        self.reconnect_now();
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        let inner = &self.inner;
        match method {
            "sessions.messages.subscribe" => {
                // Attach the thread so turns started by other Hermes clients stream here too.
                if let Some(key) = params.get("key").and_then(Value::as_str) {
                    watch_session(inner, key);
                }
                return Ok(json!({ "subscribed": true }));
            }
            "sessions.subscribe" | "sessions.unsubscribe" | "sessions.messages.unsubscribe" => {
                return Ok(json!({ "subscribed": true }))
            }
            "exec.approval.list" => {
                let st = inner.st.lock().unwrap();
                let approvals: Vec<Value> = st.approvals.values().map(|a| a.payload.clone()).collect();
                return Ok(json!({ "approvals": approvals }));
            }
            _ => {}
        }
        if inner.status.lock().unwrap().phase != Phase::Connected || inner.conn.lock().unwrap().is_none() {
            return Err(RpcError::new("NOT_CONNECTED", "not connected to Hermes"));
        }
        let api = inner.config.mode == HermesMode::Api;
        match method {
            "agents.list" => agents_list(inner, timeout).await,
            "sessions.list" => sessions_list(inner, &params, timeout).await,
            "sessions.create" => sessions_create(inner, &params, timeout).await,
            "sessions.patch" => sessions_patch(inner, &params, timeout).await,
            "sessions.delete" => sessions_delete(inner, &params, timeout).await,
            "chat.history" => chat_history(inner, &params, timeout).await,
            "chat.send" if api => api_chat_send(inner, &params, timeout).await,
            "chat.send" => local_chat_send(inner, &params, timeout).await,
            "chat.abort" => chat_abort(inner, &params, timeout).await,
            "exec.approval.resolve" => approval_resolve(inner, &params, timeout).await,
            "cron.list" | "cron.get" | "cron.add" | "cron.update" | "cron.remove" | "cron.run" | "cron.runs"
            | "cron.status" => cron_call(inner, method, &params, timeout).await,
            "agents.workspace.list" | "agents.workspace.get" | "skills.read" if api => Err(unsupported(method)),
            "agents.workspace.list" => workspace_list(inner, &params, timeout).await,
            "agents.workspace.get" => workspace_get(inner, &params, timeout).await,
            "skills.status" => skills_status(inner, &params, timeout).await,
            "skills.read" => skills_read(inner, &params, timeout).await,
            "commands.list" if api => Err(unsupported(method)),
            "commands.list" => commands_list(inner, &params, timeout).await,
            "tools.effective" | "tools.catalog" => tools_groups(inner, &params, timeout).await,
            "plugins.list" if api => Ok(json!({ "plugins": [] })),
            "plugins.list" => plugins_list(inner, &params, timeout).await,
            "mcp.servers.list" if api => Ok(json!({ "servers": [] })),
            "mcp.servers.list" => mcp_servers_list(inner, &params, timeout).await,
            other => Err(unsupported(other)),
        }
    }
}

// ---- status / events plumbing --------------------------------------------------------

fn set_status(inner: &Inner, update: impl FnOnce(&mut ConnStatus)) {
    let snapshot = {
        let mut status = inner.status.lock().unwrap();
        update(&mut status);
        status.clone()
    };
    (inner.hooks.status)(&snapshot);
}

fn emit(inner: &Inner, name: &str, payload: Value) {
    let frame = json!({ "type": "event", "event": name, "payload": payload });
    (inner.hooks.event)(name, &frame["payload"], &frame);
}

fn flush(inner: &Inner, emits: Emits) {
    for (name, payload) in emits {
        if name == "session.message" && payload.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
            let text = text_of(payload.pointer("/message/content").unwrap_or(&Value::Null));
            if let Some(key) = payload.get("sessionKey").and_then(Value::as_str) {
                note_last_text(inner, key, &text);
            }
        }
        emit(inner, &name, payload);
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(5)).min(30))
}

async fn wait_or_wake(inner: &Inner, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = inner.wake.notified() => true,
    }
}

fn connecting_phase(phase: Phase) -> Phase {
    match phase {
        Phase::AuthFailed | Phase::Offline | Phase::NoConfig | Phase::Error => phase,
        _ => Phase::Connecting,
    }
}

async fn run(inner: Arc<Inner>) {
    let mut attempt: u32 = 0;
    loop {
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let outcome = match inner.config.mode {
            HermesMode::Local => connect_local(&inner).await,
            HermesMode::Api => connect_api(&inner).await,
        };
        disconnected(&inner);
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let delay = match outcome {
            Outcome::Closed { was_connected, reason } => {
                if was_connected {
                    attempt = 0;
                }
                set_status(&inner, |s| {
                    s.phase = Phase::Offline;
                    s.message = Some(reason);
                    s.hello = None;
                });
                backoff(attempt)
            }
            Outcome::NotRunning(message) => {
                set_status(&inner, |s| {
                    s.phase = Phase::NoConfig;
                    s.message = Some(message);
                    s.hello = None;
                });
                Duration::from_secs(10)
            }
            Outcome::Unreachable(message) => {
                set_status(&inner, |s| {
                    s.phase = Phase::Offline;
                    s.message = Some(message);
                    s.hello = None;
                });
                backoff(attempt)
            }
            Outcome::Auth(message) => {
                set_status(&inner, |s| {
                    s.phase = Phase::AuthFailed;
                    s.message = Some(message);
                    s.hello = None;
                });
                Duration::from_secs(30)
            }
            Outcome::Fatal(message) => {
                set_status(&inner, |s| {
                    s.phase = Phase::Error;
                    s.message = Some(message);
                    s.hello = None;
                });
                Duration::from_secs(60)
            }
            Outcome::Retry(delay) => delay,
        };
        attempt = attempt.saturating_add(1);
        set_status(&inner, |s| s.retry_at_ms = Some(now_ms() + delay.as_millis() as u64));
        if wait_or_wake(&inner, delay).await {
            attempt = 0;
        }
    }
}

/// Connection gone: fail in-flight calls, forget runtime session ids (they belong to the
/// old socket) and remember which sessions to re-attach.
fn disconnected(inner: &Inner) {
    *inner.conn.lock().unwrap() = None;
    let pending: Vec<_> = inner.pending.lock().unwrap().drain().collect();
    for (_, tx) in pending {
        let _ = tx.send(Err(RpcError::new("DISCONNECTED", "connection to Hermes was lost")));
    }
    let resolved: Vec<String> = {
        let mut st = inner.st.lock().unwrap();
        let mut reattach: Vec<(String, String, String)> = Vec::new();
        if inner.config.mode == HermesMode::Local {
            let turns: Vec<(String, Turn)> = st.turns.drain().collect();
            for (sid, turn) in turns {
                if let Some(live) = st.live.get(&sid) {
                    reattach.push((live.profile.clone(), live.stored.clone(), live.key.clone()));
                }
                // Keep the turn under its stored id until the session is re-attached.
                st.turns.insert(format!("detached:{}:{}", turn.profile, turn.stored), turn);
            }
            for approval in st.approvals.values() {
                if let Some(live) = approval.sid.as_ref().and_then(|sid| st.live.get(sid)) {
                    reattach.push((live.profile.clone(), live.stored.clone(), live.key.clone()));
                }
            }
            st.live.clear();
            st.by_session.clear();
            st.queued_runs.clear();
        }
        reattach.sort();
        reattach.dedup();
        st.reattach = reattach;
        st.cron_check_scheduled = false;
        st.sessions_emit_scheduled = false;
        st.profiles_loaded = false;
        st.workspace_roots.clear();
        if inner.config.mode == HermesMode::Local {
            st.approvals.drain().map(|(id, _)| id).collect()
        } else {
            Vec::new()
        }
    };
    for id in resolved {
        emit(inner, "exec.approval.resolved", json!({ "id": id, "reason": "disconnected" }));
    }
}

// ---- local mode: connection ---------------------------------------------------------

fn configured_base(inner: &Inner, default_port: u16) -> Result<Option<String>, String> {
    inner.config.url.as_deref().filter(|u| !u.trim().is_empty()).map(|u| normalize_http(u, default_port)).transpose()
}

async fn connect_local(inner: &Arc<Inner>) -> Outcome {
    let base = match configured_base(inner, DEFAULT_LOCAL_PORT) {
        Ok(Some(url)) => url,
        Ok(None) => tokio::task::spawn_blocking(local_backend_url)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_LOCAL_PORT}")),
        Err(e) => return Outcome::Fatal(e),
    };
    set_status(inner, |s| {
        s.phase = connecting_phase(s.phase);
        s.url = Some(base.clone());
        s.hello = None;
    });
    let health = match inner.http.get(format!("{base}/api/health")).timeout(Duration::from_secs(5)).send().await {
        Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.unwrap_or(Value::Null),
        Ok(resp) if resp.status().as_u16() == 404 => {
            return Outcome::Fatal(format!(
                "{base} doesn't look like a Hermes backend. If it's Hermes' API server, add it in API mode."
            ))
        }
        Ok(resp) => return Outcome::Unreachable(format!("Hermes at {base} answered {}", resp.status())),
        Err(e) if inner.config.url.is_none() && (e.is_connect() || e.is_timeout()) => {
            return Outcome::NotRunning("Hermes isn't running here — start it with `hermes serve`.".into())
        }
        Err(e) => return Outcome::Unreachable(format!("can't reach Hermes at {base}: {}", short_err(&e))),
    };
    if health.get("ok").and_then(Value::as_bool) != Some(true) {
        return Outcome::Fatal(format!("{base} doesn't look like a Hermes backend."));
    }
    let version = health.get("version").and_then(Value::as_str).map(str::to_string);
    let auth_required = health.get("auth_required").and_then(Value::as_bool).unwrap_or(false);

    let Some(token) = local_token(inner, &base).await else {
        return if auth_required {
            Outcome::Auth(
                "This Hermes backend requires signing in (it isn't bound to this Mac only). Use Hermes' API server (API mode) for remote machines."
                    .into(),
            )
        } else {
            Outcome::Fatal("Couldn't get a session token from Hermes.".into())
        };
    };

    let ws_url = format!("{}/api/ws?token={token}", base.replacen("http", "ws", 1));
    let request = match ws_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return Outcome::Fatal(format!("invalid Hermes URL {base}: {e}")),
    };
    let ws_config = WebSocketConfig::default().max_message_size(Some(256 << 20)).max_frame_size(Some(64 << 20));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false);
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) if matches!(resp.status().as_u16(), 401 | 403) => {
            // Stale token (the backend restarted) or a gated backend.
            if inner.auth_retries.fetch_add(1, Ordering::SeqCst) < 2 {
                return Outcome::Retry(Duration::from_millis(500));
            }
            return Outcome::Auth("Hermes refused littleguys' session token.".into());
        }
        Ok(Err(e)) => return Outcome::Unreachable(format!("can't open Hermes' socket: {e}")),
        Err(_) => return Outcome::Unreachable("timed out opening Hermes' socket".into()),
    };
    inner.auth_retries.store(0, Ordering::SeqCst);
    let (mut sink, mut stream) = ws.split();

    // 1. gateway.ready
    let ready = tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    for line in text.lines() {
                        let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else { continue };
                        if frame["params"]["type"].as_str() == Some("gateway.ready") {
                            return Ok(frame["params"]["payload"].clone());
                        }
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                    return Err((code, frame.map(|f| f.reason.to_string()).unwrap_or_default()));
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err((0, e.to_string())),
                None => return Err((0, "connection closed".into())),
            }
        }
    })
    .await;
    match ready {
        Ok(Ok(_)) => {}
        Ok(Err((4401 | 4403, _))) => return Outcome::Auth("Hermes refused littleguys' session token.".into()),
        Ok(Err((_, reason))) => return Outcome::Unreachable(format!("Hermes closed the connection: {reason}")),
        Err(_) => return Outcome::Unreachable("Hermes didn't say hello (no gateway.ready)".into()),
    }

    // 2. serve: writer + reader tasks
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    *inner.conn.lock().unwrap() = Some(Conn { base: base.clone(), token: Some(token), writer: Some(tx.clone()) });
    inner.last_rx.store(now_ms(), Ordering::SeqCst);
    let mut reader = {
        let inner = inner.clone();
        tokio::spawn(async move { read_loop(inner, stream).await })
    };

    // 3. say we answer approvals (else Hermes withdraws them), then go live.
    if let Err(e) = rpc(inner, "client.capabilities", json!({ "server_requests": true }), Duration::from_secs(10)).await {
        reader.abort();
        writer_task.abort();
        return Outcome::Unreachable(format!("Hermes handshake failed: {e}"));
    }
    let status = rest(inner, reqwest::Method::GET, "/api/status", None, Duration::from_secs(5)).await.ok();
    let gateway_running = status.as_ref().and_then(|s| s.get("gateway_running")).and_then(Value::as_bool);
    inner.st.lock().unwrap().gateway_running = gateway_running;
    let version = version.or_else(|| status.as_ref().and_then(|s| s["version"].as_str()).map(str::to_string));
    set_status(inner, |s| {
        s.phase = Phase::Connected;
        s.message = gateway_hint(gateway_running);
        s.detail_code = None;
        s.retry_at_ms = None;
        s.url = Some(base.clone());
        s.server_version = version.clone();
        s.config_path = status.as_ref().and_then(|s| s["hermes_home"].as_str()).unwrap_or_default().to_string();
        s.hello = Some(json!({
            "hermes": {
                "mode": "local",
                "version": version,
                "gatewayRunning": gateway_running,
                "profiles": status.as_ref().map(|s| s["profiles"].clone()).unwrap_or(Value::Null),
            }
        }));
    });
    (inner.hooks.connected)();
    {
        let inner = inner.clone();
        tokio::spawn(async move {
            reattach_sessions(&inner).await;
            cron_check(&inner).await;
        });
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let reason = loop {
        tokio::select! {
            res = &mut reader => break res.unwrap_or_else(|_| "connection closed".into()),
            _ = heartbeat.tick() => {
                if now_ms().saturating_sub(inner.last_rx.load(Ordering::SeqCst)) > STALE_AFTER_MS {
                    break "Hermes stopped responding".into();
                }
                let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
                let ping = json!({ "jsonrpc": "2.0", "id": id, "method": "gateway.ping", "params": {} });
                if tx.send(Message::Text(ping.to_string().into())).is_err() {
                    break "connection closed".into();
                }
            }
            _ = inner.drop_conn.notified() => break "reconnecting".into(),
        }
    };
    reader.abort();
    writer_task.abort();
    Outcome::Closed { was_connected: true, reason }
}

fn gateway_hint(gateway_running: Option<bool>) -> Option<String> {
    (gateway_running == Some(false))
        .then(|| "Hermes' gateway isn't running, so routines won't fire on schedule (start it with `hermes gateway start`).".into())
}

/// The backend's per-process session token: injected into `GET /` on loopback binds,
/// and published 0600 next to the host rendezvous record.
async fn local_token(inner: &Inner, base: &str) -> Option<String> {
    if let Ok(resp) = inner.http.get(format!("{base}/")).timeout(Duration::from_secs(5)).send().await {
        if let Ok(html) = resp.text().await {
            if let Some(token) = scrape_token(&html) {
                return Some(token);
            }
        }
    }
    let port = base.rsplit(':').next().and_then(|p| p.split('/').next()).and_then(|p| p.parse::<u16>().ok());
    host_record().filter(|(_, p, _)| Some(*p) == port).and_then(|(_, _, token)| token)
}

fn scrape_token(html: &str) -> Option<String> {
    let start = html.find("__HERMES_SESSION_TOKEN__")?;
    let rest = &html[start + "__HERMES_SESSION_TOKEN__".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
    let token: String = rest.chars().take_while(|c| *c != '"').collect();
    (!token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))).then_some(token)
}

async fn read_loop<S>(inner: Arc<Inner>, mut stream: S) -> String
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                inner.last_rx.store(now_ms(), Ordering::SeqCst);
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        handle_frame(&inner, line.trim());
                    }
                }
            }
            Some(Ok(Message::Close(frame))) => {
                return frame
                    .map(|f| f.reason.to_string())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "Hermes closed the connection".into())
            }
            Some(Ok(_)) => inner.last_rx.store(now_ms(), Ordering::SeqCst),
            Some(Err(e)) => return e.to_string(),
            None => return "connection closed".into(),
        }
    }
}

fn handle_frame(inner: &Arc<Inner>, line: &str) {
    let Ok(frame) = serde_json::from_str::<Value>(line) else {
        log::warn!("hermes: dropping unparseable frame");
        return;
    };
    if let Some(method) = frame.get("method").and_then(Value::as_str) {
        if method == "event" {
            handle_event(inner, &frame["params"]);
        } else if let Some(id) = frame.get("id").cloned().filter(|id| !id.is_null()) {
            handle_server_request(inner, method, id, &frame["params"]);
        }
        return;
    }
    let Some(id) = frame.get("id").and_then(Value::as_u64) else { return };
    let Some(tx) = inner.pending.lock().unwrap().remove(&id) else { return };
    let result = match frame.get("error") {
        Some(err) if !err.is_null() => Err(rpc_error(err)),
        _ => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = tx.send(result);
}

fn rpc_error(err: &Value) -> RpcError {
    let code = match err.get("code") {
        Some(Value::Number(n)) => format!("HERMES_{n}"),
        Some(Value::String(s)) => s.clone(),
        _ => "HERMES_ERROR".into(),
    };
    let mut e = RpcError::new(&code, err.get("message").and_then(Value::as_str).unwrap_or("Hermes request failed"));
    e.details = err.get("data").cloned().filter(|d| !d.is_null());
    e
}

async fn rpc(inner: &Inner, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
    let writer = inner
        .conn
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|c| c.writer.clone())
        .ok_or_else(|| RpcError::new("NOT_CONNECTED", "not connected to Hermes"))?;
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    inner.pending.lock().unwrap().insert(id, tx);
    let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    if writer.send(Message::Text(frame.to_string().into())).is_err() {
        inner.pending.lock().unwrap().remove(&id);
        return Err(RpcError::new("NOT_CONNECTED", "connection closed"));
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(RpcError::new("DISCONNECTED", "connection closed before Hermes replied")),
        Err(_) => {
            inner.pending.lock().unwrap().remove(&id);
            Err(RpcError::new("TIMEOUT", format!("{method} timed out")))
        }
    }
}

fn send_frame(inner: &Inner, frame: Value) -> bool {
    let writer = inner.conn.lock().unwrap().as_ref().and_then(|c| c.writer.clone());
    writer.is_some_and(|w| w.send(Message::Text(frame.to_string().into())).is_ok())
}

/// REST on the connected backend (local: session-token header; api: bearer key).
async fn rest(inner: &Inner, method: reqwest::Method, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value, RpcError> {
    let conn = inner.conn.lock().unwrap().clone().ok_or_else(|| RpcError::new("NOT_CONNECTED", "not connected to Hermes"))?;
    let mut req = inner.http.request(method, format!("{}{path}", conn.base)).timeout(timeout);
    if let Some(token) = &conn.token {
        req = match inner.config.mode {
            HermesMode::Local => req.header("X-Hermes-Session-Token", token),
            HermesMode::Api => req.bearer_auth(token),
        };
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            RpcError::new("TIMEOUT", format!("Hermes didn't answer {path} in time"))
        } else {
            RpcError::new("UNAVAILABLE", format!("Hermes request failed: {}", short_err(&e)))
        }
    })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(value);
    }
    let message = value
        .get("detail")
        .and_then(|d| d.as_str().map(str::to_string).or_else(|| d.get("message").and_then(Value::as_str).map(str::to_string)))
        .or_else(|| value.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .or_else(|| value.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("Hermes answered {status}"));
    let code = match status.as_u16() {
        401 | 403 => "UNAUTHORIZED",
        404 => "NOT_FOUND",
        409 => "CONFLICT",
        400 | 422 => "INVALID_REQUEST",
        _ => "HERMES_HTTP",
    };
    if matches!(status.as_u16(), 401 | 403) && inner.config.mode == HermesMode::Local {
        // The backend restarted with a new token: reconnect picks up the fresh one.
        inner.drop_conn.notify_one();
    }
    Err(RpcError::new(code, message))
}

fn short_err(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg = s.to_string();
        source = s.source();
    }
    msg
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- api mode: connection -----------------------------------------------------------

fn api_key(inner: &Inner) -> Option<String> {
    match inner.credential.lock().unwrap().clone() {
        Some(Credential::Token(t)) | Some(Credential::Password(t)) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => None,
    }
}

async fn connect_api(inner: &Arc<Inner>) -> Outcome {
    let root = match configured_base(inner, DEFAULT_API_PORT) {
        Ok(Some(url)) => url,
        Ok(None) => return Outcome::NotRunning("Add the address of Hermes' API server (e.g. http://my-box:8642).".into()),
        Err(e) => return Outcome::Fatal(e),
    };
    let base = match inner.config.profile.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(profile) if !root.contains("/p/") => format!("{root}/p/{}", enc(profile.trim())),
        _ => root.clone(),
    };
    set_status(inner, |s| {
        s.phase = connecting_phase(s.phase);
        s.url = Some(root.clone());
        s.hello = None;
    });
    let health = match inner.http.get(format!("{root}/health")).timeout(Duration::from_secs(8)).send().await {
        Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.unwrap_or(Value::Null),
        Ok(resp) => return Outcome::Unreachable(format!("Hermes' API server at {root} answered {}", resp.status())),
        Err(e) => return Outcome::Unreachable(format!("can't reach Hermes at {root}: {}", short_err(&e))),
    };
    if health.get("status").and_then(Value::as_str) != Some("ok") {
        return Outcome::Fatal(format!("{root} doesn't look like Hermes' API server."));
    }
    let Some(key) = api_key(inner) else {
        return Outcome::Auth("Paste the API server key (API_SERVER_KEY from Hermes' .env).".into());
    };
    let caps = inner.http.get(format!("{base}/v1/capabilities")).bearer_auth(&key).timeout(Duration::from_secs(8)).send().await;
    let caps: Value = match caps {
        Ok(resp) if resp.status().is_success() => resp.json().await.unwrap_or(Value::Null),
        Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => {
            return Outcome::Auth("Hermes rejected the API server key.".into())
        }
        Ok(resp) if resp.status().as_u16() == 404 && base != root => {
            return Outcome::Fatal(format!("Hermes doesn't serve the profile {:?} here.", inner.config.profile.clone().unwrap_or_default()))
        }
        Ok(resp) => return Outcome::Unreachable(format!("Hermes' API server answered {}", resp.status())),
        Err(e) => return Outcome::Unreachable(format!("can't reach Hermes at {root}: {}", short_err(&e))),
    };
    *inner.conn.lock().unwrap() = Some(Conn { base: base.clone(), token: Some(key), writer: None });
    let version = health.get("version").and_then(Value::as_str).map(str::to_string);
    set_status(inner, |s| {
        s.phase = Phase::Connected;
        s.message = None;
        s.detail_code = None;
        s.retry_at_ms = None;
        s.server_version = version.clone();
        s.hello = Some(json!({ "hermes": { "mode": "api", "version": version, "capabilities": caps.get("features").cloned() } }));
    });
    (inner.hooks.connected)();
    {
        let inner = inner.clone();
        tokio::spawn(async move { cron_check(&inner).await });
    }
    let mut poll = tokio::time::interval(API_POLL);
    poll.tick().await;
    let reason = loop {
        tokio::select! {
            _ = poll.tick() => {
                let ok = inner.http.get(format!("{root}/health")).timeout(Duration::from_secs(10)).send().await
                    .is_ok_and(|r| r.status().is_success());
                if !ok {
                    break "Hermes' API server stopped responding".to_string();
                }
                // No push channel for other clients' activity: refresh the roster now and then.
                emit(inner, "sessions.changed", json!({ "reason": "poll" }));
                let inner = inner.clone();
                tokio::spawn(async move { cron_check(&inner).await });
            }
            _ = inner.drop_conn.notified() => break "reconnecting".to_string(),
        }
    };
    Outcome::Closed { was_connected: true, reason }
}

// ---- agents ---------------------------------------------------------------------------

fn profile_allowed(inner: &Inner, profile: &str) -> bool {
    inner.config.profile.as_deref().map(str::trim).filter(|p| !p.is_empty()).is_none_or(|p| p == profile)
}

fn api_agent_id(inner: &Inner) -> String {
    inner.config.profile.as_deref().map(str::trim).filter(|p| !p.is_empty()).unwrap_or("default").to_string()
}

async fn agents_list(inner: &Inner, timeout: Duration) -> Result<Value, RpcError> {
    if inner.config.mode == HermesMode::Api {
        let id = api_agent_id(inner);
        let name = if id == "default" { "Hermes".to_string() } else { id.clone() };
        inner.st.lock().unwrap().profiles = vec![id.clone()];
        return Ok(json!({
            "defaultId": id,
            "mainKey": "main",
            "scope": "per-sender",
            "agents": [{ "id": id, "name": name, "identity": { "name": name } }],
        }));
    }
    let result = rpc(inner, "profiles.list", json!({ "include_sessions": true }), timeout).await?;
    let rows: Vec<Value> = result
        .get("profiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.get("name").and_then(Value::as_str).is_some_and(|n| profile_allowed(inner, n)))
        .filter(|p| p.get("role").and_then(Value::as_str) != Some("setup"))
        .collect();
    let agents: Vec<Value> = rows.iter().map(agent_from_profile).collect();
    let default_id = rows
        .iter()
        .find(|p| p.get("is_default").and_then(Value::as_bool) == Some(true))
        .or_else(|| rows.first())
        .and_then(|p| p.get("name").and_then(Value::as_str))
        .unwrap_or("default")
        .to_string();
    {
        let mut st = inner.st.lock().unwrap();
        st.profiles = rows.iter().filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string)).collect();
        st.profiles_loaded = true;
        st.canonical.clear();
        for p in &rows {
            let (Some(name), Some(canonical)) = (p.get("name").and_then(Value::as_str), p.get("canonical_session")) else { continue };
            let id = canonical.get("resolved_id").or_else(|| canonical.get("id")).and_then(Value::as_str);
            if let Some(id) = id.filter(|i| !i.is_empty()) {
                st.canonical.insert(name.to_string(), id.to_string());
            }
        }
    }
    Ok(json!({ "defaultId": default_id, "mainKey": "main", "scope": "per-sender", "agents": agents }))
}

/// `profiles.list` row → OpenClaw `AgentSummary`.
fn agent_from_profile(p: &Value) -> Value {
    let id = p.get("name").and_then(Value::as_str).unwrap_or("default");
    let display = p.get("display_name").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty());
    let name = display.map(str::to_string).unwrap_or_else(|| if id == "default" { "Hermes".into() } else { id.to_string() });
    let mut identity = json!({ "name": name });
    if let Some(emoji) = find_emoji(p.get("ui_meta").unwrap_or(&Value::Null), 0) {
        identity["emoji"] = json!(emoji);
    }
    let mut agent = json!({ "id": id, "name": name, "identity": identity });
    if let Some(path) = p.get("path").and_then(Value::as_str) {
        agent["workspace"] = json!(path);
    }
    if let Some(model) = p.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        let primary = match p.get("provider").and_then(Value::as_str).filter(|v| !v.is_empty() && *v != "auto") {
            Some(provider) if !model.starts_with(&format!("{provider}/")) => format!("{provider}/{model}"),
            _ => model.to_string(),
        };
        agent["model"] = json!({ "primary": primary });
    }
    if let Some(desc) = p.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        agent["description"] = json!(desc);
    }
    agent
}

fn find_emoji(v: &Value, depth: u8) -> Option<String> {
    let obj = v.as_object()?;
    if let Some(e) = obj.get("emoji").and_then(Value::as_str).filter(|e| !e.trim().is_empty()) {
        return Some(e.trim().to_string());
    }
    if depth >= 2 {
        return None;
    }
    obj.values().find_map(|child| find_emoji(child, depth + 1))
}

// ---- sessions --------------------------------------------------------------------------

fn main_session(inner: &Inner, profile: &str) -> Option<String> {
    if let Some(id) = inner.saved.lock().unwrap().main.get(profile) {
        return Some(id.clone());
    }
    inner.st.lock().unwrap().canonical.get(profile).cloned()
}

fn set_main_session(inner: &Inner, profile: &str, stored: &str) {
    let mut saved = inner.saved.lock().unwrap();
    if saved.main.get(profile).map(String::as_str) == Some(stored) {
        return;
    }
    saved.main.insert(profile.to_string(), stored.to_string());
    if let Ok(bytes) = serde_json::to_vec_pretty(&*saved) {
        if let Err(e) = write_private(&inner.data_dir.join(format!("hermes-{}.json", inner.id)), &bytes) {
            log::warn!("hermes: could not save main-chat mapping: {e}");
        }
    }
}

fn forget_main_session(inner: &Inner, profile: &str) {
    let mut saved = inner.saved.lock().unwrap();
    if saved.main.remove(profile).is_some() {
        if let Ok(bytes) = serde_json::to_vec_pretty(&*saved) {
            let _ = write_private(&inner.data_dir.join(format!("hermes-{}.json", inner.id)), &bytes);
        }
    }
    inner.st.lock().unwrap().canonical.remove(profile);
}

/// The contract key for a Hermes session row.
fn session_key(profile: &str, source: &str, id: &str, main_id: Option<&str>) -> String {
    if main_id == Some(id) {
        return keys::main(profile);
    }
    let source = source.trim().to_lowercase();
    if source == "cron" || id.starts_with("cron_") {
        if let Some(job) = cron_job_of(id) {
            return keys::routine_run(profile, &job, id);
        }
    }
    if MESSAGING_SOURCES.contains(&source.as_str()) {
        return keys::channel(profile, &source, id);
    }
    keys::thread(profile, id)
}

/// `cron_<job>_<YYYYMMDD>_<HHMMSS>` → job id.
fn cron_job_of(session_id: &str) -> Option<String> {
    let rest = session_id.strip_prefix("cron_")?;
    let (job, _) = rest.split_once('_')?;
    (!job.is_empty()).then(|| job.to_string())
}

#[derive(Debug, Clone, PartialEq)]
enum KeyRef {
    Main(String),
    Session { profile: String, id: String },
}

fn parse_key(key: &str) -> Option<KeyRef> {
    let (agent, rest) = keys::split(key)?;
    if rest == "main" {
        return Some(KeyRef::Main(agent.to_string()));
    }
    let id = keys::last_id(rest)?;
    (!id.is_empty()).then(|| KeyRef::Session { profile: agent.to_string(), id })
}

/// (profile, stored session id) for a key; `None` id = a main chat that doesn't exist yet.
fn resolve_key(inner: &Inner, key: &str) -> Result<(String, Option<String>), RpcError> {
    match parse_key(key) {
        Some(KeyRef::Main(profile)) => {
            let id = main_session(inner, &profile);
            Ok((profile, id))
        }
        Some(KeyRef::Session { profile, id }) => Ok((profile, Some(id))),
        None => Err(RpcError::new("INVALID_REQUEST", format!("not a Hermes session key: {key}"))),
    }
}

fn secs_to_ms(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_f64).filter(|f| *f > 0.0).map(|f| (f * 1000.0) as u64)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

/// Hermes session row (dashboard `/api/profiles/sessions` or API server `/api/sessions`) →
/// OpenClaw-ish `SessionRow`.
fn session_row(profile: &str, row: &Value, main_id: Option<&str>, running: bool) -> Option<Value> {
    let id = row.get("id").and_then(Value::as_str)?;
    let source = row.get("source").and_then(Value::as_str).unwrap_or("");
    let key = session_key(profile, source, id, main_id);
    let title = row.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let preview = row.get("preview").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let updated = secs_to_ms(row.get("last_active"))
        .or_else(|| secs_to_ms(row.get("last_activity_at")))
        .or_else(|| secs_to_ms(row.get("started_at")))
        .unwrap_or(0);
    let is_main = key == keys::main(profile);
    let mut out = json!({
        "key": key,
        "agentId": profile,
        "sessionId": id,
        "updatedAt": updated,
        "kind": "direct",
        "unread": truthy(row.get("unread")),
        "pinned": truthy(row.get("pinned")),
        "archived": truthy(row.get("archived")),
        "hasActiveRun": running,
        "status": if running { "running" } else { "idle" },
        "channel": source,
    });
    if let Some(started) = secs_to_ms(row.get("started_at")) {
        out["createdAt"] = json!(started);
    }
    if let Some(n) = row.get("message_count").and_then(Value::as_u64) {
        out["messageCount"] = json!(n);
    }
    if !is_main {
        if let Some(t) = title {
            out["label"] = json!(t);
            out["derivedTitle"] = json!(t);
        } else if let Some(p) = preview.filter(|p| !p.starts_with('[')) {
            out["derivedTitle"] = json!(truncate(p, 60));
        }
    }
    // Cron runs start with Hermes' "[IMPORTANT: You are running as a scheduled cron job…" wrapper.
    if let Some(p) = preview.filter(|p| !(source == "cron" && p.starts_with('['))) {
        out["lastMessagePreview"] = json!(p);
    }
    Some(out)
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

async fn sessions_list(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 500);
    if inner.config.mode == HermesMode::Local && !inner.st.lock().unwrap().profiles_loaded {
        // Main chats resolve through profiles.list; the app may ask for sessions first.
        let _ = agents_list(inner, timeout).await;
    }
    let (rows, scoped_profile): (Vec<(String, Value)>, Option<String>) = match inner.config.mode {
        HermesMode::Local => {
            let scope = inner.config.profile.clone().filter(|p| !p.trim().is_empty());
            let path = format!(
                "/api/profiles/sessions?limit={limit}&order=recent&archived=exclude&profile={}",
                enc(scope.as_deref().unwrap_or("all"))
            );
            let v = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
            let rows = v
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    let profile = r.get("profile").and_then(Value::as_str)?.to_string();
                    profile_allowed(inner, &profile).then_some((profile, r))
                })
                .collect();
            (rows, scope)
        }
        HermesMode::Api => {
            let agent = api_agent_id(inner);
            let v = rest(inner, reqwest::Method::GET, &format!("/api/sessions?limit={}", limit.min(200)), None, timeout).await?;
            let rows = v
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !truthy(r.get("hidden")) || r.get("title").and_then(Value::as_str) == Some(MAIN_TITLE))
                .map(|r| (agent.clone(), r))
                .collect();
            (rows, Some(agent))
        }
    };
    let (profiles, running_keys, drafts, last_text) = {
        let st = inner.st.lock().unwrap();
        let running: Vec<String> = st.turns.values().map(|t| t.key.clone()).collect();
        let mut profiles = st.profiles.clone();
        if let Some(p) = &scoped_profile {
            if !profiles.contains(p) {
                profiles.push(p.clone());
            }
        }
        (profiles, running, st.drafts.clone(), st.last_text.clone())
    };
    let mains: HashMap<String, Option<String>> = profiles.iter().map(|p| (p.clone(), main_session(inner, p))).collect();
    let mut out: Vec<Value> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    for (profile, row) in &rows {
        let main_id = mains.get(profile).cloned().flatten();
        let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
        let key = session_key(profile, row.get("source").and_then(Value::as_str).unwrap_or(""), id, main_id.as_deref());
        if let Some(r) = session_row(profile, row, main_id.as_deref(), running_keys.contains(&key)) {
            seen_ids.push(id.to_string());
            out.push(r);
        }
    }
    // Threads created here that Hermes hasn't written yet.
    let mut stale_drafts = Vec::new();
    for (key, row) in drafts {
        let id = row.get("sessionId").and_then(Value::as_str).unwrap_or_default().to_string();
        if seen_ids.contains(&id) {
            stale_drafts.push(key);
        } else if !out.iter().any(|r| r["key"] == key.as_str()) {
            let mut row = row.clone();
            row["hasActiveRun"] = json!(running_keys.contains(&key));
            out.push(row);
        }
    }
    if !stale_drafts.is_empty() {
        let mut st = inner.st.lock().unwrap();
        for key in stale_drafts {
            st.drafts.remove(&key);
        }
    }
    // Every guy has a main chat, even before its first message.
    for profile in &profiles {
        let key = keys::main(profile);
        if !out.iter().any(|r| r["key"] == key.as_str()) {
            out.push(json!({
                "key": key,
                "agentId": profile,
                "sessionId": mains.get(profile).cloned().flatten(),
                "updatedAt": 0,
                "kind": "direct",
                "hasActiveRun": running_keys.contains(&key),
                "status": if running_keys.contains(&key) { "running" } else { "idle" },
            }));
        }
    }
    for row in out.iter_mut() {
        let Some(key) = row.get("key").and_then(Value::as_str) else { continue };
        if let Some((text, at)) = last_text.get(key) {
            if *at + 5_000 >= row.get("updatedAt").and_then(Value::as_u64).unwrap_or(0) {
                row["lastMessagePreview"] = json!(text);
            }
        }
    }
    Ok(json!({ "sessions": out, "count": out.len(), "ts": now_ms() }))
}

fn note_last_text(inner: &Inner, key: &str, text: &str) {
    if !text.trim().is_empty() {
        inner.st.lock().unwrap().last_text.insert(key.to_string(), (truncate(text, 200), now_ms()));
    }
}

async fn sessions_create(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = params
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| inner.st.lock().unwrap().profiles.first().cloned())
        .unwrap_or_else(|| api_agent_id(inner));
    if !profile_allowed(inner, &profile) {
        return Err(RpcError::new("INVALID_REQUEST", format!("unknown guy {profile}")));
    }
    let label = params.get("label").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
    let stored = match inner.config.mode {
        HermesMode::Local => {
            let mut p = json!({ "profile": profile, "source": SESSION_SOURCE });
            if let Some(l) = &label {
                p["title"] = json!(l);
            }
            let res = rpc(inner, "session.create", p, timeout).await?;
            let sid = res.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
            let stored = res.get("stored_session_id").and_then(Value::as_str).unwrap_or(&sid).to_string();
            if !sid.is_empty() {
                remember_live(inner, &sid, &profile, &stored, &keys::thread(&profile, &stored));
            }
            stored
        }
        HermesMode::Api => {
            let mut body = json!({ "source": "api_server" });
            if let Some(l) = &label {
                body["title"] = json!(l);
            }
            let res = rest(inner, reqwest::Method::POST, "/api/sessions", Some(body), timeout).await?;
            res.pointer("/session/id")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::new("HERMES_HTTP", "Hermes didn't return a session id"))?
                .to_string()
        }
    };
    let key = keys::thread(&profile, &stored);
    let mut row = json!({
        "key": key,
        "agentId": profile,
        "sessionId": stored,
        "updatedAt": now_ms(),
        "createdAt": now_ms(),
        "kind": "direct",
        "unread": false,
    });
    if let Some(l) = &label {
        row["label"] = json!(l);
        row["derivedTitle"] = json!(l);
    }
    inner.st.lock().unwrap().drafts.insert(key.clone(), row);
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": profile, "reason": "create" }));
    Ok(json!({ "ok": true, "key": key, "sessionId": stored }))
}

fn remember_live(inner: &Inner, sid: &str, profile: &str, stored: &str, key: &str) {
    let mut st = inner.st.lock().unwrap();
    st.live.insert(sid.to_string(), Live { profile: profile.into(), stored: stored.into(), key: key.into() });
    st.by_session.insert((profile.to_string(), stored.to_string()), sid.to_string());
}

async fn sessions_patch(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let key = params.get("key").and_then(Value::as_str).ok_or_else(|| RpcError::new("INVALID_REQUEST", "key is required"))?;
    let (profile, stored) = resolve_key(inner, key)?;
    let Some(stored) = stored else { return Ok(json!({ "ok": true, "key": key })) };
    let is_main = matches!(parse_key(key), Some(KeyRef::Main(_)));
    let mut body = Map::new();
    // The main chat's title is its identity in Hermes ("Bot Chat"): never rename it.
    let label = if is_main { None } else { params.get("label").cloned() };
    if let Some(label) = &label {
        body.insert("title".into(), json!(label.as_str().unwrap_or("")));
    }
    for flag in ["pinned", "archived", "unread"] {
        if let Some(v) = params.get(flag).and_then(Value::as_bool) {
            body.insert(flag.into(), json!(v));
        }
    }
    if body.is_empty() {
        return Ok(json!({ "ok": true, "key": key }));
    }
    let draft = inner.st.lock().unwrap().drafts.contains_key(key);
    let path = match inner.config.mode {
        HermesMode::Local => {
            body.insert("profile".into(), json!(profile));
            format!("/api/sessions/{}", enc(&stored))
        }
        HermesMode::Api => format!("/api/sessions/{}", enc(&stored)),
    };
    let result = if draft { Err(RpcError::new("NOT_FOUND", "draft")) } else { rest(inner, reqwest::Method::PATCH, &path, Some(Value::Object(body.clone())), timeout).await };
    match result {
        Ok(_) => {}
        Err(e) if e.code == "NOT_FOUND" && inner.config.mode == HermesMode::Local => {
            // Not written yet: queue the title on the live session (applied after its first turn).
            let sid = inner.st.lock().unwrap().by_session.get(&(profile.clone(), stored.clone())).cloned();
            match (sid, label.as_ref().and_then(Value::as_str)) {
                (Some(sid), Some(title)) => {
                    rpc(inner, "session.title", json!({ "session_id": sid, "title": title }), timeout).await?;
                }
                (None, Some(_)) => return Err(RpcError::new("NOT_FOUND", "That thread doesn't exist anymore.")),
                _ => {}
            }
            let mut st = inner.st.lock().unwrap();
            if let (Some(row), Some(title)) = (st.drafts.get_mut(key), label.as_ref().and_then(Value::as_str)) {
                row["label"] = json!(title);
                row["derivedTitle"] = json!(title);
            }
        }
        Err(e) => return Err(e),
    }
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": profile, "reason": "patch" }));
    Ok(json!({ "ok": true, "key": key }))
}

async fn sessions_delete(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let key = params.get("key").and_then(Value::as_str).ok_or_else(|| RpcError::new("INVALID_REQUEST", "key is required"))?;
    let (profile, stored) = resolve_key(inner, key)?;
    let is_main = matches!(parse_key(key), Some(KeyRef::Main(_)));
    let Some(stored) = stored else { return Ok(json!({ "ok": true, "deleted": false })) };
    let (sid, was_draft) = {
        let mut st = inner.st.lock().unwrap();
        let sid = st.by_session.remove(&(profile.clone(), stored.clone()));
        if let Some(sid) = &sid {
            st.live.remove(sid);
        }
        (sid, st.drafts.remove(key).is_some())
    };
    if let Some(sid) = sid {
        // Hermes refuses to delete a session that's live on a socket.
        let _ = rpc(inner, "session.close", json!({ "session_id": sid }), timeout).await;
    }
    if !was_draft {
        let path = match inner.config.mode {
            HermesMode::Local => format!("/api/sessions/{}?profile={}", enc(&stored), enc(&profile)),
            HermesMode::Api => format!("/api/sessions/{}", enc(&stored)),
        };
        match rest(inner, reqwest::Method::DELETE, &path, None, timeout).await {
            Ok(_) => {}
            Err(e) if e.code == "NOT_FOUND" => {}
            Err(e) => return Err(e),
        }
    }
    if is_main {
        forget_main_session(inner, &profile);
    }
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": profile, "reason": "delete" }));
    Ok(json!({ "ok": true, "deleted": true }))
}

// ---- transcripts ----------------------------------------------------------------------

async fn chat_history(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?;
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 500);
    let (profile, stored) = resolve_key(inner, key)?;
    let in_flight = in_flight_run(inner, key);
    let Some(stored) = stored else {
        return Ok(json!({ "sessionKey": key, "messages": [], "hasMore": false, "inFlightRun": in_flight }));
    };
    let path = match inner.config.mode {
        HermesMode::Local => format!("/api/sessions/{}/messages?profile={}&limit={limit}&order=latest", enc(&stored), enc(&profile)),
        HermesMode::Api => format!("/api/sessions/{}/messages?limit={limit}&order=latest", enc(&stored)),
    };
    let rows = match rest(inner, reqwest::Method::GET, &path, None, timeout).await {
        Ok(v) => v.get("messages").or_else(|| v.get("data")).and_then(Value::as_array).cloned().unwrap_or_default(),
        // Created but not written yet (Hermes writes the row on the first prompt).
        Err(e) if e.code == "NOT_FOUND" => Vec::new(),
        Err(e) => return Err(e),
    };
    let has_more = rows.len() as u64 >= limit;
    let messages = convert_messages(&rows);
    Ok(json!({
        "sessionKey": key,
        "sessionId": stored,
        "messages": messages,
        "hasMore": has_more,
        "inFlightRun": in_flight,
    }))
}

fn in_flight_run(inner: &Inner, key: &str) -> Value {
    let st = inner.st.lock().unwrap();
    st.turns
        .values()
        .find(|t| t.key == key)
        .map(|t| {
            json!({
                "runId": t.run_id,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": t.text.trim_start() }] },
            })
        })
        .unwrap_or(Value::Null)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(o) => o.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

/// Tool output for display: terminal-ish `{output, exit_code, error}` JSON becomes plain text.
fn tool_output_text(content: &Value) -> (String, bool) {
    let parsed = match content {
        Value::String(s) => serde_json::from_str::<Value>(s).ok().filter(Value::is_object),
        Value::Object(_) => Some(content.clone()),
        _ => None,
    };
    let raw = match content {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let Some(obj) = parsed else { return (raw, false) };
    let error = obj.get("error").and_then(Value::as_str).filter(|e| !e.trim().is_empty()).map(str::to_string);
    let status = obj.get("status").and_then(Value::as_str).unwrap_or("");
    let is_error = error.is_some() || matches!(status, "error" | "blocked" | "failed" | "denied");
    let text = match obj.get("output").and_then(Value::as_str) {
        Some(output) => {
            let mut text = output.to_string();
            if let Some(e) = &error {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(e);
            }
            match obj.get("exit_code").and_then(Value::as_i64) {
                Some(code) if code != 0 && error.is_none() => text.push_str(&format!("\n(exit code {code})")),
                _ => {}
            }
            text
        }
        None => match (&error, obj.get("user_summary").and_then(Value::as_str)) {
            (_, Some(summary)) if is_error => summary.to_string(),
            (Some(e), _) => e.clone(),
            _ => raw,
        },
    };
    (text, is_error)
}

fn tool_arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
        Some(v) => v.clone(),
        None => json!({}),
    }
}

/// Hermes transcript rows (OpenAI-shaped: user/assistant(+tool_calls)/tool) → contract ChatMessages.
fn convert_messages(rows: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.get("display_kind").and_then(Value::as_str) == Some("hidden") {
            continue;
        }
        if row.get("active").is_some_and(|a| !truthy(Some(a)) && !a.is_null()) {
            continue;
        }
        let role = row.get("role").and_then(Value::as_str).unwrap_or("");
        let id = match row.get("id").or_else(|| row.get("row_id")) {
            Some(Value::Number(n)) => format!("h:{n}"),
            Some(Value::String(s)) => format!("h:{s}"),
            _ => format!("h:i{i}"),
        };
        let ts = secs_to_ms(row.get("timestamp"));
        let content = row.get("display_content").filter(|c| !c.is_null()).or_else(|| row.get("content")).unwrap_or(&Value::Null);
        let mut message = match role {
            "user" => {
                let text = text_of(content);
                if text.trim().is_empty() {
                    continue;
                }
                json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
            }
            "assistant" => {
                let mut blocks = Vec::new();
                let reasoning = row
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .or_else(|| row.get("reasoning_content").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|r| !r.is_empty());
                if let Some(r) = reasoning {
                    blocks.push(json!({ "type": "thinking", "thinking": r }));
                }
                let text = text_of(content);
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text.trim_start() }));
                }
                for call in row.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let call_id = call.get("id").or_else(|| call.get("call_id")).and_then(Value::as_str).unwrap_or_default();
                    let name = call.pointer("/function/name").or_else(|| call.get("name")).and_then(Value::as_str).unwrap_or("tool");
                    let args = tool_arguments(call.pointer("/function/arguments").or_else(|| call.get("arguments")));
                    blocks.push(json!({ "type": "toolCall", "id": call_id, "name": name, "arguments": args }));
                }
                if blocks.is_empty() {
                    continue;
                }
                let mut m = json!({ "role": "assistant", "content": blocks });
                let failed = row.get("finish_reason").and_then(Value::as_str) == Some("error")
                    || row.get("display_kind").and_then(Value::as_str).is_some_and(|k| k.contains("fail") || k.contains("error"));
                if failed {
                    m["stopReason"] = json!("error");
                }
                m
            }
            "tool" => {
                let (text, is_error) = tool_output_text(content);
                json!({
                    "role": "toolResult",
                    "toolCallId": row.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                    "toolName": row.get("tool_name").or_else(|| row.get("name")).and_then(Value::as_str).unwrap_or("tool"),
                    "isError": is_error,
                    "content": [{ "type": "text", "text": text }],
                })
            }
            _ => continue,
        };
        if let Some(ts) = ts {
            message["timestamp"] = json!(ts);
        }
        message["__openclaw"] = json!({ "id": id });
        out.push(message);
    }
    out
}

// ---- turns: Hermes events → contract events ------------------------------------------

fn new_turn(run_id: String, key: &str, agent_id: &str, profile: &str, stored: &str) -> Turn {
    Turn {
        run_id,
        key: key.to_string(),
        agent_id: agent_id.to_string(),
        profile: profile.to_string(),
        stored: stored.to_string(),
        ..Default::default()
    }
}

fn turn_started(turn: &Turn) -> Emits {
    vec![
        (
            "sessions.changed".into(),
            json!({ "sessionKey": turn.key, "agentId": turn.agent_id, "phase": "start", "runId": turn.run_id }),
        ),
        ("chat".into(), json!({ "sessionKey": turn.key, "runId": turn.run_id, "state": "status", "phase": "starting_model" })),
    ]
}

fn turn_phase(turn: &mut Turn, phase: &str) -> Emits {
    if turn.phase == phase || !turn.text.is_empty() {
        return Vec::new();
    }
    turn.phase = phase.to_string();
    vec![("chat".into(), json!({ "sessionKey": turn.key, "runId": turn.run_id, "state": "status", "phase": phase }))]
}

fn turn_delta(turn: &mut Turn, delta: &str) -> Emits {
    if delta.is_empty() {
        return Vec::new();
    }
    turn.text.push_str(delta);
    turn.phase.clear();
    let text = turn.text.trim_start();
    if text.is_empty() {
        return Vec::new();
    }
    vec![(
        "chat".into(),
        json!({
            "sessionKey": turn.key,
            "runId": turn.run_id,
            "state": "delta",
            "deltaText": delta,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }], "timestamp": now_ms() },
        }),
    )]
}

/// Close the segment being streamed into a transcript message (plus any tool calls).
fn turn_seal(turn: &mut Turn, extra: Vec<Value>) -> Option<Value> {
    let mut blocks = Vec::new();
    let reasoning = std::mem::take(&mut turn.reasoning);
    if !reasoning.trim().is_empty() {
        blocks.push(json!({ "type": "thinking", "thinking": reasoning.trim() }));
    }
    let text = std::mem::take(&mut turn.text);
    if !text.trim().is_empty() {
        blocks.push(json!({ "type": "text", "text": text.trim() }));
        turn.emitted_text = true;
    }
    blocks.extend(extra);
    if blocks.is_empty() {
        return None;
    }
    turn.seq += 1;
    Some(json!({
        "role": "assistant",
        "content": blocks,
        "timestamp": now_ms(),
        "__openclaw": { "id": format!("live:{}:{}", turn.run_id, turn.seq), "runId": turn.run_id },
    }))
}

fn session_message(turn: &Turn, message: Value) -> (String, Value) {
    ("session.message".into(), json!({ "sessionKey": turn.key, "runId": turn.run_id, "message": message }))
}

fn turn_interim(turn: &mut Turn, text: &str, already_streamed: bool) -> Emits {
    if !already_streamed && !text.trim().is_empty() {
        turn.text = text.to_string();
    }
    turn_seal(turn, Vec::new()).map(|m| vec![session_message(turn, m)]).unwrap_or_default()
}

fn turn_tool_start(turn: &mut Turn, tool_id: &str, name: &str, args: Value, title: Option<&str>) -> Emits {
    turn.tools.insert(tool_id.to_string(), name.to_string());
    turn.phase.clear();
    let call = json!({ "type": "toolCall", "id": tool_id, "name": name, "arguments": args });
    let mut emits = Vec::new();
    if let Some(m) = turn_seal(turn, vec![call]) {
        emits.push(session_message(turn, m));
    }
    let mut data = json!({ "kind": "tool", "phase": "start", "toolCallId": tool_id, "name": name });
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        data["title"] = json!(t);
    }
    emits.push(("agent".into(), json!({ "sessionKey": turn.key, "runId": turn.run_id, "stream": "tool", "data": data })));
    emits
}

fn turn_tool_end(turn: &mut Turn, tool_id: &str, name: Option<&str>, text: String, is_error: bool) -> Emits {
    let name = name.map(str::to_string).or_else(|| turn.tools.get(tool_id).cloned()).unwrap_or_else(|| "tool".into());
    let message = json!({
        "role": "toolResult",
        "toolCallId": tool_id,
        "toolName": name,
        "isError": is_error,
        "content": [{ "type": "text", "text": text }],
        "timestamp": now_ms(),
        "__openclaw": { "id": format!("live:{}:r:{tool_id}", turn.run_id), "runId": turn.run_id },
    });
    vec![
        session_message(turn, message),
        (
            "agent".into(),
            json!({
                "sessionKey": turn.key,
                "runId": turn.run_id,
                "stream": "tool",
                "data": {
                    "kind": "tool", "phase": "end", "toolCallId": tool_id, "name": name,
                    "status": if is_error { "failed" } else { "completed" }, "isError": is_error,
                },
            }),
        ),
    ]
}

/// The turn ended: `status` is Hermes' complete|error|interrupted.
fn turn_complete(turn: &mut Turn, final_text: Option<&str>, status: &str, error: Option<&str>) -> Emits {
    if turn.text.trim().is_empty() && !turn.emitted_text {
        if let Some(text) = final_text.filter(|t| !t.trim().is_empty()) {
            turn.text = text.to_string();
        }
    }
    if !turn.reasoning_available.trim().is_empty() && turn.reasoning.trim().is_empty() {
        let available = std::mem::take(&mut turn.reasoning_available);
        let echoes_reply = final_text.is_some_and(|t| t.trim() == available.trim()) || turn.text.trim() == available.trim();
        if !echoes_reply {
            turn.reasoning = available;
        }
    }
    let mut emits = Vec::new();
    let final_message = turn_seal(turn, Vec::new());
    if let Some(m) = &final_message {
        emits.push(session_message(turn, m.clone()));
    }
    let base = json!({ "sessionKey": turn.key, "runId": turn.run_id });
    let mut chat = base.clone();
    let phase = match status {
        "interrupted" | "cancelled" | "aborted" => {
            chat["state"] = json!("aborted");
            "end"
        }
        "error" | "failed" => {
            chat["state"] = json!("error");
            chat["errorMessage"] = json!(error.filter(|e| !e.is_empty()).unwrap_or("The run failed."));
            "error"
        }
        _ => {
            chat["state"] = json!("final");
            if let Some(m) = final_message {
                chat["message"] = m;
            }
            "end"
        }
    };
    emits.push(("chat".into(), chat));
    emits.push((
        "sessions.changed".into(),
        json!({ "sessionKey": turn.key, "agentId": turn.agent_id, "phase": phase, "runId": turn.run_id }),
    ));
    emits
}

fn handle_event(inner: &Arc<Inner>, params: &Value) {
    let ty = params.get("type").and_then(Value::as_str).unwrap_or("");
    let sid = params.get("session_id").and_then(Value::as_str).unwrap_or("");
    let p = params.get("payload").cloned().unwrap_or(Value::Null);
    match ty {
        "sessions.changed" => return schedule_sessions_changed(inner),
        "cron.changed" => return schedule_cron_check(inner),
        "request.cancel" => {
            if let Some(id) = p.get("id").and_then(Value::as_str) {
                cancel_approval_by_srq(inner, id);
            }
            return;
        }
        _ => {}
    }
    if sid.is_empty() {
        return;
    }
    let mut emits: Emits = Vec::new();
    {
        let mut st = inner.st.lock().unwrap();
        let Some(live) = st.live.get(sid).cloned() else { return };
        match ty {
            "session.title" => {
                emits.push(("sessions.changed".into(), json!({ "sessionKey": live.key, "agentId": live.profile, "reason": "title" })));
            }
            "session.reclaimed" => {
                st.live.remove(sid);
                st.by_session.remove(&(live.profile.clone(), live.stored.clone()));
            }
            "message.start" => {
                let run_id = st.queued_runs.get_mut(sid).and_then(VecDeque::pop_front).unwrap_or_else(new_run_id);
                let turn = new_turn(run_id, &live.key, &live.profile, &live.profile, &live.stored);
                emits.extend(turn_started(&turn));
                st.turns.insert(sid.to_string(), turn);
            }
            "message.complete" => {
                if let Some(mut turn) = st.turns.remove(sid) {
                    let status = p.get("status").and_then(Value::as_str).unwrap_or("complete");
                    let error = p.get("error").and_then(Value::as_str).or_else(|| p.get("failure_reason").and_then(Value::as_str));
                    if let Some(r) = p.get("reasoning").and_then(Value::as_str) {
                        if turn.reasoning.is_empty() && turn.reasoning_available.is_empty() {
                            turn.reasoning_available = r.to_string();
                        }
                    }
                    emits.extend(turn_complete(&mut turn, p.get("text").and_then(Value::as_str), status, error));
                }
            }
            "error" => {
                if let Some(mut turn) = st.turns.remove(sid) {
                    emits.extend(turn_complete(&mut turn, None, "error", p.get("message").and_then(Value::as_str)));
                }
            }
            "message.delta" | "reasoning.delta" | "reasoning.available" | "thinking.delta" | "tool.generating"
            | "tool.start" | "tool.complete" | "message.interim" | "status.update" => {
                if !st.turns.contains_key(sid) {
                    // Attached mid-turn (or message.start was missed).
                    let run_id = st.queued_runs.get_mut(sid).and_then(VecDeque::pop_front).unwrap_or_else(new_run_id);
                    let turn = new_turn(run_id, &live.key, &live.profile, &live.profile, &live.stored);
                    emits.extend(turn_started(&turn));
                    st.turns.insert(sid.to_string(), turn);
                }
                let turn = st.turns.get_mut(sid).expect("turn just ensured");
                let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "message.delta" => emits.extend(turn_delta(turn, text)),
                    "reasoning.delta" => turn.reasoning.push_str(text),
                    "reasoning.available" => turn.reasoning_available = text.to_string(),
                    "thinking.delta" => emits.extend(turn_phase(turn, "thinking")),
                    "tool.generating" => emits.extend(turn_phase(turn, "working")),
                    "status.update" => {
                        if let Some(kind) = p.get("kind").and_then(Value::as_str).filter(|k| matches!(*k, "compacting" | "lifecycle" | "status")) {
                            let phase = if kind == "compacting" { "compacting" } else { "working" };
                            emits.extend(turn_phase(turn, phase));
                        }
                    }
                    "message.interim" => {
                        let already = p.get("already_streamed").and_then(Value::as_bool).unwrap_or(true);
                        emits.extend(turn_interim(turn, text, already));
                    }
                    "tool.start" => {
                        let tool_id = p.get("tool_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                        let args = p.get("args").cloned().filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
                        let title = p.get("context").or_else(|| p.get("preview")).and_then(Value::as_str).map(str::to_string);
                        emits.extend(turn_tool_start(turn, &tool_id, &name, args, title.as_deref()));
                    }
                    "tool.complete" => {
                        let tool_id = p.get("tool_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        let (text, is_error) = match p.get("result").filter(|r| !r.is_null()) {
                            Some(result) => tool_output_text(result),
                            None => {
                                let text = p.get("result_text").or_else(|| p.get("summary")).and_then(Value::as_str).unwrap_or_default();
                                tool_output_text(&json!(text))
                            }
                        };
                        emits.extend(turn_tool_end(turn, &tool_id, p.get("name").and_then(Value::as_str), text, is_error));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    flush(inner, emits);
}

fn new_run_id() -> String {
    format!("hermes-{}", uuid::Uuid::new_v4().simple())
}

fn schedule_sessions_changed(inner: &Arc<Inner>) {
    {
        let mut st = inner.st.lock().unwrap();
        if st.sessions_emit_scheduled {
            return;
        }
        st.sessions_emit_scheduled = true;
    }
    let inner = inner.clone();
    spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        inner.st.lock().unwrap().sessions_emit_scheduled = false;
        emit(&inner, "sessions.changed", json!({ "reason": "hermes" }));
        // Cron runs write sessions; it's also the only signal for non-default profiles' jobs.
        schedule_cron_check(&inner);
    });
}

/// After a reconnect: re-attach sessions that had a turn or an approval in flight.
async fn reattach_sessions(inner: &Arc<Inner>) {
    let targets = std::mem::take(&mut inner.st.lock().unwrap().reattach);
    for (profile, stored, key) in targets {
        let params = json!({ "session_id": stored, "profile": profile, "omit_messages": true, "lazy": true });
        let res = match rpc(inner, "session.resume", params, DEFAULT_CALL_TIMEOUT).await {
            Ok(res) => res,
            Err(e) => {
                log::warn!("hermes: couldn't re-attach {key}: {e}");
                let detached = inner.st.lock().unwrap().turns.remove(&format!("detached:{profile}:{stored}"));
                if let Some(mut turn) = detached {
                    let emits = turn_complete(&mut turn, None, "error", Some("Lost track of this reply after reconnecting."));
                    flush(inner, emits);
                }
                continue;
            }
        };
        let sid = res.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
        if sid.is_empty() {
            continue;
        }
        remember_live(inner, &sid, &profile, &stored, &key);
        let running = res.get("running").and_then(Value::as_bool).unwrap_or(false)
            || res.pointer("/inflight/streaming").and_then(Value::as_bool).unwrap_or(false);
        let emits = {
            let mut st = inner.st.lock().unwrap();
            let detached = st.turns.remove(&format!("detached:{profile}:{stored}"));
            match (detached, running) {
                (Some(mut turn), true) => {
                    if let Some(text) = res.pointer("/inflight/assistant").and_then(Value::as_str) {
                        turn.text = text.to_string();
                    }
                    st.turns.insert(sid.clone(), turn);
                    Vec::new()
                }
                (Some(mut turn), false) => turn_complete(&mut turn, None, "complete", None),
                (None, true) => {
                    let mut turn = new_turn(new_run_id(), &key, &profile, &profile, &stored);
                    turn.text = res.pointer("/inflight/assistant").and_then(Value::as_str).unwrap_or_default().to_string();
                    let emits = turn_started(&turn);
                    st.turns.insert(sid.clone(), turn);
                    emits
                }
                (None, false) => Vec::new(),
            }
        };
        flush(inner, emits);
        for req in res.get("open_requests").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(method), Some(id)) = (req.get("method").and_then(Value::as_str), req.get("id")) {
                handle_server_request(inner, method, id.clone(), &req["params"]);
            }
        }
    }
}

// ---- chat.send / abort -----------------------------------------------------------------

/// A runtime session id for (profile, stored), attaching (`session.resume`) when needed.
async fn ensure_live(inner: &Arc<Inner>, profile: &str, stored: &str, key: &str, timeout: Duration) -> Result<String, RpcError> {
    if let Some(sid) = inner.st.lock().unwrap().by_session.get(&(profile.to_string(), stored.to_string())).cloned() {
        return Ok(sid);
    }
    // Lazy: no agent is built until a prompt arrives; attaching alongside other clients is enough
    // to receive the session's events.
    let params = json!({ "session_id": stored, "profile": profile, "omit_messages": true, "lazy": true });
    let res = rpc(inner, "session.resume", params, timeout).await?;
    let sid = res
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::new("HERMES_ERROR", "Hermes didn't attach the session"))?
        .to_string();
    remember_live(inner, &sid, profile, stored, key);
    for req in res.get("open_requests").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(method), Some(id)) = (req.get("method").and_then(Value::as_str), req.get("id")) {
            handle_server_request(inner, method, id.clone(), &req["params"]);
        }
    }
    Ok(sid)
}

fn watch_session(inner: &Arc<Inner>, key: &str) {
    let connected = inner.conn.lock().unwrap().as_ref().is_some_and(|c| c.writer.is_some());
    let chat_like = key.ends_with(":main") || key.contains(":dashboard:");
    if !connected || !chat_like || inner.config.mode != HermesMode::Local {
        return;
    }
    let Ok((profile, Some(stored))) = resolve_key(inner, key) else { return };
    if !profile_allowed(inner, &profile) || inner.st.lock().unwrap().drafts.contains_key(key) {
        return;
    }
    let (inner, key) = (inner.clone(), key.to_string());
    spawn(async move {
        if let Err(e) = ensure_live(&inner, &profile, &stored, &key, DEFAULT_CALL_TIMEOUT).await {
            log::info!("hermes: couldn't attach {key}: {e}");
        }
    });
}

fn send_params(params: &Value) -> Result<(String, String), RpcError> {
    let key = params
        .get("sessionKey")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?
        .to_string();
    let message = params.get("message").map(text_of).unwrap_or_default();
    let has_attachments = params.get("attachments").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    if has_attachments {
        return Err(RpcError::new("UNSUPPORTED", "Hermes guys can't take attachments from littleguys yet."));
    }
    if message.trim().is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "message is empty"));
    }
    Ok((key, message))
}

fn user_message(text: &str, id: String, run_id: &str) -> Value {
    json!({
        "role": "user",
        "content": [{ "type": "text", "text": text }],
        "timestamp": now_ms(),
        "__openclaw": { "id": id, "runId": run_id },
    })
}

async fn local_chat_send(inner: &Arc<Inner>, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let (key, message) = send_params(params)?;
    let (profile, stored) = resolve_key(inner, &key)?;
    if !profile_allowed(inner, &profile) {
        return Err(RpcError::new("INVALID_REQUEST", format!("unknown guy {profile}")));
    }
    let (sid, stored) = match stored {
        Some(stored) => (ensure_live(inner, &profile, &stored, &key, timeout).await?, stored),
        None => {
            // First message to this guy's main chat: Hermes' canonical "Bot Chat".
            let res = rpc(inner, "session.create", json!({ "profile": profile, "title": MAIN_TITLE, "source": SESSION_SOURCE }), timeout).await?;
            let sid = res.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
            let stored = res.get("stored_session_id").and_then(Value::as_str).unwrap_or(&sid).to_string();
            set_main_session(inner, &profile, &stored);
            remember_live(inner, &sid, &profile, &stored, &key);
            (sid, stored)
        }
    };
    // Slash commands go through Hermes' own pipeline; `prompt.submit` would hand them to the model.
    let mut prompt = message.clone();
    if let Some(command) = slash_command(&message) {
        match run_slash(inner, &sid, command, timeout).await? {
            Slash::Send(composed) => prompt = composed,
            Slash::Output(output) => return Ok(slash_output(inner, &key, &message, &output)),
        }
    }
    let run_id = new_run_id();
    inner.st.lock().unwrap().queued_runs.entry(sid.clone()).or_default().push_back(run_id.clone());
    let submit = |sid: String| rpc(inner, "prompt.submit", json!({ "session_id": sid, "text": prompt }), timeout);
    let res = match submit(sid.clone()).await {
        Ok(res) => res,
        Err(e) if e.code.starts_with("HERMES_4") => {
            // The runtime session was reaped: attach again and retry once.
            {
                let mut st = inner.st.lock().unwrap();
                st.live.remove(&sid);
                st.by_session.remove(&(profile.clone(), stored.clone()));
                st.queued_runs.remove(&sid);
            }
            let fresh = ensure_live(inner, &profile, &stored, &key, timeout).await.map_err(|_| e)?;
            inner.st.lock().unwrap().queued_runs.entry(fresh.clone()).or_default().push_back(run_id.clone());
            submit(fresh).await?
        }
        Err(e) => {
            if let Some(q) = inner.st.lock().unwrap().queued_runs.get_mut(&sid) {
                q.retain(|r| r != &run_id);
            }
            return Err(e);
        }
    };
    let status = res.get("status").and_then(Value::as_str).unwrap_or("streaming");
    let run_id = if matches!(status, "steered" | "redirected") {
        // Folded into the turn already running: report that run.
        let mut st = inner.st.lock().unwrap();
        for q in st.queued_runs.values_mut() {
            q.retain(|r| r != &run_id);
        }
        st.turns.values().find(|t| t.key == key).map(|t| t.run_id.clone()).unwrap_or(run_id)
    } else {
        run_id
    };
    let id = res
        .get("user_row_id")
        .and_then(Value::as_u64)
        .map(|r| format!("h:{r}"))
        .unwrap_or_else(|| format!("live:{run_id}:user"));
    if let Some(row) = inner.st.lock().unwrap().drafts.get_mut(&key) {
        row["updatedAt"] = json!(now_ms());
        row["lastMessagePreview"] = json!(truncate(&message, 120));
    }
    note_last_text(inner, &key, &message);
    emit(inner, "session.message", json!({ "sessionKey": key, "runId": run_id, "message": user_message(&message, id, &run_id) }));
    Ok(json!({ "runId": run_id, "status": "started", "hermesStatus": status }))
}

/// `/name args…` when the text starts with a command token (not a path like `/Users/me`).
fn slash_command(message: &str) -> Option<&str> {
    let text = message.trim();
    let name = text.strip_prefix('/')?.split(char::is_whitespace).next()?;
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    valid.then_some(text)
}

enum Slash {
    /// Shown in the thread; nothing reaches the model.
    Output(String),
    /// The command composed a prompt (skills, /queue, /plan…): run it as the turn.
    Send(String),
}

/// What Hermes' TUI and desktop do with a slash command: `slash.exec`, falling back to
/// `command.dispatch` for the ones it refuses with 4018 (skills, bundles, pending-input
/// built-ins), following aliases.
async fn run_slash(inner: &Arc<Inner>, sid: &str, command: &str, timeout: Duration) -> Result<Slash, RpcError> {
    let mut command = command.to_string();
    for _ in 0..3 {
        let res = match rpc(inner, "slash.exec", json!({ "session_id": sid, "command": command }), timeout).await {
            Err(e) if e.code == "HERMES_4018" => {
                let rest = command.trim_start_matches('/');
                let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                rpc(inner, "command.dispatch", json!({ "session_id": sid, "name": name, "arg": arg.trim() }), timeout).await?
            }
            res => res?,
        };
        let field = |k: &str| nonempty(&res, k).map(str::to_string);
        match res.get("type").and_then(Value::as_str) {
            Some("send" | "skill") => {
                if let Some(message) = field("message") {
                    return Ok(Slash::Send(message));
                }
            }
            Some("alias") => {
                if let Some(target) = field("target") {
                    command = if target.starts_with('/') { target } else { format!("/{target}") };
                    continue;
                }
            }
            _ => {}
        }
        let output = [field("notice"), field("output").or_else(|| field("message")), field("warning")];
        let output: Vec<String> = output.into_iter().flatten().collect();
        return Ok(Slash::Output(if output.is_empty() { "(no output)".into() } else { output.join("\n\n") }));
    }
    Err(RpcError::new("INVALID_REQUEST", "That command points at itself."))
}

/// A command's output, shown under what was typed. Hermes doesn't keep either in the
/// transcript (its TUI shows them in a pager), so they're gone after a reload.
fn slash_output(inner: &Inner, key: &str, typed: &str, output: &str) -> Value {
    let run_id = new_run_id();
    let text = if output.contains('\n') {
        let fence = if output.contains("```") { "~~~~" } else { "```" };
        format!("{fence}text\n{output}\n{fence}")
    } else {
        output.to_string()
    };
    emit(inner, "session.message", json!({ "sessionKey": key, "runId": run_id, "message": user_message(typed, format!("live:{run_id}:user"), &run_id) }));
    let reply = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "timestamp": now_ms(),
        "__openclaw": { "id": format!("live:{run_id}:output"), "runId": run_id },
    });
    emit(inner, "session.message", json!({ "sessionKey": key, "runId": run_id, "message": reply }));
    json!({ "runId": run_id, "status": "ok" })
}

async fn chat_abort(inner: &Arc<Inner>, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let key = params.get("sessionKey").and_then(Value::as_str).ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?;
    match inner.config.mode {
        HermesMode::Local => {
            let (profile, stored) = resolve_key(inner, key)?;
            let Some(stored) = stored else { return Ok(json!({ "ok": true, "aborted": false })) };
            let sid = inner.st.lock().unwrap().by_session.get(&(profile, stored)).cloned();
            let Some(sid) = sid else { return Ok(json!({ "ok": true, "aborted": false })) };
            rpc(inner, "session.interrupt", json!({ "session_id": sid }), timeout).await?;
            Ok(json!({ "ok": true, "aborted": true }))
        }
        HermesMode::Api => {
            let run = {
                let st = inner.st.lock().unwrap();
                params
                    .get("runId")
                    .and_then(Value::as_str)
                    .filter(|r| st.turns.contains_key(*r))
                    .map(str::to_string)
                    .or_else(|| st.turns.iter().find(|(_, t)| t.key == key).map(|(id, _)| id.clone()))
            };
            let Some(run) = run else { return Ok(json!({ "ok": true, "aborted": false })) };
            rest(inner, reqwest::Method::POST, &format!("/v1/runs/{}/stop", enc(&run)), Some(json!({})), timeout).await?;
            Ok(json!({ "ok": true, "aborted": true }))
        }
    }
}

// ---- api mode: runs --------------------------------------------------------------------

async fn api_main_session(inner: &Inner, profile: &str, timeout: Duration) -> Result<String, RpcError> {
    let path = format!("/api/sessions?limit=5&title={}&include_hidden=1", enc(MAIN_TITLE));
    if let Ok(v) = rest(inner, reqwest::Method::GET, &path, None, timeout).await {
        let found = v.get("data").and_then(Value::as_array).and_then(|rows| {
            rows.iter().find(|r| r.get("title").and_then(Value::as_str) == Some(MAIN_TITLE)).and_then(|r| r.get("id").and_then(Value::as_str))
        });
        if let Some(id) = found {
            set_main_session(inner, profile, id);
            return Ok(id.to_string());
        }
    }
    let res = rest(inner, reqwest::Method::POST, "/api/sessions", Some(json!({ "title": MAIN_TITLE, "source": "api_server" })), timeout).await?;
    let id = res.pointer("/session/id").and_then(Value::as_str).ok_or_else(|| RpcError::new("HERMES_HTTP", "Hermes didn't return a session id"))?;
    set_main_session(inner, profile, id);
    Ok(id.to_string())
}

async fn api_chat_send(inner: &Arc<Inner>, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let (key, message) = send_params(params)?;
    let (profile, stored) = resolve_key(inner, &key)?;
    let stored = match stored {
        Some(s) => s,
        None => api_main_session(inner, &profile, timeout).await?,
    };
    if let Some(busy) = inner.st.lock().unwrap().turns.values().find(|t| t.key == key) {
        return Err(RpcError::new("BUSY", format!("{} is still replying in this thread (run {}).", profile, busy.run_id)));
    }
    let mut body = json!({ "input": message, "session_id": stored });
    if let Some(k) = params.get("idempotencyKey").and_then(Value::as_str) {
        body["idempotency_key"] = json!(k);
    }
    let res = rest(inner, reqwest::Method::POST, "/v1/runs", Some(body), timeout).await?;
    let run_id = res
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("HERMES_HTTP", "Hermes didn't start a run"))?
        .to_string();
    let turn = new_turn(run_id.clone(), &key, &profile, &profile, &stored);
    let started = turn_started(&turn);
    inner.st.lock().unwrap().turns.insert(run_id.clone(), turn);
    note_last_text(inner, &key, &message);
    emit(inner, "session.message", json!({
        "sessionKey": key, "runId": run_id,
        "message": user_message(&message, format!("live:{run_id}:user"), &run_id),
    }));
    flush(inner, started);
    let follower = inner.clone();
    let run = run_id.clone();
    spawn(async move { api_follow_run(follower, run).await });
    Ok(json!({ "runId": run_id, "status": "started" }))
}

/// Stream `/v1/runs/{id}/events` (SSE) into contract events until the run ends.
async fn api_follow_run(inner: Arc<Inner>, run_id: String) {
    for _attempt in 0..30 {
        let Some(conn) = inner.conn.lock().unwrap().clone() else { break };
        let mut req = inner.http.get(format!("{}/v1/runs/{}/events", conn.base, enc(&run_id)));
        if let Some(token) = &conn.token {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let mut stream = resp.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(chunk) = chunk else { break };
                    buf.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                    while let Some(pos) = buf.find("\n\n") {
                        let block: String = buf.drain(..pos + 2).collect();
                        let data: String = block
                            .lines()
                            .filter_map(|l| l.strip_prefix("data:"))
                            .map(str::trim_start)
                            .collect::<Vec<_>>()
                            .join("\n");
                        if data.is_empty() {
                            continue;
                        }
                        if let Ok(event) = serde_json::from_str::<Value>(&data) {
                            if handle_api_event(&inner, &run_id, &event) {
                                return;
                            }
                        }
                    }
                }
            }
            Ok(resp) if resp.status().as_u16() == 404 => break,
            _ => {}
        }
        // Stream ended without a terminal event: ask for the run's status.
        if let Ok(status) = rest(&inner, reqwest::Method::GET, &format!("/v1/runs/{}", enc(&run_id)), None, Duration::from_secs(10)).await {
            let s = status.get("status").and_then(Value::as_str).unwrap_or("");
            if matches!(s, "completed" | "failed" | "cancelled" | "interrupted") {
                let mut event = status.clone();
                event["event"] = json!(format!("run.{s}"));
                handle_api_event(&inner, &run_id, &event);
                return;
            }
            if let Some(approval) = status.get("approval").filter(|a| a.is_object()) {
                handle_api_event(&inner, &run_id, approval);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let turn = inner.st.lock().unwrap().turns.remove(&run_id);
    if let Some(mut turn) = turn {
        let emits = turn_complete(&mut turn, None, "error", Some("Lost track of this reply."));
        flush(&inner, emits);
    }
}

/// One `/v1/runs` SSE event. Returns true when the run is over.
fn handle_api_event(inner: &Inner, run_id: &str, event: &Value) -> bool {
    let name = event.get("event").and_then(Value::as_str).unwrap_or("");
    let mut emits: Emits = Vec::new();
    let mut done = false;
    let mut new_approval: Option<(String, Approval)> = None;
    let mut resolved_approval: Option<String> = None;
    {
        let mut st = inner.st.lock().unwrap();
        let Some(turn) = st.turns.get_mut(run_id) else { return true };
        match name {
            "message.delta" => emits.extend(turn_delta(turn, event.get("delta").and_then(Value::as_str).unwrap_or(""))),
            "message.interim" => {
                let already = event.get("already_streamed").and_then(Value::as_bool).unwrap_or(true);
                emits.extend(turn_interim(turn, event.get("text").and_then(Value::as_str).unwrap_or(""), already));
            }
            "reasoning.available" => turn.reasoning_available = event.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            "reasoning.delta" => turn.reasoning.push_str(event.get("delta").or_else(|| event.get("text")).and_then(Value::as_str).unwrap_or("")),
            "tool.started" => {
                let name = event.get("tool").and_then(Value::as_str).unwrap_or("tool").to_string();
                let preview = event.get("preview").and_then(Value::as_str).unwrap_or("").to_string();
                let tool_id = format!("{run_id}:t{}", turn.tools.len() + 1);
                turn.api_tools.push_back((name.clone(), tool_id.clone()));
                emits.extend(turn_tool_start(turn, &tool_id, &name, json!({ "preview": preview }), Some(&preview)));
            }
            "tool.completed" | "tool.failed" => {
                let name = event.get("tool").and_then(Value::as_str).unwrap_or("tool").to_string();
                let pos = turn.api_tools.iter().position(|(n, _)| *n == name).or(if turn.api_tools.is_empty() { None } else { Some(0) });
                let tool_id = pos.and_then(|p| turn.api_tools.remove(p)).map(|(_, id)| id).unwrap_or_else(|| format!("{run_id}:t?"));
                let (text, err) = tool_output_text(event.get("preview").unwrap_or(&Value::Null));
                let is_error = err || name == "tool.failed" || event.get("error").and_then(Value::as_bool).unwrap_or(false);
                emits.extend(turn_tool_end(turn, &tool_id, Some(&name), text, is_error));
            }
            "approval.request" => {
                let (agent, key) = (turn.agent_id.clone(), turn.key.clone());
                let id = event.get("request_id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("{run_id}:approval"));
                if !st.approvals.contains_key(&id) {
                    let mut approval = build_approval(&id, &agent, &key, event);
                    approval.run_id = Some(run_id.to_string());
                    new_approval = Some((id, approval));
                }
            }
            "approval.responded" => {
                let id = event.get("request_id").and_then(Value::as_str).map(str::to_string);
                resolved_approval = id.or_else(|| st.approvals.iter().find(|(_, a)| a.run_id.as_deref() == Some(run_id)).map(|(id, _)| id.clone()));
            }
            n if n.starts_with("run.") && matches!(n, "run.completed" | "run.failed" | "run.cancelled" | "run.interrupted") => {
                let mut turn = st.turns.remove(run_id).expect("turn exists");
                let status = match n {
                    "run.completed" => "complete",
                    "run.failed" => "error",
                    _ => "interrupted",
                };
                let output = event.get("output").and_then(Value::as_str);
                emits.extend(turn_complete(&mut turn, output, status, event.get("error").and_then(Value::as_str)));
                let stale: Vec<String> = st.approvals.iter().filter(|(_, a)| a.run_id.as_deref() == Some(run_id)).map(|(id, _)| id.clone()).collect();
                for id in stale {
                    st.approvals.remove(&id);
                    emits.push(("exec.approval.resolved".into(), json!({ "id": id })));
                }
                done = true;
            }
            _ => {}
        }
        if let Some((id, approval)) = &new_approval {
            st.approvals.insert(id.clone(), approval.clone());
            emits.push(("exec.approval.requested".into(), approval.payload.clone()));
        }
        if let Some(id) = &resolved_approval {
            if st.approvals.remove(id).is_some() {
                emits.push(("exec.approval.resolved".into(), json!({ "id": id })));
            }
        }
    }
    flush(inner, emits);
    done
}

// ---- approvals ----------------------------------------------------------------------------

/// OpenClaw decision → Hermes choice, given what this prompt offers.
fn hermes_choice(decision: &str, choices: &[String]) -> String {
    let has = |c: &str| choices.is_empty() || choices.iter().any(|x| x == c);
    match decision {
        "allow-once" | "once" | "approve" | "allow" => "once".into(),
        "allow-always" | "always" => {
            if has("always") {
                "always".into()
            } else if has("session") {
                "session".into()
            } else {
                "once".into()
            }
        }
        "allow-session" | "session" => if has("session") { "session".into() } else { "once".into() },
        _ => "deny".into(),
    }
}

fn allowed_decisions(choices: &[String]) -> Vec<&'static str> {
    let mut out = vec!["allow-once"];
    if choices.is_empty() || choices.iter().any(|c| c == "always" || c == "session") {
        out.push("allow-always");
    }
    out.push("deny");
    out
}

fn build_approval(id: &str, agent_id: &str, key: &str, params: &Value) -> Approval {
    let choices: Vec<String> = params
        .get("choices")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let command = params.get("command").and_then(Value::as_str).unwrap_or("").to_string();
    let description = params.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()).map(str::to_string);
    let tool = params.get("tool_name").and_then(Value::as_str).unwrap_or("terminal");
    let mut presentation = json!({ "commandText": command, "allowedDecisions": allowed_decisions(&choices) });
    if let Some(d) = &description {
        presentation["warningText"] = json!(d);
    }
    let payload = json!({
        "id": id,
        "agentId": agent_id,
        "sessionKey": key,
        "createdAtMs": now_ms(),
        "request": { "id": id, "command": command, "agentId": agent_id, "sessionKey": key, "toolName": tool },
        "presentation": presentation,
    });
    Approval {
        payload,
        choices,
        srq_id: None,
        sid: None,
        request_id: params.get("request_id").and_then(Value::as_str).map(str::to_string),
        run_id: None,
    }
}

/// A server→client JSON-RPC request (local mode). Only `approval` is answered.
fn handle_server_request(inner: &Arc<Inner>, method: &str, id: Value, params: &Value) {
    if method != "approval" {
        send_frame(inner, json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("littleguys can't answer {method}") } }));
        return;
    }
    let srq_id = match &id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let sid = params.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
    let payload = {
        let mut st = inner.st.lock().unwrap();
        if st.approvals.values().any(|a| a.srq_id.as_deref() == Some(srq_id.as_str())) {
            return;
        }
        let (agent, key) = st.live.get(&sid).map(|l| (l.profile.clone(), l.key.clone())).unwrap_or_default();
        let approval_id = params.get("request_id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| srq_id.clone());
        let mut approval = build_approval(&approval_id, &agent, &key, params);
        approval.srq_id = Some(srq_id);
        approval.sid = Some(sid);
        let payload = approval.payload.clone();
        st.approvals.insert(approval_id, approval);
        payload
    };
    emit(inner, "exec.approval.requested", payload);
}

fn cancel_approval_by_srq(inner: &Inner, srq_id: &str) {
    let removed = {
        let mut st = inner.st.lock().unwrap();
        let id = st.approvals.iter().find(|(_, a)| a.srq_id.as_deref() == Some(srq_id)).map(|(id, _)| id.clone());
        id.inspect(|id| {
            st.approvals.remove(id);
        })
    };
    if let Some(id) = removed {
        emit(inner, "exec.approval.resolved", json!({ "id": id, "reason": "withdrawn" }));
    }
}

async fn approval_resolve(inner: &Arc<Inner>, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let id = params.get("id").and_then(Value::as_str).ok_or_else(|| RpcError::new("INVALID_REQUEST", "id is required"))?;
    let decision = params.get("decision").and_then(Value::as_str).unwrap_or("deny");
    let approval = inner.st.lock().unwrap().approvals.get(id).cloned().ok_or_else(|| RpcError::new("NOT_FOUND", "That request isn't waiting anymore."))?;
    let choice = hermes_choice(decision, &approval.choices);
    match inner.config.mode {
        HermesMode::Local => {
            let answered = approval
                .srq_id
                .as_ref()
                .is_some_and(|srq| send_frame(inner, json!({ "jsonrpc": "2.0", "id": srq, "result": { "choice": choice } })));
            if !answered {
                let sid = approval.sid.clone().ok_or_else(|| RpcError::new("NOT_CONNECTED", "not connected to Hermes"))?;
                let mut p = json!({ "session_id": sid, "choice": choice });
                if let Some(r) = &approval.request_id {
                    p["request_id"] = json!(r);
                }
                rpc(inner, "approval.respond", p, timeout).await?;
            }
        }
        HermesMode::Api => {
            let run = approval.run_id.clone().ok_or_else(|| RpcError::new("NOT_FOUND", "no run for that request"))?;
            let mut body = json!({ "choice": choice });
            if let Some(r) = &approval.request_id {
                body["request_id"] = json!(r);
            }
            rest(inner, reqwest::Method::POST, &format!("/v1/runs/{}/approval", enc(&run)), Some(body), timeout).await?;
        }
    }
    inner.st.lock().unwrap().approvals.remove(id);
    emit(inner, "exec.approval.resolved", json!({ "id": id, "decision": decision }));
    Ok(json!({ "ok": true }))
}

// ---- routines (Hermes cron) --------------------------------------------------------------

/// OpenClaw schedule → Hermes `schedule` string.
fn schedule_to_hermes(schedule: &Value) -> Result<String, RpcError> {
    let bad = |m: &str| RpcError::new("INVALID_REQUEST", m.to_string());
    match schedule.get("kind").and_then(Value::as_str) {
        Some("cron") => {
            let expr = schedule.get("expr").and_then(Value::as_str).map(str::trim).unwrap_or("");
            if expr.split_whitespace().count() < 5 {
                return Err(bad("Hermes needs a 5-field cron expression."));
            }
            Ok(expr.to_string())
        }
        Some("every") => {
            let ms = schedule.get("everyMs").and_then(Value::as_u64).ok_or_else(|| bad("everyMs is required"))?;
            let minutes = ms.div_ceil(60_000).max(1);
            Ok(if minutes % 1440 == 0 {
                format!("every {}d", minutes / 1440)
            } else if minutes % 60 == 0 {
                format!("every {}h", minutes / 60)
            } else {
                format!("every {minutes}m")
            })
        }
        Some("at") => {
            let at = schedule.get("at").and_then(Value::as_str).ok_or_else(|| bad("at is required"))?;
            chrono::DateTime::parse_from_rfc3339(at).map_err(|_| bad("invalid date for a one-time routine"))?;
            Ok(at.to_string())
        }
        Some(other) => Err(bad(&format!("Hermes can't run {other} schedules"))),
        None => Err(bad("schedule is required")),
    }
}

/// Hermes schedule (`{kind: cron|interval|once, …}`) → OpenClaw schedule.
fn schedule_from_hermes(schedule: &Value) -> Value {
    match schedule.get("kind").and_then(Value::as_str) {
        Some("cron") => json!({ "kind": "cron", "expr": schedule.get("expr").and_then(Value::as_str).unwrap_or("") }),
        Some("interval") => json!({ "kind": "every", "everyMs": schedule.get("minutes").and_then(Value::as_u64).unwrap_or(60) * 60_000 }),
        Some("once") => json!({ "kind": "at", "at": schedule.get("run_at").and_then(Value::as_str).unwrap_or("") }),
        Some(other) => json!({ "kind": other, "display": schedule.get("display") }),
        None => match schedule.as_str() {
            Some(s) => json!({ "kind": "cron", "expr": s }),
            None => json!({ "kind": "unknown" }),
        },
    }
}

fn iso_to_ms(v: Option<&Value>) -> Option<u64> {
    let s = v?.as_str()?;
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis().max(0) as u64);
    }
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    chrono::Local.from_local_datetime(&naive).single().map(|d| d.timestamp_millis().max(0) as u64)
}

/// Hermes job → OpenClaw `CronJob`.
fn cron_job_out(job: &Value, profile: &str) -> Value {
    let paused = job.get("state").and_then(Value::as_str) == Some("paused");
    let enabled = job.get("enabled").and_then(Value::as_bool).unwrap_or(true) && !paused;
    let last_status = job.get("last_status").and_then(Value::as_str).map(|s| if s == "ok" { "ok".to_string() } else { "error".to_string() });
    let next = if enabled { iso_to_ms(job.get("next_run_at")) } else { None };
    let last = iso_to_ms(job.get("last_run_at"));
    let error = job.get("last_error").or_else(|| job.get("last_delivery_error")).and_then(Value::as_str);
    json!({
        "id": job.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name": job.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Routine"),
        "agentId": profile,
        "enabled": enabled,
        "schedule": schedule_from_hermes(job.get("schedule").unwrap_or(&Value::Null)),
        "scheduleDisplay": job.get("schedule_display"),
        "payload": { "kind": "agentTurn", "message": job.get("prompt").and_then(Value::as_str).unwrap_or("") },
        "sessionTarget": "isolated",
        "wakeMode": "now",
        "createdAtMs": iso_to_ms(job.get("created_at")),
        "nextRunAtMs": next,
        "lastRunAtMs": last,
        "lastRunStatus": last_status,
        "lastRunError": error,
        "state": {
            "nextRunAtMs": next,
            "lastRunAtMs": last,
            "lastRunStatus": last_status,
            "lastStatus": last_status,
            "lastError": error,
            "hermesState": job.get("state"),
        },
    })
}

fn job_fingerprint(job: &Value) -> String {
    [
        "name", "enabled", "state", "next_run_at", "last_run_at", "last_status", "prompt",
    ]
    .iter()
    .map(|k| job.get(*k).map(Value::to_string).unwrap_or_default())
    .chain(std::iter::once(job.get("schedule").map(Value::to_string).unwrap_or_default()))
    .collect::<Vec<_>>()
    .join("|")
}

/// (profile, job) pairs from the backend.
async fn list_jobs(inner: &Inner, profile: Option<&str>, timeout: Duration) -> Result<Vec<(String, Value)>, RpcError> {
    match inner.config.mode {
        HermesMode::Local => {
            let scope = profile.map(str::to_string).or_else(|| inner.config.profile.clone().filter(|p| !p.trim().is_empty()));
            let path = format!("/api/cron/jobs?profile={}", enc(scope.as_deref().unwrap_or("all")));
            let v = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
            let jobs = v.as_array().cloned().or_else(|| v.get("jobs").and_then(Value::as_array).cloned()).unwrap_or_default();
            Ok(jobs
                .into_iter()
                .filter_map(|j| {
                    let p = j.get("profile").or_else(|| j.get("profile_name")).and_then(Value::as_str).map(str::to_string).or_else(|| scope.clone())?;
                    profile_allowed(inner, &p).then_some((p, j))
                })
                .collect())
        }
        HermesMode::Api => {
            let agent = api_agent_id(inner);
            let v = rest(inner, reqwest::Method::GET, "/api/jobs?include_disabled=true", None, timeout).await?;
            let jobs = v.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default();
            Ok(jobs.into_iter().map(|j| (agent.clone(), j)).collect())
        }
    }
}

fn unwrap_job(v: Value) -> Value {
    match v.get("job") {
        Some(job) if job.is_object() => job.clone(),
        _ => v,
    }
}

async fn job_profile(inner: &Inner, job_id: &str, timeout: Duration) -> Result<String, RpcError> {
    if let Some(p) = inner.st.lock().unwrap().job_profiles.get(job_id).cloned() {
        return Ok(p);
    }
    let jobs = list_jobs(inner, None, timeout).await?;
    let mut st = inner.st.lock().unwrap();
    for (p, j) in &jobs {
        if let Some(id) = j.get("id").and_then(Value::as_str) {
            st.job_profiles.insert(id.to_string(), p.clone());
        }
    }
    st.job_profiles.get(job_id).cloned().ok_or_else(|| RpcError::new("NOT_FOUND", "No such routine."))
}

fn job_path(inner: &Inner, job_id: &str, suffix: &str, profile: &str) -> String {
    match inner.config.mode {
        HermesMode::Local => format!("/api/cron/jobs/{}{suffix}?profile={}", enc(job_id), enc(profile)),
        HermesMode::Api => format!("/api/jobs/{}{suffix}", enc(job_id)),
    }
}

async fn cron_call(inner: &Arc<Inner>, method: &str, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let id = params.get("id").or_else(|| params.get("jobId")).and_then(Value::as_str).map(str::to_string);
    let need_id = || id.clone().ok_or_else(|| RpcError::new("INVALID_REQUEST", "id is required"));
    match method {
        "cron.status" => {
            let running = inner.st.lock().unwrap().gateway_running;
            Ok(json!({ "enabled": true, "gatewayRunning": running }))
        }
        "cron.list" => {
            let agent = params.get("agentId").and_then(Value::as_str);
            let jobs = list_jobs(inner, agent, timeout).await?;
            let include_disabled = params.get("includeDisabled").and_then(Value::as_bool).unwrap_or(true);
            let mut st = inner.st.lock().unwrap();
            let out: Vec<Value> = jobs
                .iter()
                .filter(|(p, _)| agent.is_none_or(|a| a == p))
                .map(|(p, j)| {
                    if let Some(id) = j.get("id").and_then(Value::as_str) {
                        st.job_profiles.insert(id.to_string(), p.clone());
                    }
                    cron_job_out(j, p)
                })
                .filter(|j| include_disabled || j["enabled"] == true)
                .collect();
            Ok(json!({ "jobs": out, "total": out.len(), "hasMore": false, "gatewayRunning": st.gateway_running }))
        }
        "cron.get" => {
            let id = need_id()?;
            let profile = job_profile(inner, &id, timeout).await?;
            let job = unwrap_job(rest(inner, reqwest::Method::GET, &job_path(inner, &id, "", &profile), None, timeout).await?);
            Ok(cron_job_out(&job, &profile))
        }
        "cron.add" => {
            let profile = params
                .get("agentId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| inner.st.lock().unwrap().profiles.first().cloned().unwrap_or_else(|| api_agent_id(inner)));
            if !profile_allowed(inner, &profile) {
                return Err(RpcError::new("INVALID_REQUEST", format!("unknown guy {profile}")));
            }
            let schedule = schedule_to_hermes(params.get("schedule").unwrap_or(&Value::Null))?;
            let prompt = params.pointer("/payload/message").or_else(|| params.pointer("/payload/text")).and_then(Value::as_str).unwrap_or("");
            let enabled = params.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let mut body = json!({
                "name": params.get("name").and_then(Value::as_str).unwrap_or("Routine"),
                "schedule": schedule,
                "prompt": prompt,
                "deliver": "local",
            });
            if !enabled {
                body["paused"] = json!(true);
                body["paused_reason"] = json!("Created paused in littleguys.");
            }
            let path = match inner.config.mode {
                HermesMode::Local => format!("/api/cron/jobs?profile={}", enc(&profile)),
                HermesMode::Api => "/api/jobs".to_string(),
            };
            let job = unwrap_job(rest(inner, reqwest::Method::POST, &path, Some(body), timeout).await?);
            if let Some(id) = job.get("id").and_then(Value::as_str) {
                inner.st.lock().unwrap().job_profiles.insert(id.to_string(), profile.clone());
            }
            let out = cron_job_out(&job, &profile);
            note_job(inner, &job);
            emit(inner, "cron", json!({ "action": "added", "jobId": out["id"], "job": out }));
            Ok(out)
        }
        "cron.update" => {
            let id = need_id()?;
            let profile = job_profile(inner, &id, timeout).await?;
            let patch = params.get("patch").cloned().unwrap_or(Value::Null);
            let mut updates = Map::new();
            if let Some(name) = patch.get("name").and_then(Value::as_str) {
                updates.insert("name".into(), json!(name));
            }
            if let Some(schedule) = patch.get("schedule") {
                updates.insert("schedule".into(), json!(schedule_to_hermes(schedule)?));
            }
            if let Some(msg) = patch.pointer("/payload/message").and_then(Value::as_str) {
                updates.insert("prompt".into(), json!(msg));
            }
            let mut job = Value::Null;
            if !updates.is_empty() {
                let (method, body) = match inner.config.mode {
                    HermesMode::Local => (reqwest::Method::PUT, json!({ "updates": updates })),
                    HermesMode::Api => (reqwest::Method::PATCH, Value::Object(updates)),
                };
                job = unwrap_job(rest(inner, method, &job_path(inner, &id, "", &profile), Some(body), timeout).await?);
            }
            if let Some(enabled) = patch.get("enabled").and_then(Value::as_bool) {
                let suffix = if enabled { "/resume" } else { "/pause" };
                job = unwrap_job(rest(inner, reqwest::Method::POST, &job_path(inner, &id, suffix, &profile), Some(json!({})), timeout).await?);
            }
            if job.is_null() {
                job = unwrap_job(rest(inner, reqwest::Method::GET, &job_path(inner, &id, "", &profile), None, timeout).await?);
            }
            let out = cron_job_out(&job, &profile);
            note_job(inner, &job);
            emit(inner, "cron", json!({ "action": "updated", "jobId": id, "job": out }));
            Ok(out)
        }
        "cron.remove" => {
            let id = need_id()?;
            let profile = job_profile(inner, &id, timeout).await?;
            rest(inner, reqwest::Method::DELETE, &job_path(inner, &id, "", &profile), None, timeout).await?;
            {
                let mut st = inner.st.lock().unwrap();
                st.job_profiles.remove(&id);
                st.cron.remove(&id);
            }
            emit(inner, "cron", json!({ "action": "removed", "jobId": id, "job": { "id": id } }));
            Ok(json!({ "ok": true, "removed": true }))
        }
        "cron.run" => {
            let id = need_id()?;
            let profile = job_profile(inner, &id, timeout).await?;
            let run_id = format!("manual:{id}:{}", now_ms());
            let before = unwrap_job(rest(inner, reqwest::Method::GET, &job_path(inner, &id, "", &profile), None, timeout).await?);
            // Hermes resumes a paused job it's asked to run; "run now" shouldn't switch it on.
            let was_paused = before.get("state").and_then(Value::as_str) == Some("paused") || before.get("enabled") == Some(&json!(false));
            let last_before = before.get("last_run_at").cloned().unwrap_or(Value::Null);
            emit(inner, "cron", json!({ "action": "started", "jobId": id, "job": cron_job_out(&before, &profile) }));
            let bg = inner.clone();
            let pause_path = job_path(inner, &id, "/pause", &profile);
            match inner.config.mode {
                HermesMode::Local => {
                    // The backend's trigger runs the job before answering: don't hold the caller.
                    let path = job_path(inner, &id, "/trigger", &profile);
                    spawn(async move {
                        let result = rest(&bg, reqwest::Method::POST, &path, Some(json!({})), Duration::from_secs(1800)).await;
                        if was_paused {
                            if let Err(e) = rest(&bg, reqwest::Method::POST, &pause_path, Some(json!({})), Duration::from_secs(15)).await {
                                log::warn!("hermes: couldn't re-pause routine {id}: {e}");
                            }
                        }
                        match result {
                            Ok(_) => {}
                            Err(e) if e.code == "CONFLICT" => log::info!("hermes: routine {id} is already running"),
                            Err(e) => {
                                log::warn!("hermes: running routine {id} failed: {e}");
                                emit(&bg, "cron", json!({
                                    "action": "finished", "jobId": id, "job": { "id": id, "agentId": profile },
                                    "status": "error", "summary": e.message,
                                }));
                            }
                        }
                        cron_check(&bg).await;
                    });
                }
                HermesMode::Api => {
                    // The API server marks the job due; the gateway's ticker fires it.
                    rest(inner, reqwest::Method::POST, &job_path(inner, &id, "/run", &profile), Some(json!({})), timeout).await?;
                    let get_path = job_path(inner, &id, "", &profile);
                    spawn(async move {
                        for _ in 0..180 {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            let Ok(job) = rest(&bg, reqwest::Method::GET, &get_path, None, Duration::from_secs(15)).await.map(unwrap_job) else {
                                continue;
                            };
                            if job.get("last_run_at").cloned().unwrap_or(Value::Null) != last_before {
                                if was_paused {
                                    let _ = rest(&bg, reqwest::Method::POST, &pause_path, Some(json!({})), Duration::from_secs(15)).await;
                                }
                                break;
                            }
                        }
                        cron_check(&bg).await;
                    });
                }
            }
            Ok(json!({ "ok": true, "enqueued": true, "runId": run_id }))
        }
        "cron.runs" => {
            let id = need_id()?;
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100);
            let profile = job_profile(inner, &id, timeout).await?;
            let runs: Vec<Value> = match inner.config.mode {
                HermesMode::Local => {
                    let v = rest(inner, reqwest::Method::GET, &format!("{}&limit={limit}", job_path(inner, &id, "/runs", &profile)), None, timeout).await?;
                    v.get("runs").and_then(Value::as_array).cloned().unwrap_or_default()
                }
                HermesMode::Api => {
                    let v = rest(inner, reqwest::Method::GET, "/api/sessions?source=cron&limit=200", None, timeout).await?;
                    let prefix = format!("cron_{id}_");
                    v.get("data")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| r.get("id").and_then(Value::as_str).is_some_and(|s| s.starts_with(&prefix)))
                        .take(limit as usize)
                        .collect()
                }
            };
            let mut entries: Vec<Value> = runs.iter().map(|r| cron_run_entry(&id, &profile, r, None)).collect();
            // Summaries: the run's last assistant message (newest few only).
            let summaries = futures_util::future::join_all(
                entries.iter().take(8).map(|e| run_summary(inner, &profile, e["runId"].as_str().unwrap_or_default().to_string())),
            )
            .await;
            for (entry, summary) in entries.iter_mut().zip(summaries) {
                if let Some(s) = summary {
                    entry["summary"] = json!(truncate(&s, 300));
                }
            }
            let total = entries.len();
            Ok(json!({ "entries": entries, "total": total, "hasMore": false }))
        }
        other => Err(unsupported(other)),
    }
}

/// A run session row → OpenClaw `CronRunLogEntry`.
fn cron_run_entry(job_id: &str, profile: &str, run: &Value, job_name: Option<&str>) -> Value {
    let session_id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let started = secs_to_ms(run.get("started_at")).unwrap_or(0);
    let ended = secs_to_ms(run.get("ended_at"));
    let end_reason = run.get("end_reason").and_then(Value::as_str);
    let status = match (ended, end_reason) {
        (None, _) => "running",
        (_, Some("cron_complete")) => "ok",
        _ => "error",
    };
    let mut entry = json!({
        "ts": ended.unwrap_or(started),
        "jobId": job_id,
        "runId": session_id,
        "status": status,
        "sessionId": session_id,
        "sessionKey": keys::routine_run(profile, job_id, session_id),
        "runAtMs": started,
        "durationMs": ended.map(|e| e.saturating_sub(started)),
    });
    if let Some(name) = job_name {
        entry["jobName"] = json!(name);
    }
    if status == "error" {
        entry["error"] = json!(end_reason.unwrap_or("failed"));
    }
    entry
}

async fn run_summary(inner: &Inner, profile: &str, session_id: String) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let path = match inner.config.mode {
        HermesMode::Local => format!("/api/sessions/{}/messages?profile={}&limit=6&order=latest", enc(&session_id), enc(profile)),
        HermesMode::Api => format!("/api/sessions/{}/messages?limit=6&order=latest", enc(&session_id)),
    };
    let v = rest(inner, reqwest::Method::GET, &path, None, Duration::from_secs(10)).await.ok()?;
    let rows = v.get("messages").or_else(|| v.get("data")).and_then(Value::as_array)?;
    rows.iter()
        .rev()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|m| text_of(m.get("content").unwrap_or(&Value::Null)))
        .find(|t| !t.trim().is_empty())
}

/// Whether Hermes held back delivery of the job's latest run (`suppressed`, `suppressed_acked`),
/// from `latest_execution.delivery_outcome`; `None` when Hermes doesn't report it.
fn delivery_suppressed(job: &Value) -> Option<bool> {
    let outcome = job.pointer("/latest_execution/delivery_outcome").and_then(Value::as_str)?;
    Some(outcome.starts_with("suppressed"))
}

/// Silence markers, as in Hermes' `gateway/response_filters.py` (`LIVE_GATEWAY_SILENT_MARKERS`).
const SILENCE_MARKERS: [&str; 8] = ["[SILENT]", "SILENT", "NO_REPLY", "NO REPLY", "[静默]", "静默", "[沉默]", "沉默"];

/// Whether a cron reply is Hermes' "nothing to report" sentinel, which it doesn't deliver.
/// Mirrors `is_autonomous_silence_response`: a reply that opens with a bracketed marker, or whose
/// whole text / first line / last line is a marker. A marker mid-sentence is real content.
fn is_silence_response(text: &str) -> bool {
    let text = text.trim();
    let Some(first) = text.lines().find(|l| !l.trim().is_empty()) else { return false };
    let last = text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or(first);
    let upper = text.to_uppercase();
    SILENCE_MARKERS.iter().any(|m| m.starts_with('[') && upper.starts_with(m)) || [text, first, last].into_iter().any(is_silence_marker)
}

/// Exactly a marker, ignoring case, inner whitespace and stray punctuation at the edges.
fn is_silence_marker(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 64 {
        return false;
    }
    // Square brackets stay, so a malformed `[SILENT` doesn't become `SILENT`.
    let edge = |c: char| c != '[' && c != ']' && (c.is_ascii_punctuation() && !"$+<=>^`|~".contains(c) || "。，、！？；：「」『』【】（）《》…—–‘’“”·".contains(c));
    let canonical = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
    [canonical(text), canonical(text.trim_matches(edge))].iter().any(|c| SILENCE_MARKERS.contains(&c.as_str()))
}

/// The newest run session of a job.
async fn latest_run(inner: &Inner, job_id: &str, profile: &str) -> Option<String> {
    let t = Duration::from_secs(10);
    match inner.config.mode {
        HermesMode::Local => {
            let v = rest(inner, reqwest::Method::GET, &format!("{}&limit=1", job_path(inner, job_id, "/runs", profile)), None, t).await.ok()?;
            v.pointer("/runs/0/id").and_then(Value::as_str).map(str::to_string)
        }
        HermesMode::Api => {
            let v = rest(inner, reqwest::Method::GET, "/api/sessions?source=cron&limit=50", None, t).await.ok()?;
            let prefix = format!("cron_{job_id}_");
            v.get("data")?.as_array()?.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).find(|id| id.starts_with(&prefix)).map(str::to_string)
        }
    }
}

fn note_job(inner: &Inner, job: &Value) {
    if let Some(id) = job.get("id").and_then(Value::as_str) {
        let mut st = inner.st.lock().unwrap();
        st.cron.insert(
            id.to_string(),
            CronSeen {
                last_run_at: job.get("last_run_at").and_then(Value::as_str).map(str::to_string),
                fingerprint: job_fingerprint(job),
            },
        );
    }
}

fn schedule_cron_check(inner: &Arc<Inner>) {
    {
        let mut st = inner.st.lock().unwrap();
        if st.cron_check_scheduled {
            return;
        }
        st.cron_check_scheduled = true;
    }
    let inner = inner.clone();
    spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        inner.st.lock().unwrap().cron_check_scheduled = false;
        cron_check(&inner).await;
    });
}

/// Diff jobs against the last snapshot: routines that finished since → `cron {action:"finished"}`;
/// any other change → `cron {action:"updated"}` (the Routines panel reloads).
async fn cron_check(inner: &Arc<Inner>) {
    let Ok(jobs) = list_jobs(inner, None, Duration::from_secs(15)).await else { return };
    let (finished, changed) = {
        let mut st = inner.st.lock().unwrap();
        let primed = st.cron_primed;
        let mut finished: Vec<(String, Value)> = Vec::new();
        let mut changed = st.cron.len() != jobs.len();
        let mut next: HashMap<String, CronSeen> = HashMap::new();
        for (profile, job) in &jobs {
            let Some(id) = job.get("id").and_then(Value::as_str) else { continue };
            let last = job.get("last_run_at").and_then(Value::as_str).map(str::to_string);
            let fp = job_fingerprint(job);
            match st.cron.get(id) {
                Some(seen) => {
                    if seen.fingerprint != fp {
                        changed = true;
                    }
                    if primed && last.is_some() && seen.last_run_at != last {
                        finished.push((profile.clone(), job.clone()));
                    }
                }
                None => changed = true,
            }
            st.job_profiles.insert(id.to_string(), profile.clone());
            next.insert(id.to_string(), CronSeen { last_run_at: last, fingerprint: fp });
        }
        st.cron = next;
        st.cron_primed = true;
        (finished, changed && primed)
    };
    for (profile, job) in finished {
        let id = job.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let out = cron_job_out(&job, &profile);
        let ok = job.get("last_status").and_then(Value::as_str) == Some("ok");
        // The newest run session: where to read the reply.
        let mut session_key = Value::Null;
        let mut summary = job.get("last_error").and_then(Value::as_str).filter(|_| !ok).map(str::to_string);
        // Hermes didn't deliver this run (a [SILENT] reply, a script with nothing to say, an acked
        // failure…), so neither should we. Hermes reports that as the execution's delivery outcome;
        // without one, fall back to spotting the [SILENT] reply ourselves.
        let suppressed = delivery_suppressed(&job);
        let mut silent = suppressed == Some(true);
        if let Some(run_id) = latest_run(inner, &id, &profile).await {
            session_key = json!(keys::routine_run(&profile, &id, &run_id));
            if summary.is_none() {
                let reply = run_summary(inner, &profile, run_id).await;
                silent |= suppressed.is_none() && ok && reply.as_deref().is_some_and(is_silence_response);
                summary = reply.map(|t| truncate(&t, 300));
            }
        }
        emit(inner, "cron", json!({
            "action": "finished",
            "jobId": id,
            "job": out,
            "status": if ok { "ok" } else { "error" },
            "summary": summary,
            "silent": silent,
            "sessionKey": session_key,
        }));
    }
    if changed {
        emit(inner, "cron", json!({ "action": "updated" }));
    }
}

// ---- workspace, skills, tools, plugins, MCP (read-only) ------------------------------
//
// Local mode reads the dashboard REST API (`/api/fs/*`, `/api/skills`, `/api/tools/toolsets`,
// `/api/mcp/servers`, all `?profile=`-scoped) plus two WS methods; api mode only has
// `/v1/skills` and `/v1/toolsets`. Nothing here writes.

/// How long a guy's workspace root (`/api/fs/default-cwd`) is trusted.
const WORKSPACE_ROOT_TTL_MS: u64 = 60_000;
/// `agents.workspace.get` caps (Hermes itself previews ≤ 512 KB of text, data URLs ≤ 16 MB).
const PREVIEW_TEXT_MAX: usize = 256 * 1024;
const PREVIEW_IMAGE_MAX: u64 = 5 * 1024 * 1024;
/// Binary files littleguys previews (extension → mime).
const PREVIEW_IMAGES: &[(&str, &str)] = &[
    ("png", "image/png"), ("jpg", "image/jpeg"), ("jpeg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp"),
    ("bmp", "image/bmp"), ("avif", "image/avif"), ("heic", "image/heic"),
];

/// The Hermes profile behind `agentId` (agent id = profile name); `None` = Hermes' default.
fn agent_profile(inner: &Inner, params: &Value) -> Result<Option<String>, RpcError> {
    match params.get("agentId").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()) {
        Some(agent) if profile_allowed(inner, agent) => Ok(Some(agent.to_string())),
        Some(agent) => Err(RpcError::new("INVALID_REQUEST", format!("unknown guy {agent}"))),
        None => Ok(inner.config.profile.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string)),
    }
}

/// `path?k=v&…` with percent-encoded values; `None` values are left out.
fn with_query(path: &str, query: &[(&str, Option<&str>)]) -> String {
    let parts: Vec<String> = query.iter().filter_map(|(k, v)| v.map(|v| format!("{k}={}", enc(v)))).collect();
    if parts.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", parts.join("&"))
    }
}

/// A non-empty, trimmed string field.
fn nonempty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Rows of a bare array, `{<key>: […]}` or an OpenAI-style `{data: […]}`.
fn list_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.as_array()
        .or_else(|| v.get(key).and_then(Value::as_array))
        .or_else(|| v.get("data").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The separator a backend path uses (`\` only for Windows-looking paths).
fn dir_sep(path: &str) -> char {
    if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    }
}

/// A directory without trailing separators; filesystem roots (`/`, `C:\`) keep theirs.
fn trim_dir(dir: &str) -> String {
    let dir = dir.trim();
    let trimmed = dir.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        match dir[trimmed.len()..].chars().next() {
            Some(sep) => format!("{trimmed}{sep}"),
            None => dir.to_string(),
        }
    } else {
        trimmed.to_string()
    }
}

/// UI path ("" = root) → normalized workspace-relative path. Absolute paths and `..` are refused.
fn workspace_rel(path: &str) -> Result<String, RpcError> {
    let path = path.trim();
    let outside = || RpcError::new("INVALID_REQUEST", format!("{path:?} isn't a path inside the workspace"));
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.starts_with(['/', '\\']) || drive || path.contains('\0') {
        return Err(outside());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            // `\` is a separator on Windows backends: no `..` hiding behind it either.
            p if p.split('\\').any(|s| s == "..") => return Err(outside()),
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

/// Workspace-relative path → the backend's absolute path.
fn workspace_abs(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    let sep = dir_sep(root);
    let rel = if sep == '/' { rel.to_string() } else { rel.replace('/', "\\") };
    if root.ends_with(sep) {
        format!("{root}{rel}")
    } else {
        format!("{root}{sep}{rel}")
    }
}

/// A backend absolute path → workspace-relative (`/`-separated), or `None` when it isn't
/// inside the root (e.g. a symlink Hermes resolved to somewhere else).
fn workspace_relative(root: &str, abs: &str) -> Option<String> {
    let root = trim_dir(root);
    let abs = trim_dir(abs);
    if abs == root {
        return Some(String::new());
    }
    let sep = dir_sep(&root);
    let rest = abs.strip_prefix(root.as_str())?;
    let rest = if root.ends_with(sep) { rest } else { rest.strip_prefix(sep)? };
    let rel = if sep == '/' { rest.to_string() } else { rest.replace('\\', "/") };
    let clean = !rel.is_empty() && rel.split('/').all(|p| !p.is_empty() && p != "." && p != "..");
    clean.then_some(rel)
}

fn parent_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("")
}

/// The guy's workspace: Hermes' default working directory for the profile (cached a minute).
async fn workspace_root(inner: &Inner, profile: Option<&str>, timeout: Duration) -> Result<String, RpcError> {
    let key = profile.unwrap_or_default().to_string();
    if let Some((root, at)) = inner.st.lock().unwrap().workspace_roots.get(&key).cloned() {
        if now_ms().saturating_sub(at) < WORKSPACE_ROOT_TTL_MS {
            return Ok(root);
        }
    }
    let path = with_query("/api/fs/default-cwd", &[("profile", profile)]);
    let v = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
    let root = nonempty(&v, "cwd")
        .map(trim_dir)
        .ok_or_else(|| RpcError::new("UNAVAILABLE", "Hermes didn't say which folder this guy works in."))?;
    inner.st.lock().unwrap().workspace_roots.insert(key, (root.clone(), now_ms()));
    Ok(root)
}

/// `/api/fs/list` answer (`{entries:[{name, path, isDirectory}], error?}`) → OpenClaw's
/// workspace listing: dirs first, by name, paged; entries outside the root are dropped.
fn workspace_listing(agent_id: &str, root: &str, rel: &str, raw: &Value, offset: usize, limit: usize) -> Result<Value, RpcError> {
    if let Some(err) = nonempty(raw, "error") {
        let shown = if rel.is_empty() { "The workspace" } else { rel };
        return Err(match err {
            "ENOENT" => RpcError::new("NOT_FOUND", format!("{shown} doesn't exist.")),
            "ENOTDIR" => RpcError::new("INVALID_REQUEST", format!("{shown} isn't a folder.")),
            "EACCES" => RpcError::new("INVALID_REQUEST", format!("Hermes isn't allowed to read {shown}.")),
            other => RpcError::new("UNAVAILABLE", format!("Hermes couldn't list {shown}: {other}")),
        });
    }
    let mut entries: Vec<(bool, &str, String)> = list_of(raw, "entries")
        .iter()
        .filter_map(|e| {
            let name = nonempty(e, "name")?;
            let path = workspace_relative(root, e.get("path").and_then(Value::as_str)?).filter(|p| !p.is_empty())?;
            Some((e.get("isDirectory").and_then(Value::as_bool).unwrap_or(false), name, path))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())).then_with(|| a.1.cmp(b.1)));
    let total = entries.len();
    let page: Vec<Value> = entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(dir, name, path)| json!({ "path": path, "name": name, "kind": if dir { "directory" } else { "file" } }))
        .collect();
    let mut out = json!({ "agentId": agent_id, "path": rel, "entries": page, "totalEntries": total, "offset": offset });
    if !rel.is_empty() {
        out["parentPath"] = json!(parent_of(rel));
    }
    Ok(out)
}

async fn workspace_list(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let agent_id = profile.clone().unwrap_or_else(|| api_agent_id(inner));
    let rel = workspace_rel(params.get("path").and_then(Value::as_str).unwrap_or(""))?;
    let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(250).clamp(1, 500) as usize;
    let root = workspace_root(inner, profile.as_deref(), timeout).await?;
    let abs = workspace_abs(&root, &rel);
    let path = with_query("/api/fs/list", &[("path", Some(abs.as_str())), ("profile", profile.as_deref())]);
    let raw = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
    workspace_listing(&agent_id, &root, &rel, &raw, offset, limit)
}

#[derive(Debug, PartialEq)]
enum Preview {
    Text(Value),
    /// A binary image to fetch as a data URL.
    Image { mime: &'static str, size: u64 },
}

fn image_mime(name: &str, mime: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    PREVIEW_IMAGES.iter().find(|(e, m)| ext.as_deref() == Some(*e) || *m == mime).map(|(_, m)| *m)
}

/// `/api/fs/read-text` answer (`{text, binary, byteSize, mimeType, path, truncated}`) → the
/// file, or the image to fetch instead. Other binaries (and symlinks out of the root) can't be shown.
fn file_preview(root: &str, rel: &str, r: &Value) -> Result<Preview, RpcError> {
    if let Some(resolved) = nonempty(r, "path") {
        if workspace_relative(root, resolved).is_none() {
            return Err(RpcError::new("INVALID_REQUEST", format!("{rel} points outside the workspace.")));
        }
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let text = r.get("text").and_then(Value::as_str).unwrap_or("");
    let size = r.get("byteSize").and_then(Value::as_u64).unwrap_or(text.len() as u64);
    let mime = nonempty(r, "mimeType").unwrap_or("application/octet-stream");
    if r.get("binary").and_then(Value::as_bool).unwrap_or(false) {
        let Some(mime) = image_mime(name, mime) else {
            return Err(RpcError::new("INVALID_REQUEST", format!("{name} is a binary file; littleguys can't preview it.")));
        };
        if size > PREVIEW_IMAGE_MAX {
            let mb = size as f64 / (1024.0 * 1024.0);
            return Err(RpcError::new("INVALID_REQUEST", format!("{name} is too big to preview ({mb:.1} MB; images up to 5 MB).")));
        }
        return Ok(Preview::Image { mime, size });
    }
    let mut end = text.len().min(PREVIEW_TEXT_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mime = if mime == "application/octet-stream" { "text/plain" } else { mime };
    let mut file = json!({ "path": rel, "name": name, "size": size, "mimeType": mime, "encoding": "utf8", "content": &text[..end] });
    if end < text.len() || r.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        file["truncated"] = json!(true);
    }
    Ok(Preview::Text(file))
}

/// `data:<mime>;base64,<payload>` → payload.
fn data_url_base64(url: &str) -> Option<&str> {
    url.strip_prefix("data:")?.split_once(";base64,").map(|(_, b64)| b64)
}

async fn workspace_get(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let agent_id = profile.clone().unwrap_or_else(|| api_agent_id(inner));
    let rel = workspace_rel(params.get("path").and_then(Value::as_str).unwrap_or(""))?;
    if rel.is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "path is required"));
    }
    let root = workspace_root(inner, profile.as_deref(), timeout).await?;
    let abs = workspace_abs(&root, &rel);
    let query = [("path", Some(abs.as_str())), ("profile", profile.as_deref())];
    let text = rest(inner, reqwest::Method::GET, &with_query("/api/fs/read-text", &query), None, timeout).await?;
    let file = match file_preview(&root, &rel, &text)? {
        Preview::Text(file) => file,
        Preview::Image { mime, size } => {
            let v = rest(inner, reqwest::Method::GET, &with_query("/api/fs/read-data-url", &query), None, timeout).await?;
            let content = v
                .get("dataUrl")
                .and_then(Value::as_str)
                .and_then(data_url_base64)
                .ok_or_else(|| RpcError::new("UNAVAILABLE", "Hermes sent an unreadable image."))?;
            let name = rel.rsplit('/').next().unwrap_or(&rel);
            json!({ "path": rel, "name": name, "size": size, "mimeType": mime, "encoding": "base64", "content": content })
        }
    };
    Ok(json!({ "agentId": agent_id, "file": file }))
}

/// One skill row → OpenClaw's skill entry.
fn skill_entry(s: &Value, source: &str, enabled: bool) -> Option<Value> {
    let name = nonempty(s, "name")?;
    let mut out = json!({ "name": name, "skillKey": name, "source": source, "eligible": enabled, "disabled": !enabled });
    if let Some(d) = nonempty(s, "description") {
        out["description"] = json!(d);
    }
    if let Some(c) = nonempty(s, "category") {
        out["category"] = json!(c);
    }
    if let Some(n) = s.get("usage").and_then(Value::as_u64) {
        out["usage"] = json!(n);
    }
    Some(out)
}

/// Local `/api/skills` (`[{name, description, category, enabled, usage, provenance}]`).
fn skills_from_local(v: &Value) -> Vec<Value> {
    list_of(v, "skills")
        .iter()
        .filter_map(|s| {
            let source = nonempty(s, "provenance").or_else(|| nonempty(s, "category")).unwrap_or("hermes");
            skill_entry(s, source, s.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        })
        .collect()
}

/// Api `/v1/skills` (`{data:[{name, description, category}]}`): disabled skills aren't listed.
fn skills_from_api(v: &Value) -> Vec<Value> {
    list_of(v, "skills").iter().filter_map(|s| skill_entry(s, nonempty(s, "category").unwrap_or("hermes"), true)).collect()
}

async fn skills_status(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let agent_id = profile.clone().unwrap_or_else(|| api_agent_id(inner));
    let skills = match inner.config.mode {
        HermesMode::Local => {
            let path = with_query("/api/skills", &[("profile", profile.as_deref())]);
            skills_from_local(&rest(inner, reqwest::Method::GET, &path, None, timeout).await?)
        }
        HermesMode::Api => skills_from_api(&rest(inner, reqwest::Method::GET, "/v1/skills", None, timeout).await?),
    };
    Ok(json!({ "agentId": agent_id, "skills": skills }))
}

async fn skills_read(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let name = nonempty(params, "name").ok_or_else(|| RpcError::new("INVALID_REQUEST", "name is required"))?;
    let path = with_query("/api/skills/content", &[("name", Some(name)), ("profile", profile.as_deref())]);
    let v = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
    let mut out = json!({ "name": nonempty(&v, "name").unwrap_or(name), "content": v.get("content").and_then(Value::as_str).unwrap_or("") });
    if let Some(p) = nonempty(&v, "path") {
        out["path"] = json!(p);
    }
    Ok(out)
}

/// Slash commands from `commands.catalog` (what Hermes' desktop reads), scoped to the guy's
/// profile and, when the thread is live, its session (project-local skills).
async fn commands_list(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let mut p = json!({});
    if let Some(profile) = &profile {
        p["profile"] = json!(profile);
    }
    if let Some(key) = params.get("sessionKey").and_then(Value::as_str) {
        if let Ok((profile, Some(stored))) = resolve_key(inner, key) {
            if let Some(sid) = inner.st.lock().unwrap().by_session.get(&(profile, stored)) {
                p["session_id"] = json!(sid);
            }
        }
    }
    let catalog = rpc(inner, "commands.catalog", p, timeout).await?;
    Ok(json!({ "commands": catalog_commands(&catalog) }))
}

/// `commands.catalog` (`pairs:[[/key, desc]]`, `categories:[{name, pairs}]`, `commands:{/key:
/// {argument_mode, desktop, desktop_subcommands?}}`, `skills:{/key:…}`, `sub:{/key:[…]}`) →
/// OpenClaw `CommandEntry`s. Leaves out what Hermes' desktop leaves out (`desktop` set:
/// terminal-only or hidden) and the TUI's own display commands.
fn catalog_commands(catalog: &Value) -> Vec<Value> {
    let mut category: HashMap<&str, &str> = HashMap::new();
    for c in list_of(catalog, "categories") {
        let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
        for pair in list_of(c, "pairs") {
            if let Some(key) = pair.get(0).and_then(Value::as_str) {
                category.insert(key, name);
            }
        }
    }
    let skills = catalog.get("skills").and_then(Value::as_object);
    let mut seen = HashSet::new();
    list_of(catalog, "pairs")
        .iter()
        .filter_map(|pair| {
            let key = pair.get(0)?.as_str()?.trim();
            let name = key.trim_start_matches('/');
            let cat = category.get(key).copied().unwrap_or_default();
            if name.is_empty() || cat == "TUI" || !seen.insert(name.to_string()) {
                return None;
            }
            let meta = catalog.pointer("/commands").and_then(|c| c.get(key));
            if meta.and_then(|m| m.get("desktop")).and_then(Value::as_str).is_some_and(|d| !d.is_empty()) {
                return None;
            }
            let skill = skills.is_some_and(|s| s.contains_key(key));
            let source = if skill { "skill" } else if cat == "Plugin commands" { "plugin" } else { "native" };
            let mut entry = json!({
                "name": name,
                "description": pair.get(1).and_then(Value::as_str).unwrap_or_default(),
                "source": source,
                "scope": "text",
            });
            if skill {
                entry["acceptsArgs"] = json!(true);
            } else if let Some(meta) = meta {
                entry["acceptsArgs"] = json!(!meta.get("argument_mode").is_none_or(Value::is_null));
                let subs = meta.get("desktop_subcommands").or_else(|| catalog.pointer("/sub").and_then(|s| s.get(key)));
                let choices: Vec<Value> =
                    subs.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|v| json!({ "value": v, "label": v })).collect();
                if !choices.is_empty() {
                    entry["args"] = json!([{ "name": "option", "description": "", "type": "string", "choices": choices }]);
                }
            }
            Some(entry)
        })
        .collect()
}

/// Hermes toolsets (`/api/tools/toolsets` or `/v1/toolsets`: `{name, label, description,
/// enabled, configured, tools:[names]}`) → OpenClaw tool groups, disabled ones included.
fn tool_groups(v: &Value) -> Vec<Value> {
    list_of(v, "toolsets")
        .iter()
        .filter_map(|t| {
            let name = nonempty(t, "name")?;
            let tools: Vec<Value> = t
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|id| json!({ "id": id, "label": id }))
                .collect();
            let mut group = json!({
                "id": name,
                "label": nonempty(t, "label").unwrap_or(name),
                "source": "toolset",
                "enabled": t.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                "tools": tools,
            });
            if let Some(d) = nonempty(t, "description") {
                group["description"] = json!(d);
            }
            if let Some(c) = t.get("configured").and_then(Value::as_bool) {
                group["configured"] = json!(c);
            }
            Some(group)
        })
        .collect()
}

async fn tools_groups(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let agent_id = profile.clone().unwrap_or_else(|| api_agent_id(inner));
    let path = match inner.config.mode {
        HermesMode::Local => with_query("/api/tools/toolsets", &[("profile", profile.as_deref())]),
        HermesMode::Api => "/v1/toolsets".to_string(),
    };
    let raw = rest(inner, reqwest::Method::GET, &path, None, timeout).await?;
    Ok(json!({ "agentId": agent_id, "groups": tool_groups(&raw) }))
}

/// `plugins.manage list` rows (`{name, key, version, description, status}`) or legacy
/// `plugins.list` rows (`{name, version, enabled}`) → OpenClaw plugin entries.
fn plugins_out(v: &Value) -> Vec<Value> {
    list_of(v, "plugins")
        .iter()
        .filter_map(|p| {
            let name = nonempty(p, "name")?;
            let enabled = match p.get("enabled").and_then(Value::as_bool) {
                Some(enabled) => enabled,
                None => nonempty(p, "status") == Some("enabled"),
            };
            let mut out = json!({
                "id": nonempty(p, "key").unwrap_or(name),
                "name": name,
                "enabled": enabled,
                "state": if enabled { "enabled" } else { "disabled" },
            });
            if let Some(d) = nonempty(p, "description") {
                out["description"] = json!(d);
            }
            if let Some(version) = nonempty(p, "version").filter(|v| *v != "?") {
                out["version"] = json!(version);
            }
            Some(out)
        })
        .collect()
}

async fn plugins_list(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let mut list = json!({ "action": "list" });
    if let Some(profile) = &profile {
        list["profile"] = json!(profile);
    }
    // The Plugins Hub inventory is per profile and describes each plugin; older backends only
    // have the process-wide `plugins.list` (which takes no params).
    let raw = match rpc(inner, "plugins.manage", list, timeout).await {
        Ok(v) => v,
        Err(e) if matches!(e.code.as_str(), "NOT_CONNECTED" | "DISCONNECTED") => return Err(e),
        Err(_) => rpc(inner, "plugins.list", json!({}), timeout).await?,
    };
    Ok(json!({ "plugins": plugins_out(&raw) }))
}

/// A server URL without userinfo, query, fragment or token-looking path segments.
fn safe_url(url: &str) -> Option<String> {
    let url = url.trim().split(['?', '#']).next().unwrap_or("");
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), url),
    };
    let (authority, path) = rest.find('/').map(|i| rest.split_at(i)).unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority);
    if host.is_empty() {
        return None;
    }
    let looks_secret =
        |seg: &str| seg.len() >= 16 && seg.chars().any(|c| c.is_ascii_digit()) && seg.chars().any(|c| c.is_ascii_alphabetic());
    let path: Vec<&str> = path.split('/').map(|seg| if looks_secret(seg) { "…" } else { seg }).collect();
    Some(format!("{scheme}{host}{}", path.join("/")))
}

/// The program a stdio server runs: no directory, no arguments.
fn program_name(command: &str) -> Option<String> {
    let base = command.trim().rsplit(['/', '\\']).next()?;
    base.split_whitespace().next().filter(|p| !p.contains('=')).map(str::to_string)
}

/// Configured MCP servers (`/api/mcp/servers`) + cached runtime state (`mcp.servers.status`:
/// `{name, transport, tools:int, connected, disabled, status}`). Only names, transports, a
/// credential-free URL and the program name: never args, env (not even key names), headers or auth.
fn mcp_servers_out(config: &Value, status: Option<&Value>) -> Vec<Value> {
    let runtime: Vec<(&str, &Value)> =
        status.map(|s| list_of(s, "servers")).unwrap_or(&[]).iter().filter_map(|r| Some((nonempty(r, "name")?, r))).collect();
    let configured = list_of(config, "servers");
    let mut out = Vec::new();
    for s in configured.iter().chain(runtime.iter().map(|(_, r)| *r)) {
        let Some(name) = nonempty(s, "name") else { continue };
        if out.iter().any(|row: &Value| row["name"] == name) {
            continue;
        }
        let rt = runtime.iter().find(|(n, _)| *n == name).map(|(_, r)| *r);
        let transport = nonempty(s, "transport").or_else(|| rt.and_then(|r| nonempty(r, "transport"))).unwrap_or("unknown");
        let mut row = json!({ "name": name, "transport": transport });
        if let Some(url) = s.get("url").and_then(Value::as_str).and_then(safe_url) {
            row["url"] = json!(url);
        }
        if let Some(program) = s.get("command").and_then(Value::as_str).and_then(program_name) {
            row["command"] = json!(program);
        }
        let disabled = rt.and_then(|r| r.get("disabled")).and_then(Value::as_bool);
        row["enabled"] = json!(s.get("enabled").and_then(Value::as_bool).or(disabled.map(|d| !d)).unwrap_or(true));
        let connected = rt.and_then(|r| r.get("connected")).and_then(Value::as_bool);
        let count = rt.and_then(|r| r.get("tools")).and_then(Value::as_u64).filter(|n| *n > 0 || connected == Some(true));
        // Config `tools` is the selection: a list of names, or `{include:[…]}`; absent = all.
        let selected = s.get("tools").and_then(|t| t.as_array().or_else(|| t.get("include").and_then(Value::as_array)));
        if let Some(n) = count {
            row["tools"] = json!(n);
        } else if let Some(names) = selected {
            row["tools"] = json!(names.iter().filter_map(Value::as_str).collect::<Vec<_>>());
        }
        if let Some(status) = rt.and_then(|r| nonempty(r, "status")) {
            row["status"] = json!(status);
        }
        if let Some(connected) = connected {
            row["connected"] = json!(connected);
        }
        if let Some(source) = nonempty(s, "source") {
            row["source"] = json!(source);
        }
        out.push(row);
    }
    out
}

async fn mcp_servers_list(inner: &Inner, params: &Value, timeout: Duration) -> Result<Value, RpcError> {
    let profile = agent_profile(inner, params)?;
    let path = with_query("/api/mcp/servers", &[("profile", profile.as_deref())]);
    let mut status_params = json!({});
    if let Some(profile) = &profile {
        status_params["profile"] = json!(profile);
    }
    // Runtime state is a nice-to-have: it's cached server-side and never probes.
    let (config, status) = tokio::join!(
        rest(inner, reqwest::Method::GET, &path, None, timeout),
        rpc(inner, "mcp.servers.status", status_params, timeout.min(Duration::from_secs(10))),
    );
    Ok(json!({ "servers": mcp_servers_out(&config?, status.ok().as_ref()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hermes_addresses() {
        assert_eq!(normalize_http("127.0.0.1", 9119).unwrap(), "http://127.0.0.1:9119");
        assert_eq!(normalize_http("127.0.0.1:50108", 9119).unwrap(), "http://127.0.0.1:50108");
        assert_eq!(normalize_http("http://box.local:8642/", 8642).unwrap(), "http://box.local:8642");
        assert_eq!(normalize_http("ws://127.0.0.1:9119/api/ws", 9119).unwrap(), "http://127.0.0.1:9119");
        assert_eq!(normalize_http("attic.tail1234.ts.net", 8642).unwrap(), "https://attic.tail1234.ts.net");
        assert_eq!(normalize_http("https://h.example.com/p/fox", 8642).unwrap(), "https://h.example.com/p/fox");
        assert!(normalize_http("ftp://x", 1).is_err());
        assert!(normalize_http("  ", 1).is_err());
        assert!(is_loopback_url("http://127.0.0.1:9119"));
        assert!(!is_loopback_url("http://box.local:9119"));
    }

    #[test]
    fn spots_silent_cron_replies() {
        // Expected values from Hermes' own is_autonomous_silence_response on the same inputs.
        let cases = [
            ("[SILENT]", true), ("  [silent]  ", true), ("[SILENT] No changes detected", true), ("SILENT", true),
            ("NO_REPLY", true), ("no reply", true), (".NO_REPLY", true), ("*NO_REPLY*", true), ("[SILENT", false),
            ("Checked 12 feeds.\n[SILENT]", true), ("[SILENT]\nnothing new", true), ("Nothing new, so [SILENT] it is.", false),
            ("Silent retry succeeded", false), ("", false), ("   ", false), ("【静默】", true), ("静默。", true),
            ("**[SILENT]**", true), ("`[SILENT]`", false), ("(NO REPLY)", true), ("$SILENT", false), ("Alert: disk 91%", false),
            ("no   reply", true), ("Report:\n- a\n- b\nSILENT.", true), ("NO_REPLY please ignore", false),
        ];
        for (text, silent) in cases {
            assert_eq!(is_silence_response(text), silent, "{text:?}");
        }
    }

    #[test]
    fn reads_hermes_delivery_outcome() {
        let job = |outcome: Value| json!({ "id": "c7664bd199c2", "no_agent": true, "latest_execution": { "status": "completed", "delivery_outcome": outcome } });
        assert_eq!(delivery_suppressed(&job(json!("suppressed"))), Some(true));
        assert_eq!(delivery_suppressed(&job(json!("suppressed_acked"))), Some(true));
        assert_eq!(delivery_suppressed(&job(json!("delivered"))), Some(false));
        assert_eq!(delivery_suppressed(&job(json!("failed"))), Some(false));
        assert_eq!(delivery_suppressed(&job(Value::Null)), None);
        assert_eq!(delivery_suppressed(&json!({ "id": "x", "latest_execution": {} })), None);
        assert_eq!(delivery_suppressed(&json!({ "id": "x" })), None);
    }

    #[test]
    fn spots_slash_commands() {
        assert_eq!(slash_command("/model gpt-5"), Some("/model gpt-5"));
        assert_eq!(slash_command("  /status "), Some("/status"));
        assert_eq!(slash_command("/skills:pdf do it"), Some("/skills:pdf do it"));
        assert_eq!(slash_command("/Users/me/notes.md has the list"), None);
        assert_eq!(slash_command("/ hi"), None);
        assert_eq!(slash_command("hi /status"), None);
        assert_eq!(slash_command("//x"), None);
    }

    #[test]
    fn catalog_becomes_command_entries() {
        let catalog = json!({
            "pairs": [["/model", "Switch model (usage: /model [name])"], ["/quit", "Exit"], ["/help", "Show help"],
                      ["/density", "Toggle compact display mode"], ["/standup", "exec: ./standup.sh"],
                      ["/think", "Reasoning effort"], ["/pdf", "Work with PDFs"], ["/model", "dupe"]],
            "categories": [{ "name": "Session", "pairs": [["/model", ""], ["/quit", ""], ["/help", ""], ["/think", ""]] },
                           { "name": "TUI", "pairs": [["/density", ""]] },
                           { "name": "User commands", "pairs": [["/standup", ""]] }],
            "commands": { "/model": { "argument_mode": "text", "desktop": null }, "/quit": { "argument_mode": null, "desktop": "terminal" },
                          "/help": { "argument_mode": null, "desktop": null },
                          "/think": { "argument_mode": "options", "desktop": null, "desktop_subcommands": ["low", "high"] } },
            "sub": { "/think": ["low", "medium", "high"] },
            "skills": { "/pdf": { "usage": 3, "origin": "hub" } },
        });
        let entries = catalog_commands(&catalog);
        let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["model", "help", "standup", "think", "pdf"]);
        assert_eq!(entries[0], json!({ "name": "model", "description": "Switch model (usage: /model [name])", "source": "native", "scope": "text", "acceptsArgs": true }));
        assert_eq!(entries[1]["acceptsArgs"], false);
        assert!(entries[2].get("acceptsArgs").is_none());
        assert_eq!(entries[3]["args"][0]["choices"], json!([{ "value": "low", "label": "low" }, { "value": "high", "label": "high" }]));
        assert_eq!(entries[4]["source"], "skill");
        assert_eq!(entries[4]["acceptsArgs"], true);
    }

    #[test]
    fn scrapes_the_session_token() {
        let html = r#"<!doctype html><html><head><script>window.__HERMES_SESSION_TOKEN__="test-session-token_not-a-real-one_0123456789";window.__HERMES_AUTH_REQUIRED__=false;</script>"#;
        assert_eq!(scrape_token(html).as_deref(), Some("test-session-token_not-a-real-one_0123456789"));
        assert_eq!(scrape_token("<html>gated</html>"), None);
        assert_eq!(scrape_token(r#"window.__HERMES_SESSION_TOKEN__ = "abc_-1";"#).as_deref(), Some("abc_-1"));
    }

    #[test]
    fn maps_sessions_to_keys() {
        assert_eq!(session_key("fox", "tui", "20261002_094821_9771f0", None), "agent:fox:dashboard:20261002_094821_9771f0");
        assert_eq!(session_key("fox", "tui", "abc", Some("abc")), "agent:fox:main");
        assert_eq!(
            session_key("fox", "cron", "cron_ee08d3315d59_20261002_095035", None),
            "agent:fox:cron:ee08d3315d59:run:cron_ee08d3315d59_20261002_095035"
        );
        assert_eq!(session_key("default", "telegram", "tg:123", None), "agent:default:telegram:tg%3A123");
        assert_eq!(session_key("default", "api_server", "api_1_x", None), "agent:default:dashboard:api_1_x");
        assert_eq!(cron_job_of("cron_ee08d3315d59_20261002_095035").as_deref(), Some("ee08d3315d59"));
        assert_eq!(cron_job_of("20261002_094821"), None);

        assert_eq!(parse_key("agent:fox:main"), Some(KeyRef::Main("fox".into())));
        assert_eq!(
            parse_key("agent:fox:dashboard:20261002_094821_9771f0"),
            Some(KeyRef::Session { profile: "fox".into(), id: "20261002_094821_9771f0".into() })
        );
        assert_eq!(
            parse_key("agent:fox:cron:ee08d3315d59:run:cron_ee08d3315d59_20261002_095035"),
            Some(KeyRef::Session { profile: "fox".into(), id: "cron_ee08d3315d59_20261002_095035".into() })
        );
        assert_eq!(
            parse_key("agent:default:telegram:tg%3A123"),
            Some(KeyRef::Session { profile: "default".into(), id: "tg:123".into() })
        );
        assert_eq!(parse_key("global"), None);
    }

    #[test]
    fn builds_session_rows() {
        let row = json!({
            "id": "20261002_094821_9771f0", "profile": "fox", "source": "tui", "title": "lg test",
            "preview": "hello", "last_active": 1790945308.348, "started_at": 1790945301.9, "unread": true,
            "pinned": false, "archived": false, "message_count": 4,
        });
        let r = session_row("fox", &row, None, true).unwrap();
        assert_eq!(r["key"], "agent:fox:dashboard:20261002_094821_9771f0");
        assert_eq!(r["agentId"], "fox");
        assert_eq!(r["label"], "lg test");
        assert_eq!(r["lastMessagePreview"], "hello");
        assert_eq!(r["updatedAt"], 1790945308348u64);
        assert_eq!(r["unread"], true);
        assert_eq!(r["hasActiveRun"], true);
        let main = session_row("fox", &row, Some("20261002_094821_9771f0"), false).unwrap();
        assert_eq!(main["key"], "agent:fox:main");
        assert!(main.get("label").is_none(), "main chat keeps the app's own name");
    }

    #[test]
    fn maps_profiles_to_agents() {
        let p = json!({
            "name": "fox", "path": "/h/profiles/fox", "is_default": false, "model": "claude-sonnet-4",
            "provider": "anthropic", "display_name": "", "description": "A clever fox",
            "ui_meta": { "hermes-bots": { "emoji": "🦊" } },
        });
        let a = agent_from_profile(&p);
        assert_eq!(a["id"], "fox");
        assert_eq!(a["name"], "fox");
        assert_eq!(a["identity"]["emoji"], "🦊");
        assert_eq!(a["workspace"], "/h/profiles/fox");
        assert_eq!(a["model"]["primary"], "anthropic/claude-sonnet-4");
        let d = agent_from_profile(&json!({ "name": "default", "path": "/h", "model": "fake", "provider": "custom" }));
        assert_eq!(d["name"], "Hermes");
        assert_eq!(d["model"]["primary"], "custom/fake");
    }

    #[test]
    fn converts_transcripts() {
        let rows = json!([
            { "id": 1, "role": "user", "content": "hello LG_TOOL", "timestamp": 1790945301.9 },
            { "id": 2, "role": "assistant", "content": "", "reasoning": "let me check", "finish_reason": "tool_calls", "timestamp": 1790945305.6,
              "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "terminal", "arguments": "{\"command\": \"echo hi\"}" } }] },
            { "id": 3, "role": "tool", "content": "{\"output\": \"hi\", \"exit_code\": 0, \"error\": null}", "tool_call_id": "call_1", "tool_name": "terminal", "timestamp": 1790945308.3 },
            { "id": 4, "role": "tool", "content": "{\"output\": \"\", \"exit_code\": -1, \"error\": \"BLOCKED: denied\", \"status\": \"blocked\"}", "tool_call_id": "call_2", "tool_name": "terminal" },
            { "id": 5, "role": "assistant", "content": "done", "timestamp": 1790945308.35 },
            { "id": 6, "role": "system", "content": "internal" },
            { "id": 7, "role": "user", "content": "hidden", "display_kind": "hidden" },
        ]);
        let out = convert_messages(rows.as_array().unwrap());
        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["text"], "hello LG_TOOL");
        assert_eq!(out[0]["timestamp"], 1790945301900u64);
        assert_eq!(out[0]["__openclaw"]["id"], "h:1");
        assert_eq!(out[1]["content"][0], json!({ "type": "thinking", "thinking": "let me check" }));
        assert_eq!(out[1]["content"][1]["type"], "toolCall");
        assert_eq!(out[1]["content"][1]["id"], "call_1");
        assert_eq!(out[1]["content"][1]["arguments"]["command"], "echo hi");
        assert_eq!(out[2]["role"], "toolResult");
        assert_eq!(out[2]["toolCallId"], "call_1");
        assert_eq!(out[2]["content"][0]["text"], "hi");
        assert_eq!(out[2]["isError"], false);
        assert_eq!(out[3]["isError"], true);
        assert_eq!(out[3]["content"][0]["text"], "BLOCKED: denied");
        assert_eq!(out[4]["content"][0]["text"], "done");
    }

    #[test]
    fn translates_a_streamed_turn() {
        let mut turn = new_turn("run-1".into(), "agent:fox:dashboard:s1", "fox", "fox", "s1");
        let started = turn_started(&turn);
        assert_eq!(started[0].0, "sessions.changed");
        assert_eq!(started[0].1["phase"], "start");
        assert_eq!(turn_phase(&mut turn, "thinking")[0].1["phase"], "thinking");
        assert!(turn_phase(&mut turn, "thinking").is_empty(), "no repeats");

        let tool = turn_tool_start(&mut turn, "call_1", "terminal", json!({ "command": "echo hi" }), Some("echo hi"));
        assert_eq!(tool[0].0, "session.message");
        assert_eq!(tool[0].1["message"]["content"][0]["type"], "toolCall");
        assert_eq!(tool[1].0, "agent");
        assert_eq!(tool[1].1["data"]["phase"], "start");
        assert_eq!(tool[1].1["data"]["toolCallId"], "call_1");

        let end = turn_tool_end(&mut turn, "call_1", None, "hi".into(), false);
        assert_eq!(end[0].1["message"]["role"], "toolResult");
        assert_eq!(end[0].1["message"]["toolName"], "terminal");
        assert_eq!(end[1].1["data"]["phase"], "end");

        let d1 = turn_delta(&mut turn, "\n\ndo");
        assert_eq!(d1[0].1["state"], "delta");
        assert_eq!(d1[0].1["message"]["content"][0]["text"], "do");
        let d2 = turn_delta(&mut turn, "ne");
        assert_eq!(d2[0].1["message"]["content"][0]["text"], "done", "cumulative segment text");

        turn.reasoning_available = "done".into(); // Hermes echoes the reply here without real reasoning
        let fin = turn_complete(&mut turn, Some("done"), "complete", None);
        assert_eq!(fin[0].0, "session.message");
        assert_eq!(fin[0].1["message"]["content"], json!([{ "type": "text", "text": "done" }]));
        assert_eq!(fin[1].1["state"], "final");
        assert_eq!(fin[2].1["phase"], "end");
    }

    #[test]
    fn interim_text_becomes_its_own_message() {
        let mut turn = new_turn("r".into(), "k", "a", "a", "s");
        turn_delta(&mut turn, "Let me look.");
        let sealed = turn_interim(&mut turn, "Let me look.", true);
        assert_eq!(sealed[0].1["message"]["content"][0]["text"], "Let me look.");
        assert!(turn.text.is_empty());
        let fin = turn_complete(&mut turn, Some("Let me look.\nFound it."), "complete", None);
        // Text was already streamed and sealed: the final event carries no duplicate message.
        assert_eq!(fin[0].0, "chat");
        assert_eq!(fin[0].1["state"], "final");
    }

    #[test]
    fn ends_turns_by_status() {
        let mut t = new_turn("r".into(), "k", "a", "a", "s");
        let aborted = turn_complete(&mut t, Some("partial"), "interrupted", None);
        assert_eq!(aborted.iter().find(|(n, _)| n == "chat").unwrap().1["state"], "aborted");
        let mut t = new_turn("r".into(), "k", "a", "a", "s");
        let failed = turn_complete(&mut t, None, "error", Some("rate limited"));
        let chat = &failed.iter().find(|(n, _)| n == "chat").unwrap().1;
        assert_eq!(chat["state"], "error");
        assert_eq!(chat["errorMessage"], "rate limited");
        assert_eq!(failed.last().unwrap().1["phase"], "error");
    }

    #[test]
    fn maps_schedules_both_ways() {
        assert_eq!(schedule_to_hermes(&json!({ "kind": "cron", "expr": "0 9 * * 1-5", "tz": "America/Sao_Paulo" })).unwrap(), "0 9 * * 1-5");
        assert_eq!(schedule_to_hermes(&json!({ "kind": "every", "everyMs": 7_200_000 })).unwrap(), "every 2h");
        assert_eq!(schedule_to_hermes(&json!({ "kind": "every", "everyMs": 1_800_000 })).unwrap(), "every 30m");
        assert_eq!(schedule_to_hermes(&json!({ "kind": "every", "everyMs": 86_400_000 })).unwrap(), "every 1d");
        assert_eq!(schedule_to_hermes(&json!({ "kind": "at", "at": "2030-01-01T09:00:00.000Z" })).unwrap(), "2030-01-01T09:00:00.000Z");
        assert!(schedule_to_hermes(&json!({ "kind": "cron", "expr": "nope" })).is_err());
        assert!(schedule_to_hermes(&json!({ "kind": "at", "at": "tomorrow" })).is_err());

        assert_eq!(schedule_from_hermes(&json!({ "kind": "cron", "expr": "30 8 * * *", "display": "30 8 * * *" })), json!({ "kind": "cron", "expr": "30 8 * * *" }));
        assert_eq!(schedule_from_hermes(&json!({ "kind": "interval", "minutes": 120 })), json!({ "kind": "every", "everyMs": 7_200_000 }));
        assert_eq!(schedule_from_hermes(&json!({ "kind": "once", "run_at": "2030-01-01T09:00:00+00:00" })), json!({ "kind": "at", "at": "2030-01-01T09:00:00+00:00" }));
    }

    #[test]
    fn maps_cron_jobs_and_runs() {
        let job = json!({
            "id": "ee08d3315d59", "name": "Morning", "prompt": "say ok",
            "schedule": { "kind": "cron", "expr": "0 9 * * 1-5" }, "enabled": true, "state": "scheduled",
            "next_run_at": "2026-10-05T09:00:00-03:00", "last_run_at": "2026-10-02T09:50:35.517267-03:00", "last_status": "ok",
        });
        let out = cron_job_out(&job, "fox");
        assert_eq!(out["agentId"], "fox");
        assert_eq!(out["enabled"], true);
        assert_eq!(out["payload"]["message"], "say ok");
        assert_eq!(out["nextRunAtMs"], 1_791_201_600_000u64);
        assert_eq!(out["lastRunStatus"], "ok");
        assert_eq!(out["state"]["lastRunAtMs"], out["lastRunAtMs"]);
        let paused = cron_job_out(&json!({ "id": "x", "enabled": false, "state": "paused", "next_run_at": "2026-10-05T09:00:00-03:00" }), "fox");
        assert_eq!(paused["enabled"], false);
        assert!(paused["nextRunAtMs"].is_null());

        let run = json!({ "id": "cron_ee08d3315d59_20261002_095035", "started_at": 1790945435.497, "ended_at": 1790945435.509, "end_reason": "cron_complete" });
        let e = cron_run_entry("ee08d3315d59", "fox", &run, None);
        assert_eq!(e["status"], "ok");
        assert_eq!(e["sessionKey"], "agent:fox:cron:ee08d3315d59:run:cron_ee08d3315d59_20261002_095035");
        assert_eq!(e["runAtMs"], 1790945435497u64);
        let running = cron_run_entry("j", "fox", &json!({ "id": "cron_j_1", "started_at": 1.0 }), None);
        assert_eq!(running["status"], "running");
    }

    #[test]
    fn maps_approval_decisions() {
        let all: Vec<String> = ["once", "session", "always", "deny"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hermes_choice("allow-once", &all), "once");
        assert_eq!(hermes_choice("allow-always", &all), "always");
        assert_eq!(hermes_choice("deny", &all), "deny");
        let no_permanent: Vec<String> = ["once", "session", "deny"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hermes_choice("allow-always", &no_permanent), "session");
        let smart: Vec<String> = ["once", "deny"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hermes_choice("allow-always", &smart), "once");
        assert_eq!(allowed_decisions(&smart), vec!["allow-once", "deny"]);

        let a = build_approval("req-1", "fox", "agent:fox:main", &json!({
            "session_id": "31df73c0", "request_id": "req-1", "command": "rm -rf /tmp/x",
            "description": "delete in root path", "choices": ["once", "session", "always", "deny"],
        }));
        assert_eq!(a.payload["presentation"]["commandText"], "rm -rf /tmp/x");
        assert_eq!(a.payload["presentation"]["warningText"], "delete in root path");
        assert_eq!(a.payload["presentation"]["allowedDecisions"], json!(["allow-once", "allow-always", "deny"]));
        assert_eq!(a.payload["agentId"], "fox");
        assert_eq!(a.request_id.as_deref(), Some("req-1"));
    }

    #[test]
    fn tool_output_reads_terminal_results() {
        assert_eq!(tool_output_text(&json!("{\"output\": \"hi\", \"exit_code\": 0, \"error\": null}")), ("hi".into(), false));
        assert_eq!(tool_output_text(&json!("{\"output\": \"nope\", \"exit_code\": 2, \"error\": null}")), ("nope\n(exit code 2)".into(), false));
        assert_eq!(tool_output_text(&json!("plain text")), ("plain text".into(), false));
        let blocked = json!({ "output": "", "exit_code": -1, "error": "BLOCKED: denied", "status": "blocked", "user_summary": "You denied this command" });
        assert_eq!(tool_output_text(&blocked), ("BLOCKED: denied".into(), true));
    }

    #[test]
    fn maps_workspace_paths() {
        assert_eq!(workspace_rel("").unwrap(), "");
        assert_eq!(workspace_rel(" ./src//lib/ ").unwrap(), "src/lib");
        assert_eq!(workspace_rel("notes/~draft.md").unwrap(), "notes/~draft.md");
        for bad in ["/etc/passwd", "\\\\server\\share", "C:\\Users", "c:/x", "..", "a/../../b", "a/./..", "a\\..\\..\\b", "a\0b"] {
            assert!(workspace_rel(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(with_query("/api/fs/list", &[("path", Some("/w/a b&c")), ("profile", None)]), "/api/fs/list?path=%2Fw%2Fa%20b%26c");
        assert_eq!(with_query("/api/skills", &[("profile", None)]), "/api/skills");

        assert_eq!(trim_dir("/Users/me/proj/"), "/Users/me/proj");
        assert_eq!(trim_dir("/"), "/");
        assert_eq!(trim_dir("C:\\"), "C:\\");
        assert_eq!(workspace_abs("/Users/me/proj", ""), "/Users/me/proj");
        assert_eq!(workspace_abs("/Users/me/proj", "src/main.rs"), "/Users/me/proj/src/main.rs");
        assert_eq!(workspace_abs("/", "etc"), "/etc");
        assert_eq!(workspace_abs("C:\\work", "src/a.py"), "C:\\work\\src\\a.py");

        assert_eq!(workspace_relative("/Users/me/proj", "/Users/me/proj/src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(workspace_relative("/Users/me/proj/", "/Users/me/proj").as_deref(), Some(""));
        assert_eq!(workspace_relative("/Users/me/proj", "/Users/me/project2/a"), None);
        assert_eq!(workspace_relative("/Users/me/proj", "/Users/me/other"), None);
        assert_eq!(workspace_relative("/Users/me/proj", "/Users/me/proj\\x"), None);
        assert_eq!(workspace_relative("/", "/etc/hosts").as_deref(), Some("etc/hosts"));
        assert_eq!(workspace_relative("C:\\work", "C:\\work\\src\\a.py").as_deref(), Some("src/a.py"));
        assert_eq!(parent_of("a/b/c"), "a/b");
        assert_eq!(parent_of("a"), "");
    }

    #[test]
    fn lists_workspace_folders() {
        let raw = json!({ "entries": [
            { "name": "zeta.md", "path": "/w/docs/zeta.md", "isDirectory": false },
            { "name": "Alpha.txt", "path": "/w/docs/Alpha.txt", "isDirectory": false },
            { "name": "img", "path": "/w/docs/img", "isDirectory": true },
            { "name": "escape", "path": "/etc/escape", "isDirectory": true },
            { "name": "beta", "path": "/w/docs/beta", "isDirectory": true },
        ]});
        let out = workspace_listing("fox", "/w", "docs", &raw, 0, 250).unwrap();
        assert_eq!(out["agentId"], "fox");
        assert_eq!(out["path"], "docs");
        assert_eq!(out["parentPath"], "");
        assert_eq!(out["totalEntries"], 4);
        let names: Vec<&str> = out["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["beta", "img", "Alpha.txt", "zeta.md"]);
        assert_eq!(out["entries"][0], json!({ "path": "docs/beta", "name": "beta", "kind": "directory" }));
        assert_eq!(out["entries"][2], json!({ "path": "docs/Alpha.txt", "name": "Alpha.txt", "kind": "file" }));

        let page = workspace_listing("fox", "/w", "docs", &raw, 1, 2).unwrap();
        assert_eq!(page["offset"], 1);
        assert_eq!(page["totalEntries"], 4);
        let names: Vec<&str> = page["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["img", "Alpha.txt"]);

        let root = workspace_listing("fox", "/w", "", &json!({ "entries": [{ "name": "a", "path": "/w/a", "isDirectory": false }] }), 0, 250).unwrap();
        assert!(root.get("parentPath").is_none());
        assert_eq!(root["entries"][0]["path"], "a");

        let missing = workspace_listing("fox", "/w", "gone", &json!({ "entries": [], "error": "ENOENT" }), 0, 250).unwrap_err();
        assert_eq!(missing.code, "NOT_FOUND");
    }

    #[test]
    fn previews_workspace_files() {
        let text = json!({ "text": "# hi", "binary": false, "byteSize": 4, "language": "markdown", "mimeType": "text/markdown",
            "path": "/w/docs/README.md", "truncated": false });
        let Preview::Text(file) = file_preview("/w", "docs/README.md", &text).unwrap() else { panic!("expected text") };
        assert_eq!(file, json!({ "path": "docs/README.md", "name": "README.md", "size": 4, "mimeType": "text/markdown",
            "encoding": "utf8", "content": "# hi" }));

        let big = "é".repeat(PREVIEW_TEXT_MAX);
        let long = json!({ "text": big, "binary": false, "byteSize": 900_000, "mimeType": "application/octet-stream", "path": "/w/Makefile", "truncated": true });
        let Preview::Text(file) = file_preview("/w", "Makefile", &long).unwrap() else { panic!("expected text") };
        assert_eq!(file["truncated"], true);
        assert_eq!(file["mimeType"], "text/plain");
        assert_eq!(file["size"], 900_000);
        assert!(file["content"].as_str().unwrap().len() <= PREVIEW_TEXT_MAX);

        let png = json!({ "text": "\u{fffd}PNG", "binary": true, "byteSize": 2048, "mimeType": "image/png", "path": "/w/a.png" });
        assert_eq!(file_preview("/w", "a.png", &png).unwrap(), Preview::Image { mime: "image/png", size: 2048 });
        let heic = json!({ "binary": true, "byteSize": 10, "mimeType": "application/octet-stream", "path": "/w/IMG.HEIC" });
        assert_eq!(file_preview("/w", "IMG.HEIC", &heic).unwrap(), Preview::Image { mime: "image/heic", size: 10 });
        let huge = json!({ "binary": true, "byteSize": 6 * 1024 * 1024, "mimeType": "image/jpeg", "path": "/w/big.jpg" });
        assert!(file_preview("/w", "big.jpg", &huge).unwrap_err().message.contains("too big"));
        let zip = json!({ "binary": true, "byteSize": 10, "mimeType": "application/zip", "path": "/w/a.zip" });
        let err = file_preview("/w", "a.zip", &zip).unwrap_err();
        assert_eq!(err.code, "INVALID_REQUEST");
        assert!(err.message.contains("can't preview"));
        let link = json!({ "text": "secret", "binary": false, "byteSize": 6, "mimeType": "text/plain", "path": "/Users/me/.ssh/id_rsa" });
        assert!(file_preview("/w", "id_rsa", &link).is_err());

        assert_eq!(data_url_base64("data:image/png;base64,iVBORw0KGgo="), Some("iVBORw0KGgo="));
        assert_eq!(data_url_base64("data:application/octet-stream;base64,AAAA"), Some("AAAA"));
        assert_eq!(data_url_base64("https://x/y.png"), None);
    }

    #[test]
    fn maps_skills_and_toolsets() {
        let local = json!([
            { "name": "arxiv", "description": "Search papers", "category": "research", "enabled": true, "usage": 3, "provenance": "bundled" },
            { "name": "my-notes", "description": "", "category": null, "enabled": false, "usage": 0, "provenance": "agent" },
        ]);
        let skills = skills_from_local(&local);
        assert_eq!(skills[0], json!({ "name": "arxiv", "skillKey": "arxiv", "description": "Search papers", "category": "research",
            "source": "bundled", "eligible": true, "disabled": false, "usage": 3 }));
        assert_eq!(skills[1], json!({ "name": "my-notes", "skillKey": "my-notes", "source": "agent", "eligible": false, "disabled": true, "usage": 0 }));

        let api = json!({ "object": "list", "data": [{ "name": "arxiv", "description": "Search papers", "category": "research" }] });
        assert_eq!(skills_from_api(&api), vec![json!({ "name": "arxiv", "skillKey": "arxiv", "description": "Search papers",
            "category": "research", "source": "research", "eligible": true, "disabled": false })]);

        let local_sets = json!([
            { "name": "web", "label": "Web Search", "description": "Search the web", "platform": "cli", "enabled": true,
              "available": true, "configured": false, "tools": ["web_extract", "web_search"] },
            { "name": "browser", "label": "", "description": "", "platform": "cli", "enabled": false, "configured": true, "tools": [] },
        ]);
        let groups = tool_groups(&local_sets);
        assert_eq!(groups[0], json!({ "id": "web", "label": "Web Search", "description": "Search the web", "source": "toolset",
            "enabled": true, "configured": false, "tools": [{ "id": "web_extract", "label": "web_extract" }, { "id": "web_search", "label": "web_search" }] }));
        assert_eq!(groups[1], json!({ "id": "browser", "label": "browser", "source": "toolset", "enabled": false, "configured": true, "tools": [] }));
        let api_sets = json!({ "object": "list", "platform": "api_server", "data": [{ "name": "web", "label": "Web", "enabled": true, "tools": ["web_search"] }] });
        assert_eq!(tool_groups(&api_sets)[0]["tools"], json!([{ "id": "web_search", "label": "web_search" }]));
    }

    #[test]
    fn maps_plugins() {
        let manage = json!({ "plugins": [
            { "name": "honcho", "key": "memory/honcho", "version": "1.2.0", "description": "Memory", "source": "bundled", "status": "enabled",
              "install_dir": "/Users/me/.hermes/plugins/honcho", "settings_schema": [] },
            { "name": "spotify", "key": "spotify", "version": "", "description": "", "source": "user", "status": "not enabled" },
        ], "user_count": 1, "bundled_count": 1 });
        assert_eq!(plugins_out(&manage), vec![
            json!({ "id": "memory/honcho", "name": "honcho", "version": "1.2.0", "description": "Memory", "enabled": true, "state": "enabled" }),
            json!({ "id": "spotify", "name": "spotify", "enabled": false, "state": "disabled" }),
        ]);
        let legacy = json!({ "plugins": [{ "name": "disk-cleanup", "version": "?", "enabled": true }] });
        assert_eq!(plugins_out(&legacy), vec![json!({ "id": "disk-cleanup", "name": "disk-cleanup", "enabled": true, "state": "enabled" })]);
    }

    #[test]
    fn maps_mcp_servers_without_secrets() {
        let config = json!({ "servers": [
            { "name": "github", "transport": "stdio", "url": null, "command": "/Users/me/.local/bin/github-mcp-server",
              "args": ["--token", "ghp_secret"], "env": { "GITHUB_TOKEN": "ghp_***" }, "auth": null, "enabled": true,
              "tools": ["search_repos"], "source": "config", "plugin": null },
            { "name": "zapier", "transport": "http", "url": "https://user:pw@actions.zapier.com/mcp/sk-ak-1234567890abcdef/sse?api_key=xyz#frag",
              "command": null, "args": [], "env": {}, "auth": "header", "enabled": false, "tools": null, "source": "config" },
            { "name": "inline", "transport": "stdio", "command": "API_KEY=abc node", "args": [], "env": [], "enabled": true, "source": "plugin", "plugin": "p" },
        ]});
        let status = json!({ "servers": [
            { "name": "github", "transport": "stdio", "tools": 12, "connected": true, "disabled": false, "status": "connected", "source": "config" },
            { "name": "zapier", "transport": "http", "tools": 0, "connected": false, "disabled": true, "status": "disabled", "source": "config" },
        ], "checked_at": 1 });
        let out = mcp_servers_out(&config, Some(&status));
        assert_eq!(out[0], json!({ "name": "github", "transport": "stdio", "command": "github-mcp-server", "enabled": true, "tools": 12,
            "status": "connected", "connected": true, "source": "config" }));
        assert_eq!(out[1], json!({ "name": "zapier", "transport": "http", "url": "https://actions.zapier.com/mcp/…/sse", "enabled": false,
            "status": "disabled", "connected": false, "source": "config" }));
        assert_eq!(out[2], json!({ "name": "inline", "transport": "stdio", "enabled": true, "source": "plugin" }));
        let flat = serde_json::to_string(&out).unwrap();
        for secret in ["ghp_", "GITHUB_TOKEN", "--token", "user:pw", "api_key", "sk-ak", "abc", "header", "/Users/me"] {
            assert!(!flat.contains(secret), "leaked {secret}");
        }

        let no_status = mcp_servers_out(&config, None);
        assert_eq!(no_status[0]["tools"], json!(["search_repos"]));
        assert!(no_status[0].get("status").is_none());
        assert_eq!(safe_url("http://localhost:3000/mcp").as_deref(), Some("http://localhost:3000/mcp"));
        assert_eq!(program_name("C:\\Tools\\mcp.exe").as_deref(), Some("mcp.exe"));
        assert_eq!(program_name("npx").as_deref(), Some("npx"));
    }
}
