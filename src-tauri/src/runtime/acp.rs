//! ACP (Agent Client Protocol v1) adapter: any ACP agent — Claude Code
//! (claude-agent-acp), Codex (codex-acp), Gemini CLI, Goose, OpenCode… — shows
//! up as one little guy (agent id `bot`).
//!
//! - Transport: stdio (spawn the agent, newline-delimited JSON-RPC both ways,
//!   stderr = logs) or a WebSocket (`ws(s)://host:port/acp`, text frames). Both
//!   feed the same JSON-RPC engine (`Conn`).
//! - littleguys keeps its own thread index and transcripts under
//!   `<data_dir>/acp-<gatewayId>/` (`index.json`, `transcripts/*.jsonl`). They
//!   are the source of truth for `sessions.list` / `chat.history`; the agent's
//!   `session/list` is merged in, and `session/load` replays backfill history
//!   for threads littleguys never recorded.
//! - Keys: `agent:bot:main` (created lazily on first send),
//!   `agent:bot:dashboard:<sessionId>` (threads), `agent:bot:cron:<job>:run:<sessionId>`.
//! - ACP agents have no scheduler, so routines run through a `LocalScheduler`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::{now_ms, write_private};
use crate::gateways::{AcpConfig, AcpTransport};
use crate::openclaw::config::{Credential, GatewayMode};
use crate::openclaw::runtime::login_path;
use crate::runtime::scheduler::{LocalScheduler, RoutineRunner, TurnOutcome};
use crate::runtime::{keys, unsupported, Capabilities, RuntimeKind};

/// Every ACP entry is exactly one guy.
const AGENT_ID: &str = "bot";
const PROTOCOL_VERSION: u64 = 1;
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(45);
/// `npx -y` may download the agent on first launch.
const INIT_TIMEOUT: Duration = Duration::from_secs(150);
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_TTL: Duration = Duration::from_secs(30);
const LIST_MAX_PAGES: usize = 5;
/// Listing every folder's sessions reads the start and end of each transcript.
const PAST_TIMEOUT: Duration = Duration::from_secs(45);
const STDERR_LINES: usize = 60;
const MAX_TOOL_OUTPUT: usize = 20_000;
const MAX_HIDDEN: usize = 500;

// JSON-RPC / ACP error codes.
const AUTH_REQUIRED: i64 = -32000;
const METHOD_NOT_FOUND: i64 = -32601;
const REQUEST_CANCELLED: i64 = -32800;
// Local pseudo-codes (never on the wire).
const LOCAL_DISCONNECTED: i64 = -1;
const LOCAL_TIMEOUT: i64 = -2;
const LOCAL_NOT_CONNECTED: i64 = -3;

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

// ============================================================================
// Public client
// ============================================================================

/// One ACP agent. Cheap to clone.
#[derive(Clone)]
pub struct AcpClient {
    inner: Arc<Inner>,
}

impl AcpClient {
    pub fn new(id: String, name: String, config: AcpConfig, credential: Option<Credential>, data_dir: PathBuf, hooks: Hooks) -> Self {
        let dir = data_dir.join(format!("acp-{id}"));
        let status = ConnStatus {
            gateway_id: id.clone(),
            gateway_name: name.clone(),
            kind: RuntimeKind::Acp,
            capabilities: capabilities(false, config.transport == AcpTransport::Stdio, false),
            mode: Some(transport_mode(&config)),
            url: Some(describe_endpoint(&config)),
            config_path: dir.display().to_string(),
            ..Default::default()
        };
        let scheduler = LocalScheduler::new(&data_dir, &id);
        let store = Store::open(dir.clone());
        let commands = load_commands(&dir);
        Self {
            inner: Arc::new(Inner {
                id,
                name: Mutex::new(name),
                config,
                dir,
                hooks,
                status: Mutex::new(status),
                credential: Mutex::new(credential),
                stopped: AtomicBool::new(false),
                supervising: AtomicBool::new(false),
                scheduling: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                kick: Notify::new(),
                conn: Mutex::new(None),
                agent: Mutex::new(AgentState::default()),
                store: Mutex::new(store),
                live: Mutex::new(HashSet::new()),
                runs: Mutex::new(HashMap::new()),
                key_locks: Mutex::new(HashMap::new()),
                replays: Mutex::new(HashMap::new()),
                approvals: Mutex::new(HashMap::new()),
                list_cache: Mutex::new(None),
                commands: Mutex::new(commands),
                tried_auth: AtomicBool::new(false),
                scheduler,
            }),
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn status(&self) -> ConnStatus {
        self.inner.status.lock().unwrap().clone()
    }

    /// Start (or restart) the agent and the routine scheduler.
    pub fn start(&self) {
        self.inner.stopped.store(false, Ordering::SeqCst);
        if !self.inner.scheduling.swap(true, Ordering::SeqCst) {
            let runner: Arc<dyn RoutineRunner> = Arc::new(Runner(Arc::downgrade(&self.inner)));
            self.inner.scheduler.start(runner);
        }
        if self.inner.supervising.swap(true, Ordering::SeqCst) {
            self.reconnect_now();
            return;
        }
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            supervise(inner.clone()).await;
            inner.supervising.store(false, Ordering::SeqCst);
        });
    }

    /// Stop for good (bot removed or turned off): kills the agent process.
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.kick.notify_waiters();
        if self.inner.scheduling.swap(false, Ordering::SeqCst) {
            self.inner.scheduler.stop();
        }
    }

    /// Restart the agent (or retry right away if it's waiting to).
    pub fn reconnect_now(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.kick.notify_waiters();
    }

    pub fn set_name(&self, name: String) {
        *self.inner.name.lock().unwrap() = name.clone();
        self.inner.set_status(|s| s.gateway_name = name);
    }

    /// Token for WebSocket agents (sent as `Authorization: Bearer …`).
    pub fn set_credential_override(&self, credential: Option<Credential>) {
        *self.inner.credential.lock().unwrap() = credential;
        self.reconnect_now();
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        match tokio::time::timeout(timeout, self.inner.handle(method, params)).await {
            Ok(result) => result,
            Err(_) => Err(RpcError::new("TIMEOUT", format!("{method} timed out"))),
        }
    }
}

/// `local`: the agent runs on this Mac (stdio), so its folders can be read directly.
/// `past`: it lists sessions (`session/list`) and replays them (`session/load`).
fn capabilities(attachments: bool, local: bool, past: bool) -> Capabilities {
    Capabilities {
        manage_guys: false,
        routines: true,
        routines_local: true,
        approvals: true,
        thread_actions: true,
        new_threads: true,
        attachments,
        models: false,
        files: local,
        skills: true,
        commands: true,
        tools: false,
        plugins: local,
        browser: false,
        past_sessions: past,
    }
}

fn transport_mode(config: &AcpConfig) -> GatewayMode {
    match config.transport {
        AcpTransport::Stdio => GatewayMode::Local,
        AcpTransport::Websocket => {
            let url = config.url.as_deref().unwrap_or_default();
            let local = ["ws://127.0.0.1", "ws://localhost", "ws://[::1]", "wss://127.0.0.1", "wss://localhost"]
                .iter()
                .any(|p| url.starts_with(p));
            if local {
                GatewayMode::Local
            } else {
                GatewayMode::Remote
            }
        }
    }
}

/// What the status shows as the bot's "address".
fn describe_endpoint(config: &AcpConfig) -> String {
    match config.transport {
        AcpTransport::Stdio => {
            let mut parts = vec![config.command.clone().unwrap_or_default()];
            parts.extend(config.args.iter().cloned());
            parts.join(" ").trim().to_string()
        }
        AcpTransport::Websocket => config.url.clone().unwrap_or_default(),
    }
}

// ============================================================================
// State
// ============================================================================

struct Inner {
    id: String,
    name: Mutex<String>,
    config: AcpConfig,
    /// `<data_dir>/acp-<id>`
    dir: PathBuf,
    hooks: Hooks,
    status: Mutex<ConnStatus>,
    credential: Mutex<Option<Credential>>,
    stopped: AtomicBool,
    supervising: AtomicBool,
    /// The routine scheduler is running.
    scheduling: AtomicBool,
    /// Bumped by `reconnect_now`/`stop`: the live connection is dropped.
    generation: AtomicU64,
    epoch: AtomicU64,
    kick: Notify,
    conn: Mutex<Option<Arc<Conn>>>,
    agent: Mutex<AgentState>,
    store: Mutex<Store>,
    /// ACP session ids usable in the current agent process.
    live: Mutex<HashSet<String>>,
    /// Turn in flight, by session key.
    runs: Mutex<HashMap<String, Run>>,
    /// Serializes turns (and backfills) per session key.
    key_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `session/load` in progress, by ACP session id.
    replays: Mutex<HashMap<String, Replay>>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    list_cache: Mutex<Option<(Instant, Vec<Value>)>>,
    /// The agent's slash commands (latest `available_commands_update`, kept in
    /// `commands.json` so the composer has them before the first turn).
    commands: Mutex<Vec<Value>>,
    tried_auth: AtomicBool,
    scheduler: Arc<LocalScheduler>,
}

/// What the agent said about itself in `initialize`.
#[derive(Debug, Clone, Default)]
struct AgentState {
    info: Value,
    caps: Value,
    auth_methods: Vec<Value>,
}

impl AgentState {
    fn from_init(result: &Value) -> Self {
        Self {
            info: result.get("agentInfo").cloned().unwrap_or(Value::Null),
            caps: result.get("agentCapabilities").cloned().unwrap_or(Value::Null),
            auth_methods: result.get("authMethods").and_then(Value::as_array).cloned().unwrap_or_default(),
        }
    }

    fn load_session(&self) -> bool {
        self.caps.get("loadSession").and_then(Value::as_bool).unwrap_or(false)
    }

    /// `sessionCapabilities.<name>` is advertised (`{}` = yes, null/absent = no).
    fn session_cap(&self, name: &str) -> bool {
        self.caps.pointer(&format!("/sessionCapabilities/{name}")).is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    }

    fn prompt_cap(&self, name: &str) -> bool {
        self.caps.pointer(&format!("/promptCapabilities/{name}")).and_then(Value::as_bool).unwrap_or(false)
    }

    fn title(&self) -> Option<String> {
        self.info
            .get("title")
            .or_else(|| self.info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

struct Run {
    run_id: String,
    session_id: Option<String>,
    cancelled: bool,
    builder: TurnBuilder,
}

enum Replay {
    /// History is already recorded locally: drop the replay.
    Ignore,
    /// Turn the replay into transcript messages.
    Collect(Box<Collector>),
}

struct PendingApproval {
    rpc_id: Value,
    conn: Weak<Conn>,
    session_id: String,
    options: Vec<Value>,
    payload: Value,
}

struct PromptInput {
    /// ACP content blocks for `session/prompt`.
    prompt: Vec<Value>,
    /// What the transcript shows for the user's message.
    shown: Vec<Value>,
    text: String,
    /// Folder picked for a thread that hasn't started yet.
    cwd: Option<PathBuf>,
}

struct TurnResult {
    final_text: Option<String>,
}

// ============================================================================
// JSON-RPC engine
// ============================================================================

#[derive(Debug, Clone)]
struct AcpError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl AcpError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    fn from_wire(error: &Value) -> Self {
        Self {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: error.get("message").and_then(Value::as_str).unwrap_or("request failed").to_string(),
            data: error.get("data").cloned().filter(|d| !d.is_null()),
        }
    }

    fn disconnected() -> Self {
        Self::new(LOCAL_DISCONNECTED, "The agent stopped before answering.")
    }

    fn is_auth(&self) -> bool {
        self.code == AUTH_REQUIRED
    }

    /// The message plus whatever detail the agent attached.
    fn describe(&self) -> String {
        let detail = self.data.as_ref().and_then(|d| {
            d.get("details")
                .or_else(|| d.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| d.as_str().map(str::to_string))
        });
        match detail {
            Some(d) if !d.trim().is_empty() && d.trim() != self.message.trim() => format!("{}: {}", self.message, d.trim()),
            _ => self.message.clone(),
        }
    }

    fn to_rpc(&self) -> RpcError {
        let code = match self.code {
            AUTH_REQUIRED => "AUTH_REQUIRED",
            METHOD_NOT_FOUND => "UNSUPPORTED",
            -32602 => "INVALID_REQUEST",
            -32002 => "NOT_FOUND",
            REQUEST_CANCELLED => "CANCELLED",
            LOCAL_DISCONNECTED => "DISCONNECTED",
            LOCAL_TIMEOUT => "TIMEOUT",
            LOCAL_NOT_CONNECTED => "NOT_CONNECTED",
            _ => "AGENT_ERROR",
        };
        RpcError { code: code.into(), message: self.describe(), details: self.data.clone() }
    }
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.describe(), self.code)
    }
}

/// One live JSON-RPC connection to the agent (one process / one socket).
struct Conn {
    epoch: u64,
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, AcpError>>>>,
    next_id: AtomicI64,
    closed: AtomicBool,
    /// `initialize` succeeded.
    ready: AtomicBool,
    /// The agent reported (via `_auth/status_update`) that nobody is logged in.
    auth_missing: AtomicBool,
}

impl Conn {
    fn new(epoch: u64, tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            epoch,
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            auth_missing: AtomicBool::new(false),
        }
    }

    fn send(&self, message: Value) -> bool {
        !self.closed.load(Ordering::SeqCst) && self.tx.send(message.to_string()).is_ok()
    }

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if !self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })) {
            self.pending.lock().unwrap().remove(&id);
            return Err(AcpError::disconnected());
        }
        let outcome = match timeout {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(r) => r,
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    return Err(AcpError::new(LOCAL_TIMEOUT, format!("The agent didn't answer {method} in time.")));
                }
            },
            None => rx.await,
        };
        outcome.unwrap_or_else(|_| Err(AcpError::disconnected()))
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn resolve(&self, id: i64, result: Result<Value, AcpError>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(AcpError::disconnected()));
        }
    }
}

fn dispatch(inner: &Arc<Inner>, conn: &Arc<Conn>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            log::debug!("[acp {}] ignoring non-JSON output: {}", inner.id, truncate(line, 200));
            return;
        }
    };
    match message {
        Value::Array(batch) => batch.into_iter().for_each(|m| dispatch_one(inner, conn, m)),
        m => dispatch_one(inner, conn, m),
    }
}

fn dispatch_one(inner: &Arc<Inner>, conn: &Arc<Conn>, message: Value) {
    let method = message.get("method").and_then(Value::as_str).map(str::to_string);
    let id = message.get("id").cloned().filter(|v| !v.is_null());
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => inner.on_request(conn, id, &method, params),
        (Some(method), None) => inner.on_notification(conn, &method, &params),
        (None, Some(id)) => {
            let Some(id) = id.as_i64().or_else(|| id.as_str().and_then(|s| s.parse().ok())) else { return };
            let result = match message.get("error") {
                Some(e) if !e.is_null() => Err(AcpError::from_wire(e)),
                _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            conn.resolve(id, result);
        }
        _ => {}
    }
}

// ============================================================================
// Supervisor: spawn/connect, initialize, restart with backoff
// ============================================================================

enum Outcome {
    /// `reconnect_now` / `stop`.
    Kicked,
    /// The process exited / the socket closed.
    Closed { connected_for: Option<Duration>, reason: String },
    Unreachable(String),
    NoConfig(String),
    Auth(String),
    Fatal(String),
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(5)).min(30))
}

async fn supervise(inner: Arc<Inner>) {
    let mut attempt: u32 = 0;
    while !inner.stopped.load(Ordering::SeqCst) {
        let gen = inner.generation.load(Ordering::SeqCst);
        let name = inner.agent_name();
        inner.set_status(|s| {
            // Keep an actionable phase on screen while retrying.
            if !matches!(s.phase, Phase::AuthFailed | Phase::Offline | Phase::NoConfig | Phase::Error) {
                s.phase = Phase::Connecting;
                s.message = Some(format!("Starting {name}…"));
            }
            s.retry_at_ms = None;
        });
        let outcome = match inner.config.transport {
            AcpTransport::Stdio => serve_stdio(&inner, gen).await,
            AcpTransport::Websocket => serve_ws(&inner, gen).await,
        };
        let delay = match outcome {
            Outcome::Kicked => {
                attempt = 0;
                continue;
            }
            Outcome::Closed { connected_for, reason } => {
                if connected_for.is_some_and(|d| d > Duration::from_secs(60)) {
                    attempt = 0;
                }
                inner.set_phase(Phase::Offline, Some(reason));
                backoff(attempt)
            }
            Outcome::Unreachable(message) => {
                inner.set_phase(Phase::Offline, Some(message));
                backoff(attempt)
            }
            Outcome::NoConfig(message) => {
                inner.set_phase(Phase::NoConfig, Some(message));
                Duration::from_secs(60)
            }
            Outcome::Auth(message) => {
                inner.set_phase(Phase::AuthFailed, Some(message));
                Duration::from_secs(60)
            }
            Outcome::Fatal(message) => {
                inner.set_phase(Phase::Error, Some(message));
                Duration::from_secs(60)
            }
        };
        if inner.stopped.load(Ordering::SeqCst) {
            break;
        }
        attempt = attempt.saturating_add(1);
        inner.set_status(|s| s.retry_at_ms = Some(now_ms() + delay.as_millis() as u64));
        if wait_or_kick(&inner, gen, delay).await {
            attempt = 0;
        }
    }
}

/// Resolves once `reconnect_now`/`stop` has been called since `gen`.
async fn kicked(inner: &Inner, gen: u64) {
    loop {
        // Registered before the check, so a notify in between isn't lost.
        let notified = inner.kick.notified();
        if inner.generation.load(Ordering::SeqCst) != gen || inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// Returns true when woken early by `reconnect_now`.
async fn wait_or_kick(inner: &Inner, gen: u64, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = kicked(inner, gen) => true,
    }
}

/// Handshake, then wait until the transport ends or we're kicked.
async fn drive(inner: &Arc<Inner>, conn: &Arc<Conn>, gen: u64, ended: impl Future<Output = String>) -> Outcome {
    tokio::pin!(ended);
    let init = tokio::select! {
        r = handshake(inner, conn) => r,
        reason = &mut ended => return Outcome::Closed { connected_for: None, reason },
        _ = kicked(inner, gen) => return Outcome::Kicked,
    };
    if let Err(outcome) = init {
        return outcome;
    }
    let since = Instant::now();
    tokio::select! {
        reason = &mut ended => Outcome::Closed { connected_for: Some(since.elapsed()), reason },
        _ = kicked(inner, gen) => Outcome::Kicked,
    }
}

async fn handshake(inner: &Arc<Inner>, conn: &Arc<Conn>) -> Result<(), Outcome> {
    let name = inner.agent_name();
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
        "clientInfo": { "name": "littleguys", "title": "littleguys", "version": env!("CARGO_PKG_VERSION") },
    });
    let result = conn.request("initialize", params, Some(INIT_TIMEOUT)).await.map_err(|e| match e.code {
        LOCAL_TIMEOUT => Outcome::Closed {
            connected_for: None,
            reason: format!("{name} didn't finish starting within {}s.", INIT_TIMEOUT.as_secs()),
        },
        LOCAL_DISCONNECTED => Outcome::Closed { connected_for: None, reason: format!("{name} stopped while starting.") },
        AUTH_REQUIRED => Outcome::Auth(inner.auth_hint()),
        _ => Outcome::Fatal(format!("{name} refused to start: {}", e.describe())),
    })?;
    let version = result.get("protocolVersion").and_then(Value::as_u64).unwrap_or(0);
    if version != PROTOCOL_VERSION {
        return Err(Outcome::Fatal(format!(
            "{name} speaks ACP version {version}; littleguys speaks version {PROTOCOL_VERSION}."
        )));
    }
    let agent = AgentState::from_init(&result);
    let image = agent.prompt_cap("image");
    let server_version = agent.info.get("version").and_then(Value::as_str).map(str::to_string);
    let hello = json!({
        "protocolVersion": version,
        "agent": agent.info,
        "agentCapabilities": agent.caps,
        "authMethods": agent.auth_methods,
    });
    let past = agent.session_cap("list") && agent.load_session();
    *inner.agent.lock().unwrap() = agent;
    inner.live.lock().unwrap().clear();
    inner.tried_auth.store(false, Ordering::SeqCst);
    *inner.list_cache.lock().unwrap() = None;
    conn.ready.store(true, Ordering::SeqCst);
    let auth_missing = conn.auth_missing.load(Ordering::SeqCst);
    let hint = inner.auth_hint();
    inner.set_status(|s| {
        s.phase = if auth_missing { Phase::AuthFailed } else { Phase::Connected };
        s.message = auth_missing.then_some(hint);
        s.server_version = server_version;
        s.capabilities = capabilities(image, inner.config.transport == AcpTransport::Stdio, past);
        s.hello = Some(hello);
        s.retry_at_ms = None;
    });
    if !auth_missing {
        (inner.hooks.connected)();
    }
    Ok(())
}

/// Drop everything tied to a connection that just ended.
fn teardown(inner: &Arc<Inner>, conn: &Arc<Conn>) {
    conn.close();
    {
        let mut current = inner.conn.lock().unwrap();
        if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
            *current = None;
        }
    }
    inner.live.lock().unwrap().clear();
    inner.replays.lock().unwrap().clear();
    *inner.list_cache.lock().unwrap() = None;
    let dropped: Vec<String> = {
        let mut approvals = inner.approvals.lock().unwrap();
        let ids: Vec<String> = approvals
            .iter()
            .filter(|(_, a)| a.conn.upgrade().is_none_or(|c| c.epoch == conn.epoch))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            approvals.remove(id);
        }
        ids
    };
    for id in dropped {
        inner.emit("exec.approval.resolved", json!({ "id": id, "decision": "expired" }));
    }
}

async fn serve_stdio(inner: &Arc<Inner>, gen: u64) -> Outcome {
    let config = &inner.config;
    let Some(command) = config.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
        return Outcome::NoConfig(
            "Tell littleguys how to start this agent — a command like `npx -y @agentclientprotocol/claude-agent-acp`.".into(),
        );
    };
    let Some(program) = resolve_program(command) else {
        return Outcome::NoConfig(missing_program_hint(command));
    };
    let cwd = match inner.workspace() {
        Ok(cwd) => cwd,
        Err(message) => return Outcome::Fatal(message),
    };
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&config.args)
        .current_dir(&cwd)
        .env("PATH", login_path())
        .envs(config.env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Own process group, so `npx` and whatever it spawns go down together.
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Outcome::Fatal(format!("Couldn't start `{command}`: {e}")),
    };
    let pid = child.id();
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child, pid).await;
        return Outcome::Fatal("Couldn't talk to the agent process.".into());
    };
    log::info!("[acp {}] started {} (pid {:?}) in {}", inner.id, describe_endpoint(config), pid, cwd.display());

    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_task = {
        let tail = tail.clone();
        let id = inner.id.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[acp {id}] {line}");
                let mut tail = tail.lock().unwrap();
                tail.push_back(line);
                while tail.len() > STDERR_LINES {
                    tail.pop_front();
                }
            }
        })
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = Arc::new(Conn::new(inner.epoch.fetch_add(1, Ordering::SeqCst) + 1, tx));
    let writer_task = tauri::async_runtime::spawn(async move {
        let mut stdin = stdin;
        while let Some(line) = rx.recv().await {
            let ok = stdin.write_all(line.as_bytes()).await.is_ok()
                && stdin.write_all(b"\n").await.is_ok()
                && stdin.flush().await.is_ok();
            if !ok {
                break;
            }
        }
    });
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let reader_task = {
        let inner = inner.clone();
        let conn = conn.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch(&inner, &conn, &line);
            }
            let _ = done_tx.send(());
        })
    };
    *inner.conn.lock().unwrap() = Some(conn.clone());

    let name = inner.agent_name();
    let outcome = {
        let child = &mut child;
        let tail = tail.clone();
        let ended = async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = done_rx => tokio::time::timeout(Duration::from_secs(2), child.wait()).await.ok().and_then(Result::ok),
            };
            // Let stderr drain a moment so the tail has the real error.
            tokio::time::sleep(Duration::from_millis(200)).await;
            exit_message(&name, status, &stderr_tail(&tail))
        };
        drive(inner, &conn, gen, ended).await
    };
    teardown(inner, &conn);
    terminate(&mut child, pid).await;
    reader_task.abort();
    writer_task.abort();
    stderr_task.abort();
    log::info!("[acp {}] agent process ended", inner.id);
    outcome
}

async fn serve_ws(inner: &Arc<Inner>, gen: u64) -> Outcome {
    let Some(url) = inner.config.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
        return Outcome::NoConfig("Tell littleguys where this agent listens — e.g. ws://127.0.0.1:3284/acp.".into());
    };
    let url = if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        url.to_string()
    };
    let url = url.as_str();
    let mut request = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return Outcome::Fatal(format!("{url} isn't a valid address: {e}")),
    };
    {
        let headers = request.headers_mut();
        if let Ok(ua) = HeaderValue::from_str(&format!("littleguys/{}", env!("CARGO_PKG_VERSION"))) {
            headers.insert("User-Agent", ua);
        }
        let secret = match inner.credential.lock().unwrap().clone() {
            Some(Credential::Token(t)) | Some(Credential::Password(t)) => Some(t),
            _ => None,
        };
        if let Some(secret) = secret {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {secret}")) {
                headers.insert("Authorization", v);
            }
            // goose serve
            if let Ok(v) = HeaderValue::from_str(&secret) {
                headers.insert("X-Secret-Key", v);
            }
        }
    }
    let connect = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(request));
    let ws = tokio::select! {
        r = connect => match r {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) if matches!(resp.status().as_u16(), 401 | 403) => {
                return Outcome::Auth(format!("{url} turned littleguys away — paste the agent's token in littleguys."));
            }
            Ok(Err(e)) => return Outcome::Unreachable(format!("Couldn't reach {url}: {e}")),
            Err(_) => return Outcome::Unreachable(format!("{url} didn't answer.")),
        },
        _ = kicked(inner, gen) => return Outcome::Kicked,
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = Arc::new(Conn::new(inner.epoch.fetch_add(1, Ordering::SeqCst) + 1, tx));
    let writer_task = tauri::async_runtime::spawn(async move {
        while let Some(line) = rx.recv().await {
            if sink.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let (done_tx, done_rx) = oneshot::channel::<String>();
    let reader_task = {
        let inner = inner.clone();
        let conn = conn.clone();
        tauri::async_runtime::spawn(async move {
            let reason = loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => dispatch(&inner, &conn, text.as_str()),
                    Some(Ok(Message::Close(frame))) => {
                        break frame
                            .map(|f| f.reason.to_string())
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| "the agent closed the connection".into());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => break e.to_string(),
                    None => break "the connection closed".into(),
                }
            };
            let _ = done_tx.send(reason);
        })
    };
    *inner.conn.lock().unwrap() = Some(conn.clone());
    let name = inner.agent_name();
    let ended = async move {
        let reason = done_rx.await.unwrap_or_else(|_| "the connection closed".into());
        format!("Lost {name}: {reason}")
    };
    let outcome = drive(inner, &conn, gen, ended).await;
    teardown(inner, &conn);
    reader_task.abort();
    writer_task.abort();
    outcome
}

fn signal_group(pid: u32, signal: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: kill(2) has no memory-safety preconditions; a negative pid
    // targets the process group we created with process_group(0).
    unsafe {
        kill(-(pid as i32), signal);
    }
}

/// SIGTERM the agent's process group, then SIGKILL if it lingers.
async fn terminate(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        signal_group(pid, SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        if let Some(pid) = pid {
            signal_group(pid, SIGKILL);
        }
        let _ = child.kill().await;
    } else if let Some(pid) = pid {
        // The leader is gone; make sure helpers it spawned are too.
        signal_group(pid, SIGKILL);
    }
}

/// Drop ANSI escape sequences (colored logs) from a line.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn stderr_tail(tail: &Mutex<VecDeque<String>>) -> String {
    let tail = tail.lock().unwrap();
    let lines: Vec<String> = tail.iter().map(|l| strip_ansi(l.trim_end())).filter(|l| !l.trim().is_empty()).collect();
    let last = &lines[lines.len().saturating_sub(8)..];
    let text = last.join("\n");
    if text.chars().count() > 800 {
        let skip = text.chars().count() - 800;
        format!("…{}", text.chars().skip(skip).collect::<String>())
    } else {
        text
    }
}

fn exit_message(name: &str, status: Option<std::process::ExitStatus>, tail: &str) -> String {
    let code = status
        .and_then(|s| s.code())
        .map(|c| format!(" (exit code {c})"))
        .unwrap_or_default();
    if tail.is_empty() {
        format!("{name} stopped{code}.")
    } else {
        format!("{name} stopped{code}:\n{tail}")
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Absolute path for `command`, searching the login-shell PATH.
fn resolve_program(command: &str) -> Option<PathBuf> {
    let expanded = expand_tilde(command);
    if command.contains('/') {
        return is_executable(&expanded).then_some(expanded);
    }
    login_path()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| Path::new(dir).join(command))
        .find(|candidate| is_executable(candidate))
}

fn missing_program_hint(command: &str) -> String {
    let base = Path::new(command).file_name().and_then(|f| f.to_str()).unwrap_or(command);
    match base {
        "npx" | "node" | "npm" => {
            "littleguys couldn't find `npx`. Install Node.js (https://nodejs.org, or `brew install node`), then reconnect.".into()
        }
        "uvx" | "uv" => "littleguys couldn't find `uvx`. Install uv (`brew install uv`), then reconnect.".into(),
        "bunx" | "bun" => "littleguys couldn't find `bunx`. Install Bun (https://bun.sh), then reconnect.".into(),
        _ => format!("littleguys couldn't find `{command}`. Install it (or give its full path), then reconnect."),
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    if path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// `~`-expanded and absolute (relative paths are under home).
fn resolve_path(path: &str) -> PathBuf {
    let path = expand_tilde(path.trim());
    if path.is_absolute() {
        path
    } else {
        dirs::home_dir().unwrap_or_default().join(path)
    }
}

/// `resolve_path`, if it's an existing folder.
fn resolve_dir(path: &str) -> Option<PathBuf> {
    Some(resolve_path(path)).filter(|p| p.is_dir())
}

// ============================================================================
// Inner: status, events, incoming agent messages
// ============================================================================

impl Inner {
    fn agent_name(&self) -> String {
        let configured = self.config.agent_name.trim();
        if !configured.is_empty() {
            return configured.to_string();
        }
        let name = self.name.lock().unwrap().trim().to_string();
        if !name.is_empty() {
            return name;
        }
        self.agent.lock().unwrap().title().unwrap_or_else(|| "The agent".into())
    }

    fn agent(&self) -> AgentState {
        self.agent.lock().unwrap().clone()
    }

    fn set_status(&self, update: impl FnOnce(&mut ConnStatus)) {
        let snapshot = {
            let mut status = self.status.lock().unwrap();
            update(&mut status);
            status.clone()
        };
        (self.hooks.status)(&snapshot);
    }

    fn set_phase(&self, phase: Phase, message: Option<String>) {
        self.set_status(|s| {
            s.phase = phase;
            s.message = message;
            if phase != Phase::Connected {
                s.hello = None;
            }
        });
    }

    fn emit(&self, name: &str, payload: Value) {
        let frame = json!({ "type": "event", "event": name, "payload": payload });
        (self.hooks.event)(name, &payload, &frame);
    }

    fn emit_chat(&self, key: &str, run_id: &str, fields: Value) {
        let mut payload = json!({ "sessionKey": key, "runId": run_id });
        if let (Some(p), Value::Object(extra)) = (payload.as_object_mut(), fields) {
            p.extend(extra);
        }
        self.emit("chat", payload);
    }

    /// The folder the agent works in (absolute; created if it's ours).
    fn workspace(&self) -> Result<PathBuf, String> {
        match self.config.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(cwd) => resolve_dir(cwd).ok_or_else(|| format!("The agent's folder {} doesn't exist.", resolve_path(cwd).display())),
            None => {
                let path = self.dir.join("workspace");
                std::fs::create_dir_all(&path).map_err(|e| format!("Couldn't create {}: {e}", path.display()))?;
                Ok(path)
            }
        }
    }

    fn cwd_string(&self) -> String {
        self.workspace()
            .unwrap_or_else(|_| self.dir.join("workspace"))
            .display()
            .to_string()
    }

    /// The folder a thread works in: its own, else the guy's.
    fn thread_cwd(&self, key: &str) -> String {
        let own = self.store.lock().unwrap().get(key).and_then(|m| m.cwd.clone());
        own.unwrap_or_else(|| self.cwd_string())
    }

    fn auth_hint(&self) -> String {
        let probe = format!(
            "{} {} {}",
            self.config.command.clone().unwrap_or_default(),
            self.config.args.join(" "),
            self.config.registry_id.clone().unwrap_or_default()
        )
        .to_lowercase();
        let name = self.agent_name();
        if probe.contains("claude") {
            "Claude Code isn't logged in — run `claude` once in Terminal to sign in, then reconnect.".into()
        } else if probe.contains("codex") {
            "Codex isn't logged in — run `codex login` in Terminal, then reconnect.".into()
        } else if probe.contains("gemini") {
            "Gemini CLI isn't signed in — run `gemini` once in Terminal to sign in, then reconnect.".into()
        } else if self.config.transport == AcpTransport::Websocket {
            format!("{name} wants a valid token — paste it in littleguys, then reconnect.")
        } else {
            format!("{name} needs you to sign in — run it once in Terminal to log in, then reconnect.")
        }
    }

    fn mark_auth_failed(&self) {
        let hint = self.auth_hint();
        self.set_phase(Phase::AuthFailed, Some(hint));
    }

    fn require_conn(&self) -> Result<Arc<Conn>, AcpError> {
        self.conn
            .lock()
            .unwrap()
            .clone()
            .filter(|c| c.ready.load(Ordering::SeqCst) && !c.closed.load(Ordering::SeqCst))
            .ok_or_else(|| AcpError::new(LOCAL_NOT_CONNECTED, format!("{} isn't running right now.", self.agent_name())))
    }

    fn rpc_error(&self, e: &AcpError) -> RpcError {
        if e.is_auth() {
            self.mark_auth_failed();
            return RpcError::new("AUTH_REQUIRED", self.auth_hint());
        }
        e.to_rpc()
    }

    fn key_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.key_locks.lock().unwrap().entry(key.to_string()).or_default().clone()
    }

    fn key_for_session(&self, session_id: &str) -> String {
        self.store
            .lock()
            .unwrap()
            .key_for_session(session_id)
            .unwrap_or_else(|| keys::thread(AGENT_ID, session_id))
    }

    // ---- incoming ---------------------------------------------------------------

    fn on_notification(self: &Arc<Self>, conn: &Arc<Conn>, method: &str, params: &Value) {
        match method {
            "session/update" => {
                let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or_default();
                if let Some(update) = params.get("update") {
                    self.on_update(sid, update);
                }
            }
            "_auth/status_update" => self.on_auth_status(conn, params),
            "$/cancel_request" => self.on_cancel_request(conn, params),
            _ => {}
        }
    }

    fn on_request(self: &Arc<Self>, conn: &Arc<Conn>, id: Value, method: &str, params: Value) {
        match method {
            "session/request_permission" => self.on_permission(conn, id, params),
            _ => conn.respond_error(&id, METHOD_NOT_FOUND, &format!("littleguys doesn't support {method}")),
        }
    }

    fn on_update(self: &Arc<Self>, sid: &str, update: &Value) {
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();
        if kind == "session_info_update" {
            if let Some(title) = update.get("title") {
                self.set_agent_title(sid, title.as_str());
            }
        }
        if kind == "available_commands_update" {
            if let Some(commands) = update.get("availableCommands").and_then(Value::as_array) {
                let mut current = self.commands.lock().unwrap();
                if *current != *commands {
                    *current = commands.clone();
                    save_commands(&self.dir, commands);
                }
            }
            return;
        }
        {
            let mut replays = self.replays.lock().unwrap();
            if let Some(replay) = replays.get_mut(sid) {
                if let Replay::Collect(collector) = replay {
                    collector.apply(update);
                }
                return;
            }
        }
        let routed = {
            let mut runs = self.runs.lock().unwrap();
            runs.iter_mut()
                .find(|(_, r)| r.session_id.as_deref() == Some(sid))
                .map(|(key, run)| (key.clone(), run.run_id.clone(), run.builder.apply(update)))
        };
        if let Some((key, run_id, effects)) = routed {
            self.apply_effects(&key, Some(&run_id), effects);
        }
    }

    fn apply_effects(&self, key: &str, run_id: Option<&str>, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Event(name, payload) => self.emit(name, payload),
                Effect::Message(message) => self.append_message(key, run_id, message),
                Effect::Patch(message) => self.store.lock().unwrap().replace(key, &message),
            }
        }
    }

    /// Record a transcript message and tell subscribers.
    fn append_message(&self, key: &str, run_id: Option<&str>, message: Value) {
        self.store.lock().unwrap().append(key, &message);
        self.emit("session.message", json!({ "sessionKey": key, "runId": run_id, "message": message }));
    }

    fn set_agent_title(&self, sid: &str, title: Option<&str>) {
        let key = {
            let mut store = self.store.lock().unwrap();
            let Some(key) = store.key_for_session(sid) else { return };
            if let Some(meta) = store.get_mut(&key) {
                meta.title = title.map(str::trim).filter(|t| !t.is_empty()).map(|t| truncate(t, 120));
                meta.title_from_agent = meta.title.is_some();
            }
            store.save();
            key
        };
        self.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "title" }));
    }

    fn on_auth_status(&self, conn: &Arc<Conn>, params: &Value) {
        let Some(kind) = params.pointer("/authStatus/kind").and_then(Value::as_str) else { return };
        let missing = kind == "none";
        conn.auth_missing.store(missing, Ordering::SeqCst);
        if !conn.ready.load(Ordering::SeqCst) {
            return; // the handshake picks it up
        }
        let phase = self.status.lock().unwrap().phase;
        if missing && phase == Phase::Connected {
            self.mark_auth_failed();
        } else if !missing && phase == Phase::AuthFailed {
            self.set_phase(Phase::Connected, None);
            (self.hooks.connected)();
        }
    }

    fn on_cancel_request(&self, conn: &Arc<Conn>, params: &Value) {
        let Some(request_id) = params.get("requestId") else { return };
        let found = {
            let mut approvals = self.approvals.lock().unwrap();
            let id = approvals
                .iter()
                .find(|(_, a)| &a.rpc_id == request_id && a.conn.upgrade().is_some_and(|c| Arc::ptr_eq(&c, conn)))
                .map(|(id, _)| id.clone());
            id.and_then(|id| approvals.remove(&id).map(|a| (id, a)))
        };
        if let Some((id, approval)) = found {
            conn.respond_error(&approval.rpc_id, REQUEST_CANCELLED, "cancelled");
            self.emit("exec.approval.resolved", json!({ "id": id, "decision": "cancelled" }));
        }
    }

    fn on_permission(self: &Arc<Self>, conn: &Arc<Conn>, rpc_id: Value, params: Value) {
        let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or_default().to_string();
        let tool = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let options = params.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
        // Show the tool call in the transcript before asking about it.
        let routed = {
            let mut runs = self.runs.lock().unwrap();
            runs.iter_mut()
                .find(|(_, r)| r.session_id.as_deref() == Some(sid.as_str()))
                .map(|(key, run)| (key.clone(), run.run_id.clone(), run.builder.permission(&tool)))
        };
        let key = match routed {
            Some((key, run_id, effects)) => {
                self.apply_effects(&key, Some(&run_id), effects);
                key
            }
            None => self.key_for_session(&sid),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let command = approval_command(&tool);
        let payload = json!({
            "id": id,
            "agentId": AGENT_ID,
            "sessionKey": key,
            "createdAtMs": now_ms(),
            "presentation": { "commandText": command, "allowedDecisions": allowed_decisions(&options) },
            "request": {
                "id": id,
                "command": command,
                "agentId": AGENT_ID,
                "sessionKey": key,
                "cwd": self.thread_cwd(&key),
                "toolCallId": tool.get("toolCallId"),
                "toolKind": tool.get("kind"),
            },
        });
        self.approvals.lock().unwrap().insert(
            id,
            PendingApproval { rpc_id, conn: Arc::downgrade(conn), session_id: sid, options, payload: payload.clone() },
        );
        self.emit("exec.approval.requested", payload);
    }

    /// Answer every pending permission request of a session with "cancelled"
    /// (required by ACP after `session/cancel`).
    fn cancel_approvals_for(&self, sid: &str) {
        let cancelled: Vec<(String, PendingApproval)> = {
            let mut approvals = self.approvals.lock().unwrap();
            let ids: Vec<String> = approvals.iter().filter(|(_, a)| a.session_id == sid).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| approvals.remove(&id).map(|a| (id, a))).collect()
        };
        for (id, approval) in cancelled {
            if let Some(conn) = approval.conn.upgrade() {
                conn.respond(&approval.rpc_id, json!({ "outcome": { "outcome": "cancelled" } }));
            }
            self.emit("exec.approval.resolved", json!({ "id": id, "decision": "cancelled" }));
        }
    }
}

// ============================================================================
// Contract methods
// ============================================================================

fn param_str<'a>(params: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| params.get(*n).and_then(Value::as_str))
}

fn bad_request(message: &str) -> RpcError {
    RpcError::new("INVALID_REQUEST", message)
}

/// A key this guy owns.
fn session_key(params: &Value) -> Result<String, RpcError> {
    let key = param_str(params, &["sessionKey", "key"]).ok_or_else(|| bad_request("sessionKey is required"))?;
    match keys::split(key) {
        Some((AGENT_ID, rest)) if !rest.is_empty() => Ok(key.to_string()),
        _ => Err(RpcError::new("NOT_FOUND", format!("{key} isn't a thread of this bot"))),
    }
}

/// The ACP session id a thread/routine-run key was created from.
fn implied_session_id(key: &str) -> Option<String> {
    let (agent, rest) = keys::split(key)?;
    if agent != AGENT_ID {
        return None;
    }
    if rest.starts_with("dashboard:") || (rest.starts_with("cron:") && rest.contains(":run:")) {
        keys::last_id(key).filter(|id| !id.is_empty())
    } else {
        None
    }
}

impl Inner {
    async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "agents.list" => Ok(self.agents_list()),
            "sessions.list" => Ok(self.sessions_list(&params).await),
            "sessions.subscribe" | "sessions.unsubscribe" | "sessions.messages.subscribe" | "sessions.messages.unsubscribe" => {
                Ok(json!({ "subscribed": true }))
            }
            "sessions.create" => self.sessions_create(&params).await,
            "sessions.past" => self.sessions_past().await,
            "sessions.open" => self.sessions_open(&params),
            "sessions.patch" => self.sessions_patch(&params),
            "sessions.delete" => self.sessions_delete(&params).await,
            "chat.history" => self.chat_history(&params).await,
            "chat.send" => self.chat_send(&params),
            "chat.abort" => self.chat_abort(&params),
            "exec.approval.list" => {
                let approvals: Vec<Value> = self.approvals.lock().unwrap().values().map(|a| a.payload.clone()).collect();
                Ok(json!({ "approvals": approvals }))
            }
            "exec.approval.resolve" => self.resolve_approval(&params),
            "agents.workspace.list" | "agents.workspace.get" => self.workspace_call(method, params).await,
            "skills.status" => self.skills_status(&params).await,
            "skills.read" => self.skills_read(&params).await,
            "commands.list" => self.commands_list(&params).await,
            "mcp.servers.list" => self.mcp_servers_list(method, &params).await,
            "plugins.list" => self.plugins_list(method).await,
            m if m.starts_with("cron.") => self.scheduler.handle(m, params).await,
            other => Err(unsupported(other)),
        }
    }

    fn agents_list(&self) -> Value {
        let name = self.agent_name();
        let mut identity = json!({ "name": name });
        if let Some(emoji) = self.config.emoji.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            identity["emoji"] = json!(emoji);
        }
        json!({
            "defaultId": AGENT_ID,
            "mainKey": "main",
            "agents": [{
                "id": AGENT_ID,
                "name": name,
                "identity": identity,
                "workspace": self.cwd_string(),
            }],
        })
    }

    // ---- sessions -----------------------------------------------------------------

    async fn sessions_list(self: &Arc<Self>, params: &Value) -> Value {
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
        let archived = super::archived_filter(params);
        // Sessions littleguys hasn't touched are never archived.
        let listed = if archived == Some(true) { Vec::new() } else { self.agent_sessions().await };
        let running: HashSet<String> = self.runs.lock().unwrap().keys().cloned().collect();
        let cwd = self.cwd_string();
        let store = self.store.lock().unwrap();
        let mut known: HashSet<String> = store.index.hidden.iter().cloned().collect();
        let mut rows: Vec<Value> = Vec::new();
        for meta in &store.index.sessions {
            known.extend(meta.session_id.iter().cloned());
            known.extend(meta.aliases.iter().cloned());
            known.extend(implied_session_id(&meta.key));
            if archived.is_none_or(|a| a == meta.archived) {
                rows.push(session_row(meta, running.contains(&meta.key), &cwd));
            }
        }
        drop(store);
        for info in &listed {
            let Some(sid) = info.get("sessionId").and_then(Value::as_str) else { continue };
            if known.insert(sid.to_string()) {
                rows.push(listed_row(info, &cwd));
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.get("updatedAt").and_then(Value::as_u64).unwrap_or(0)));
        rows.truncate(limit);
        json!({ "count": rows.len(), "sessions": rows })
    }

    /// The agent's own session list (`session/list`), cached briefly.
    async fn agent_sessions(&self) -> Vec<Value> {
        if !self.agent().session_cap("list") {
            return Vec::new();
        }
        if let Some((at, rows)) = self.list_cache.lock().unwrap().as_ref() {
            if at.elapsed() < LIST_TTL {
                return rows.clone();
            }
        }
        let Ok(conn) = self.require_conn() else { return Vec::new() };
        let cwd = self.cwd_string();
        let canonical = std::fs::canonicalize(&cwd).map(|p| p.display().to_string()).unwrap_or_else(|_| cwd.clone());
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            let mut params = json!({ "cwd": cwd });
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            match conn.request("session/list", params, Some(LIST_TIMEOUT)).await {
                Ok(result) => {
                    let page = result.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
                    let n = page.len();
                    out.extend(page.into_iter().filter(|s| {
                        let c = s.get("cwd").and_then(Value::as_str).map(|c| c.trim_end_matches('/'));
                        c.is_none_or(|c| c == cwd.trim_end_matches('/') || c == canonical.trim_end_matches('/'))
                    }));
                    cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
                    if n == 0 || cursor.is_none() {
                        break;
                    }
                }
                Err(e) => {
                    log::info!("[acp {}] session/list failed: {e}", self.id);
                    break;
                }
            }
        }
        *self.list_cache.lock().unwrap() = Some((Instant::now(), out.clone()));
        out
    }

    /// Every session the agent can resume, from every folder, newest first (the
    /// resume picker). `inThreads`: already in the thread list.
    async fn sessions_past(self: &Arc<Self>) -> Result<Value, RpcError> {
        if !self.agent().session_cap("list") {
            return Ok(json!({ "sessions": [] }));
        }
        let conn = self.require_conn().map_err(|e| e.to_rpc())?;
        let mut listed = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            // No `cwd`: every folder.
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let result = conn.request("session/list", params, Some(PAST_TIMEOUT)).await.map_err(|e| self.rpc_error(&e))?;
            let page = result.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
            let n = page.len();
            listed.extend(page);
            cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if n == 0 || cursor.is_none() {
                break;
            }
        }
        let guy_cwd = self.cwd_string();
        let store = self.store.lock().unwrap();
        let hidden: HashSet<&str> = store.index.hidden.iter().map(String::as_str).collect();
        let mut threads: HashMap<String, String> = HashMap::new();
        for meta in &store.index.sessions {
            for sid in meta.session_id.iter().chain(&meta.aliases).cloned().chain(implied_session_id(&meta.key)) {
                threads.insert(sid, meta.key.clone());
            }
        }
        let rows: Vec<Value> = listed
            .iter()
            .filter_map(|info| {
                let sid = info.get("sessionId").and_then(Value::as_str)?;
                if hidden.contains(sid) {
                    return None;
                }
                let cwd = info.get("cwd").and_then(Value::as_str).unwrap_or(&guy_cwd);
                let key = threads.get(sid).cloned();
                Some(json!({
                    "sessionId": sid,
                    "cwd": cwd,
                    "title": info.get("title").and_then(Value::as_str),
                    "updatedAt": info.get("updatedAt").and_then(Value::as_str).and_then(parse_iso_ms),
                    // Sessions in the guy's own folder are listed with the threads already.
                    "inThreads": key.is_some() || same_dir(cwd, &guy_cwd),
                    "key": key.unwrap_or_else(|| keys::thread(AGENT_ID, sid)),
                }))
            })
            .collect();
        Ok(json!({ "sessions": rows }))
    }

    /// A session from the resume picker, as a thread that works in the session's own folder.
    fn sessions_open(&self, params: &Value) -> Result<Value, RpcError> {
        let sid = param_str(params, &["sessionId"]).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad_request("sessionId is required"))?;
        let guy_cwd = self.cwd_string();
        let known = |m: &SessionMeta| {
            m.session_id.as_deref() == Some(sid) || m.aliases.iter().any(|a| a == sid) || implied_session_id(&m.key).as_deref() == Some(sid)
        };
        if let Some(meta) = self.store.lock().unwrap().index.sessions.iter().find(|m| known(m)) {
            return Ok(json!({ "key": meta.key }));
        }
        let cwd = match param_str(params, &["cwd"]).map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => Some(
                resolve_dir(c).ok_or_else(|| bad_request(&format!("This session's folder {} doesn't exist anymore.", resolve_path(c).display())))?,
            ),
            None => None,
        };
        let own = cwd.map(|p| p.display().to_string()).filter(|c| !same_dir(c, &guy_cwd));
        let title = param_str(params, &["title"]).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let updated = params.get("updatedAt").and_then(Value::as_u64).unwrap_or_else(now_ms);
        let key = keys::thread(AGENT_ID, sid);
        let mut store = self.store.lock().unwrap();
        if !store.index.sessions.iter().any(|m| known(m)) {
            store.index.sessions.push(SessionMeta {
                key: key.clone(),
                session_id: Some(sid.to_string()),
                title_from_agent: title.is_some(),
                title,
                cwd: own,
                created_at: updated,
                updated_at: updated,
                ..Default::default()
            });
            store.save();
        }
        Ok(json!({ "key": key }))
    }

    /// Title/updatedAt the agent reported for a session we haven't indexed.
    fn listed_defaults(&self, key: &str) -> (Option<String>, Option<u64>) {
        let Some(sid) = implied_session_id(key) else { return (None, None) };
        let cache = self.list_cache.lock().unwrap();
        let info = cache
            .as_ref()
            .and_then(|(_, rows)| rows.iter().find(|r| r.get("sessionId").and_then(Value::as_str) == Some(sid.as_str())).cloned());
        let Some(info) = info else { return (None, None) };
        (
            info.get("title").and_then(Value::as_str).map(str::to_string),
            info.get("updatedAt").and_then(Value::as_str).and_then(parse_iso_ms),
        )
    }

    /// Index entry for `key`, created if needed.
    fn ensure_meta(&self, key: &str) {
        if self.store.lock().unwrap().get(key).is_some() {
            return;
        }
        let (title, updated) = self.listed_defaults(key);
        let mut store = self.store.lock().unwrap();
        if store.get(key).is_none() {
            let now = now_ms();
            store.index.sessions.push(SessionMeta {
                key: key.to_string(),
                session_id: implied_session_id(key),
                title_from_agent: title.is_some(),
                title,
                created_at: updated.unwrap_or(now),
                updated_at: updated.unwrap_or(now),
                ..Default::default()
            });
            store.save();
        }
    }

    async fn sessions_create(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        if let Some(agent) = param_str(params, &["agentId"]) {
            if agent != AGENT_ID {
                return Err(RpcError::new("NOT_FOUND", format!("no guy {agent} here")));
            }
        }
        let conn = self.require_conn().map_err(|e| e.to_rpc())?;
        let label = param_str(params, &["label"]).map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
        // In its own task: a slow session/new still gets indexed if this call times out.
        let inner = self.clone();
        let task = tauri::async_runtime::spawn(async move {
            let sid = inner.new_session(&conn, &inner.cwd_string()).await.map_err(|e| inner.rpc_error(&e))?;
            let key = keys::thread(AGENT_ID, &sid);
            let now = now_ms();
            {
                let mut store = inner.store.lock().unwrap();
                store.index.sessions.push(SessionMeta {
                    key: key.clone(),
                    session_id: Some(sid.clone()),
                    label,
                    created_at: now,
                    updated_at: now,
                    ..Default::default()
                });
                store.save();
            }
            inner.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "created" }));
            Ok(json!({ "ok": true, "key": key, "sessionId": sid }))
        });
        task.await.map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
    }

    fn sessions_patch(&self, params: &Value) -> Result<Value, RpcError> {
        let key = session_key(params)?;
        self.ensure_meta(&key);
        let running = self.runs.lock().unwrap().contains_key(&key);
        let cwd = self.cwd_string();
        let mut store = self.store.lock().unwrap();
        let meta = store.get_mut(&key).ok_or_else(|| RpcError::new("NOT_FOUND", "no such thread"))?;
        if let Some(label) = params.get("label") {
            meta.label = label.as_str().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
        }
        if let Some(v) = params.get("unread").and_then(Value::as_bool) {
            meta.unread = v;
        }
        if let Some(v) = params.get("pinned").and_then(Value::as_bool) {
            meta.pinned = v;
        }
        if let Some(v) = params.get("archived").and_then(Value::as_bool) {
            meta.archived = v;
        }
        let row = session_row(meta, running, &cwd);
        store.save();
        Ok(row)
    }

    async fn sessions_delete(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let key = session_key(params)?;
        let _ = self.chat_abort(&json!({ "sessionKey": key }));
        let meta = self.store.lock().unwrap().remove(&key);
        let mut ids: Vec<String> = Vec::new();
        if let Some(meta) = &meta {
            ids.extend(meta.session_id.iter().cloned());
            ids.extend(meta.aliases.iter().cloned());
        }
        ids.extend(implied_session_id(&key));
        ids.sort();
        ids.dedup();
        let agent = self.agent();
        if agent.session_cap("delete") {
            if let Ok(conn) = self.require_conn() {
                for sid in &ids {
                    if let Err(e) = conn.request("session/delete", json!({ "sessionId": sid }), Some(Duration::from_secs(15))).await {
                        log::info!("[acp {}] session/delete {sid} failed: {e}", self.id);
                    }
                }
            }
        }
        {
            let mut live = self.live.lock().unwrap();
            for sid in &ids {
                live.remove(sid);
            }
        }
        {
            let mut store = self.store.lock().unwrap();
            store.delete_transcript(&key);
            for sid in ids {
                if !store.index.hidden.contains(&sid) {
                    store.index.hidden.push(sid);
                }
            }
            let excess = store.index.hidden.len().saturating_sub(MAX_HIDDEN);
            store.index.hidden.drain(..excess);
            store.save();
        }
        *self.list_cache.lock().unwrap() = None;
        Ok(json!({ "ok": true, "deleted": true, "key": key }))
    }

    // ---- history --------------------------------------------------------------------

    async fn chat_history(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let key = session_key(params)?;
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(200).max(1) as usize;
        let (has_transcript, sid, deleted) = {
            let store = self.store.lock().unwrap();
            let sid = store.get(&key).and_then(|m| m.session_id.clone()).or_else(|| implied_session_id(&key));
            let deleted = store.get(&key).is_none() && sid.as_ref().is_some_and(|s| store.index.hidden.contains(s));
            (store.has_transcript(&key), sid, deleted)
        };
        if !has_transcript && !deleted {
            if let Some(sid) = sid.clone() {
                let live = self.live.lock().unwrap().contains(&sid);
                if !live && self.agent().load_session() && self.require_conn().is_ok() {
                    // In its own task: a slow replay still lands if this call times out.
                    let inner = self.clone();
                    let k = key.clone();
                    let task = tauri::async_runtime::spawn(async move { inner.backfill(&k, &sid).await });
                    if let Ok(Err(e)) = task.await {
                        log::info!("[acp {}] couldn't load history for {key}: {e}", self.id);
                    }
                }
            }
        }
        let messages = self.store.lock().unwrap().read(&key);
        let total = messages.len();
        let start = total.saturating_sub(limit);
        let in_flight = self.runs.lock().unwrap().get(&key).map(|r| {
            json!({
                "runId": r.run_id,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": r.builder.live_text() }] },
            })
        });
        Ok(json!({
            "sessionKey": key,
            "sessionId": sid,
            "messages": messages[start..].to_vec(),
            "hasMore": start > 0,
            "totalMessages": total,
            "inFlightRun": in_flight,
        }))
    }

    /// Fill in history for a thread littleguys never recorded (started in the
    /// agent's own UI, or before littleguys kept transcripts) from a replay.
    async fn backfill(self: &Arc<Self>, key: &str, sid: &str) -> Result<(), AcpError> {
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        if self.store.lock().unwrap().has_transcript(key) || self.live.lock().unwrap().contains(sid) {
            return Ok(());
        }
        let conn = self.require_conn()?;
        self.agent_sessions().await; // title/updatedAt for the new index entry
        self.ensure_meta(key);
        match self.load_session(&conn, key, sid, true).await {
            Ok(messages) => {
                let preview = messages
                    .iter()
                    .rev()
                    .filter(|m| m["role"] == "assistant")
                    .find_map(|m| {
                        let text: String = m["content"].as_array()?.iter().filter_map(|b| b["text"].as_str()).collect();
                        let text = clean_preview(&text);
                        (!text.is_empty()).then(|| truncate(&text, 200))
                    });
                let mut store = self.store.lock().unwrap();
                store.append_all(key, &messages);
                if preview.is_some() {
                    store.update(key, |m| m.last_preview = preview);
                }
                drop(store);
                self.live.lock().unwrap().insert(sid.to_string());
                Ok(())
            }
            Err(e) if e.is_auth() || matches!(e.code, LOCAL_DISCONNECTED | LOCAL_NOT_CONNECTED) => Err(e),
            Err(e) => {
                // Nothing to show; don't retry the load on every open.
                self.store.lock().unwrap().append_all(key, &[]);
                Err(e)
            }
        }
    }

    /// `session/load`, turning the replay into transcript messages (`collect`)
    /// or dropping it.
    async fn load_session(self: &Arc<Self>, conn: &Arc<Conn>, key: &str, sid: &str, collect: bool) -> Result<Vec<Value>, AcpError> {
        let replay = if collect { Replay::Collect(Box::new(Collector::new(key))) } else { Replay::Ignore };
        self.replays.lock().unwrap().insert(sid.to_string(), replay);
        let _guard = ReplayGuard { inner: self.clone(), sid: sid.to_string() };
        let params = json!({ "sessionId": sid, "cwd": self.thread_cwd(key), "mcpServers": [] });
        conn.request("session/load", params, Some(SESSION_TIMEOUT)).await?;
        let replay = self.replays.lock().unwrap().remove(sid);
        Ok(match replay {
            Some(Replay::Collect(collector)) => collector.finish(),
            _ => Vec::new(),
        })
    }

    // ---- sessions on the agent ---------------------------------------------------

    /// `session/new` in `cwd` (authenticating once if the agent asks for it).
    async fn new_session(&self, conn: &Arc<Conn>, cwd: &str) -> Result<String, AcpError> {
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        let result = match conn.request("session/new", params.clone(), Some(SESSION_TIMEOUT)).await {
            Err(e) if e.is_auth() && self.try_authenticate(conn).await => {
                conn.request("session/new", params, Some(SESSION_TIMEOUT)).await?
            }
            other => other?,
        };
        let sid = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::new(-32603, "The agent didn't return a session id."))?;
        self.live.lock().unwrap().insert(sid.clone());
        *self.list_cache.lock().unwrap() = None;
        Ok(sid)
    }

    /// Call `authenticate` once per connection, when there's an obvious method.
    async fn try_authenticate(&self, conn: &Arc<Conn>) -> bool {
        if self.tried_auth.swap(true, Ordering::SeqCst) {
            return false;
        }
        let agent = self.agent();
        let methods: Vec<&Value> = agent
            .auth_methods
            .iter()
            .filter(|m| m.get("type").and_then(Value::as_str).is_none_or(|t| t == "agent"))
            .collect();
        let has_secret = matches!(self.credential.lock().unwrap().as_ref(), Some(Credential::Token(_) | Credential::Password(_)));
        let pick = if methods.len() == 1 {
            methods.first().copied()
        } else if has_secret {
            methods.iter().copied().find(|m| {
                let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_lowercase();
                id.contains("key") || id.contains("token")
            })
        } else {
            None
        };
        let Some(method_id) = pick.and_then(|m| m.get("id")).and_then(Value::as_str) else { return false };
        log::info!("[acp {}] authenticating with {method_id}", self.id);
        match conn.request("authenticate", json!({ "methodId": method_id }), Some(AUTH_TIMEOUT)).await {
            Ok(_) => true,
            Err(e) => {
                log::info!("[acp {}] authenticate failed: {e}", self.id);
                false
            }
        }
    }

    /// Point a thread that hasn't started yet at another folder. ACP sessions
    /// are bound to their folder, so an empty one is swapped for a fresh one there.
    async fn move_thread(&self, key: &str, cwd: Option<&Path>) -> Result<(), AcpError> {
        let Some(cwd) = cwd else { return Ok(()) };
        self.ensure_meta(key);
        let target = cwd.display().to_string();
        let (sid, used) = {
            let store = self.store.lock().unwrap();
            let meta = store.get(key);
            // A title or preview means the agent has history for it, even if we couldn't load it.
            let used = store.has_messages(key) || meta.is_some_and(|m| m.title.is_some() || m.last_preview.is_some());
            (meta.and_then(|m| m.session_id.clone()), used)
        };
        if used || self.thread_cwd(key) == target {
            return Ok(());
        }
        let own = (target != self.cwd_string()).then(|| target.clone());
        let fresh = match sid {
            Some(_) => Some(self.new_session(&self.require_conn()?, &target).await?),
            None => None,
        };
        self.store.lock().unwrap().update(key, |m| {
            m.cwd = own;
            if let Some(fresh) = fresh {
                m.aliases.extend(m.session_id.replace(fresh));
            }
        });
        Ok(())
    }

    /// Make the thread's ACP session usable in the current agent process.
    async fn ensure_live(self: &Arc<Self>, key: &str) -> Result<String, AcpError> {
        let conn = self.require_conn()?;
        self.ensure_meta(key);
        let (sid, has_messages, own_cwd) = {
            let store = self.store.lock().unwrap();
            let meta = store.get(key);
            (meta.and_then(|m| m.session_id.clone()), store.has_messages(key), meta.and_then(|m| m.cwd.clone()))
        };
        if let Some(dir) = own_cwd.filter(|d| !Path::new(d).is_dir()) {
            return Err(AcpError::new(-32602, format!("This thread's folder {dir} doesn't exist anymore.")));
        }
        let cwd = self.thread_cwd(key);
        let Some(sid) = sid else {
            let sid = self.new_session(&conn, &cwd).await?;
            self.store.lock().unwrap().update(key, |m| m.session_id = Some(sid.clone()));
            return Ok(sid);
        };
        if self.live.lock().unwrap().contains(&sid) {
            return Ok(sid);
        }
        let agent = self.agent();
        let fatal = |e: &AcpError| e.is_auth() || matches!(e.code, LOCAL_DISCONNECTED | LOCAL_NOT_CONNECTED);
        if agent.session_cap("resume") {
            let params = json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] });
            match conn.request("session/resume", params, Some(SESSION_TIMEOUT)).await {
                Ok(_) => {
                    self.live.lock().unwrap().insert(sid.clone());
                    return Ok(sid);
                }
                Err(e) if fatal(&e) => return Err(e),
                Err(e) => log::info!("[acp {}] session/resume {sid} failed: {e}", self.id),
            }
        }
        if agent.load_session() {
            match self.load_session(&conn, key, &sid, !has_messages).await {
                Ok(messages) => {
                    if !has_messages {
                        self.store.lock().unwrap().append_all(key, &messages);
                    }
                    self.live.lock().unwrap().insert(sid.clone());
                    return Ok(sid);
                }
                Err(e) if fatal(&e) => return Err(e),
                Err(e) => log::info!("[acp {}] session/load {sid} failed: {e}", self.id),
            }
        }
        let fresh = self.new_session(&conn, &cwd).await?;
        self.store.lock().unwrap().update(key, |m| {
            if !m.aliases.contains(&sid) {
                m.aliases.push(sid.clone());
            }
            m.session_id = Some(fresh.clone());
        });
        if has_messages {
            let text = format!("{} started a fresh session — it won't remember the earlier messages in this chat.", self.agent_name());
            self.append_message(key, None, note_message(&text, None));
        }
        Ok(fresh)
    }

    // ---- chat ---------------------------------------------------------------------------

    fn chat_send(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let key = session_key(params)?;
        let text = param_str(params, &["message", "text"]).unwrap_or_default().to_string();
        let attachments = params.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default();
        let agent = self.agent();
        let (prompt, shown) = build_prompt(
            &text,
            &attachments,
            agent.prompt_cap("image"),
            agent.prompt_cap("embeddedContext"),
            &self.agent_name(),
        )?;
        if prompt.is_empty() {
            return Err(bad_request("Nothing to send."));
        }
        let cwd = match param_str(params, &["cwd"]).map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => Some(resolve_dir(c).ok_or_else(|| bad_request(&format!("{} isn't a folder.", resolve_path(c).display())))?),
            None => None,
        };
        self.require_conn().map_err(|e| e.to_rpc())?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let inner = self.clone();
        let (k, r) = (key.clone(), run_id.clone());
        tauri::async_runtime::spawn(async move {
            let _ = inner.run_turn(k, r, PromptInput { prompt, shown, text, cwd }).await;
        });
        Ok(json!({ "runId": run_id, "status": "started" }))
    }

    fn chat_abort(&self, params: &Value) -> Result<Value, RpcError> {
        let key = session_key(params)?;
        let found = {
            let mut runs = self.runs.lock().unwrap();
            runs.get_mut(&key).map(|r| {
                r.cancelled = true;
                (r.run_id.clone(), r.session_id.clone())
            })
        };
        let Some((run_id, sid)) = found else {
            return Ok(json!({ "ok": true, "aborted": false, "runIds": [] }));
        };
        if let Some(sid) = sid {
            if let Ok(conn) = self.require_conn() {
                conn.notify("session/cancel", json!({ "sessionId": sid }));
            }
            self.cancel_approvals_for(&sid);
        }
        Ok(json!({ "ok": true, "aborted": true, "runIds": [run_id] }))
    }

    /// One prompt turn, start to finish. Turns on a thread run one at a time.
    async fn run_turn(self: &Arc<Self>, key: String, run_id: String, input: PromptInput) -> Result<TurnResult, String> {
        let lock = self.key_lock(&key);
        let _guard = lock.lock().await;
        self.runs.lock().unwrap().insert(
            key.clone(),
            Run { run_id: run_id.clone(), session_id: None, cancelled: false, builder: TurnBuilder::live(&key, &run_id) },
        );
        self.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "start" }));
        self.emit_chat(&key, &run_id, json!({ "state": "status", "phase": "preparing_workspace" }));

        let live = match self.move_thread(&key, input.cwd.as_deref()).await {
            Ok(()) => self.ensure_live(&key).await,
            Err(e) => Err(e),
        };
        self.record_user_message(&key, &run_id, &input);

        let outcome: Result<String, AcpError> = match live {
            Err(e) => Err(e),
            Ok(sid) => {
                let cancelled = {
                    let mut runs = self.runs.lock().unwrap();
                    runs.get_mut(&key).map(|r| {
                        r.session_id = Some(sid.clone());
                        r.cancelled
                    })
                };
                if cancelled == Some(true) {
                    Ok("cancelled".into())
                } else {
                    self.emit_chat(&key, &run_id, json!({ "state": "status", "phase": "starting_model" }));
                    match self.require_conn() {
                        Err(e) => Err(e),
                        Ok(conn) => conn
                            .request("session/prompt", json!({ "sessionId": sid, "prompt": input.prompt }), None)
                            .await
                            .map(|r| r.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn").to_string()),
                    }
                }
            }
        };

        let cancelled = matches!(outcome.as_deref(), Ok("cancelled"));
        let run = self.runs.lock().unwrap().remove(&key);
        let (effects, final_text, last_assistant) = match run {
            Some(mut run) => {
                let effects = run.builder.finish(cancelled || outcome.is_err());
                (effects, run.builder.last_text.clone(), run.builder.last_assistant.clone())
            }
            None => (Vec::new(), None, None),
        };
        self.apply_effects(&key, Some(&run_id), effects);
        if let Some(note) = outcome.as_deref().ok().and_then(stop_note) {
            self.append_message(&key, Some(&run_id), note_message(note, Some(&run_id)));
        }
        self.store.lock().unwrap().update(&key, |m| {
            m.updated_at = now_ms();
            m.unread = true;
            if let Some(text) = final_text.as_deref().map(clean_preview).filter(|t| !t.is_empty()) {
                m.last_preview = Some(truncate(&text, 200));
            }
        });

        match outcome {
            Ok(stop) if stop == "cancelled" => {
                self.emit_chat(&key, &run_id, json!({ "state": "aborted", "stopReason": stop }));
                self.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "end" }));
                Err("The run was stopped.".into())
            }
            Ok(stop) => {
                self.emit_chat(&key, &run_id, json!({ "state": "final", "stopReason": stop, "message": last_assistant }));
                self.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "end" }));
                Ok(TurnResult { final_text })
            }
            Err(e) => {
                let message = if e.is_auth() {
                    self.mark_auth_failed();
                    self.auth_hint()
                } else {
                    e.describe()
                };
                log::info!("[acp {}] turn on {key} failed: {e}", self.id);
                self.emit_chat(&key, &run_id, json!({ "state": "error", "errorMessage": message }));
                self.emit("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "error" }));
                Err(message)
            }
        }
    }

    fn record_user_message(&self, key: &str, run_id: &str, input: &PromptInput) {
        let message = chat_message("user", input.shown.clone(), now_ms(), &uuid::Uuid::new_v4().to_string(), Some(run_id));
        {
            let mut store = self.store.lock().unwrap();
            store.update(key, |m| {
                if m.title.is_none() {
                    let first = input.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                    if !first.is_empty() {
                        m.title = Some(truncate(first, 80));
                    }
                }
                m.updated_at = now_ms();
            });
        }
        self.append_message(key, Some(run_id), message);
    }

    // ---- approvals ------------------------------------------------------------------

    fn resolve_approval(&self, params: &Value) -> Result<Value, RpcError> {
        let id = param_str(params, &["id", "approvalId"]).ok_or_else(|| bad_request("id is required"))?.to_string();
        let decision = param_str(params, &["decision"]).ok_or_else(|| bad_request("decision is required"))?.to_string();
        if !matches!(decision.as_str(), "allow-once" | "allow-always" | "deny") {
            return Err(bad_request("decision must be allow-once, allow-always or deny"));
        }
        let outcome = {
            let approvals = self.approvals.lock().unwrap();
            let pending = approvals.get(&id).ok_or_else(|| RpcError::new("NOT_FOUND", "That request isn't pending anymore."))?;
            match pick_option(&pending.options, &decision) {
                Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
                None if decision == "deny" => json!({ "outcome": "cancelled" }),
                None => return Err(bad_request("The agent didn't offer that choice.")),
            }
        };
        let pending = self.approvals.lock().unwrap().remove(&id).ok_or_else(|| RpcError::new("NOT_FOUND", "That request isn't pending anymore."))?;
        let conn = pending
            .conn
            .upgrade()
            .filter(|c| !c.closed.load(Ordering::SeqCst))
            .ok_or_else(|| RpcError::new("EXPIRED", "The agent restarted, so that request expired."))?;
        conn.respond(&pending.rpc_id, json!({ "outcome": outcome }));
        self.emit("exec.approval.resolved", json!({ "id": id, "decision": decision }));
        Ok(json!({ "ok": true }))
    }
}

/// Removes a `session/load` replay entry however the load ends.
struct ReplayGuard {
    inner: Arc<Inner>,
    sid: String,
}

impl Drop for ReplayGuard {
    fn drop(&mut self) {
        self.inner.replays.lock().unwrap().remove(&self.sid);
    }
}

/// `cwd`: the guy's folder, for threads without their own.
fn session_row(meta: &SessionMeta, running: bool, cwd: &str) -> Value {
    json!({
        "key": meta.key,
        "agentId": AGENT_ID,
        "kind": "direct",
        "sessionId": meta.session_id,
        "workspaceDir": meta.cwd.as_deref().unwrap_or(cwd),
        "label": meta.label,
        "derivedTitle": meta.title,
        "lastMessagePreview": meta.last_preview,
        "createdAt": meta.created_at,
        "updatedAt": meta.updated_at,
        "unread": meta.unread,
        "pinned": meta.pinned,
        "archived": meta.archived,
        "hasActiveRun": running,
        "status": if running { "running" } else { "done" },
    })
}

/// A session the agent knows about but littleguys hasn't touched yet.
fn listed_row(info: &Value, cwd: &str) -> Value {
    let sid = info.get("sessionId").and_then(Value::as_str).unwrap_or_default();
    let updated = info.get("updatedAt").and_then(Value::as_str).and_then(parse_iso_ms).unwrap_or(0);
    json!({
        "key": keys::thread(AGENT_ID, sid),
        "agentId": AGENT_ID,
        "kind": "direct",
        "sessionId": sid,
        "workspaceDir": info.get("cwd").and_then(Value::as_str).unwrap_or(cwd),
        "derivedTitle": info.get("title").and_then(Value::as_str),
        "createdAt": updated,
        "updatedAt": updated,
        "unread": false,
        "pinned": false,
        "archived": false,
        "hasActiveRun": false,
        "status": "done",
    })
}

/// The same folder, however it's spelled (trailing slash, symlinks).
fn same_dir(a: &str, b: &str) -> bool {
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p.trim_end_matches('/')));
    a.trim_end_matches('/') == b.trim_end_matches('/') || canonical(a) == canonical(b)
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis().max(0) as u64)
}

/// ACP prompt blocks + what the transcript shows for the user's message.
fn build_prompt(
    text: &str,
    attachments: &[Value],
    images_ok: bool,
    files_ok: bool,
    agent_name: &str,
) -> Result<(Vec<Value>, Vec<Value>), RpcError> {
    let mut prompt = Vec::new();
    let mut shown = Vec::new();
    if !text.trim().is_empty() {
        prompt.push(json!({ "type": "text", "text": text }));
        shown.push(json!({ "type": "text", "text": text }));
    }
    for attachment in attachments {
        let raw = attachment.get("content").or_else(|| attachment.get("data")).and_then(Value::as_str).unwrap_or_default();
        let data = raw.split_once(";base64,").map_or(raw, |(_, d)| d);
        if data.is_empty() {
            continue;
        }
        let mime = attachment.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream");
        let name = attachment.get("fileName").and_then(Value::as_str).unwrap_or("attachment");
        let is_image = attachment.get("type").and_then(Value::as_str) == Some("image") || mime.starts_with("image/");
        if is_image {
            if !images_ok {
                return Err(bad_request(&format!("{agent_name} can't look at images.")));
            }
            let block = json!({ "type": "image", "mimeType": mime, "data": data });
            prompt.push(block.clone());
            shown.push(block);
        } else {
            if !files_ok {
                return Err(bad_request(&format!("{agent_name} can't take files.")));
            }
            let uri = format!("attachment:{}", name.replace(' ', "%20"));
            let texty = mime.starts_with("text/")
                || ["json", "xml", "yaml", "csv", "javascript", "typescript", "markdown", "toml"].iter().any(|t| mime.contains(t));
            let decoded = texty
                .then(|| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            let resource = match decoded {
                Some(text) => json!({ "uri": uri, "mimeType": mime, "text": text }),
                None => json!({ "uri": uri, "mimeType": mime, "blob": data }),
            };
            prompt.push(json!({ "type": "resource", "resource": resource }));
            shown.push(json!({ "type": "text", "text": format!("📎 {name}") }));
        }
    }
    Ok((prompt, shown))
}

// ============================================================================
// The agent's folders on this Mac: workspace, skills, MCP servers, plugins
// ============================================================================
//
// Read-only and best-effort: missing or broken config files are skipped, and
// only names / program names / scrubbed URLs ever leave this module (never env
// values, headers, args or tokens).

const MAX_SKILLS: usize = 300;
const MAX_SKILL_BYTES: u64 = 256 * 1024;
/// Front matter lives at the top; no need to read whole SKILL.md files to list them.
const SKILL_HEAD_BYTES: u64 = 16 * 1024;
const MAX_SUBDIRS: usize = 1_000;
/// `~/.claude.json` also keeps per-project history; skip anything absurd.
const MAX_CONFIG_BYTES: u64 = 32 * 1024 * 1024;

/// Which CLI sits behind the adapter (decides which config folders to read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Claude,
    Codex,
    Gemini,
    Other,
}

impl Flavor {
    fn detect(probe: &str) -> Self {
        let probe = probe.to_lowercase();
        if probe.contains("claude") {
            Self::Claude
        } else if probe.contains("codex") {
            Self::Codex
        } else if probe.contains("gemini") {
            Self::Gemini
        } else {
            Self::Other
        }
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/var/empty"))
}

/// Run file work off the async runtime.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work).await.map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
}

/// `agentId`, when given, must be this bot's.
fn check_agent(params: &Value) -> Result<(), RpcError> {
    match param_str(params, &["agentId"]) {
        Some(id) if id != AGENT_ID => Err(bad_request(&format!("agent \"{id}\" not found"))),
        _ => Ok(()),
    }
}

impl Inner {
    fn is_local(&self) -> bool {
        self.config.transport == AcpTransport::Stdio
    }

    /// From the launch command first, then the names it goes by.
    fn flavor(&self) -> Flavor {
        let launch = format!(
            "{} {} {}",
            self.config.command.clone().unwrap_or_default(),
            self.config.args.join(" "),
            self.config.registry_id.clone().unwrap_or_default()
        );
        match Flavor::detect(&launch) {
            Flavor::Other => Flavor::detect(&format!("{} {}", self.agent().title().unwrap_or_default(), self.config.agent_name)),
            flavor => flavor,
        }
    }

    /// The agent's folder, for calls that read this Mac's disk (stdio agents only).
    fn local_root(&self, method: &str) -> Result<PathBuf, RpcError> {
        if !self.is_local() {
            return Err(unsupported(method));
        }
        self.workspace().map_err(|e| RpcError::new("UNAVAILABLE", e))
    }

    fn skill_dirs(&self) -> Vec<(PathBuf, &'static str)> {
        let cwd = self.workspace().unwrap_or_else(|_| PathBuf::from(self.cwd_string()));
        skill_dirs(self.flavor(), &cwd, &home())
    }

    async fn workspace_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        check_agent(&params)?;
        let root = self.local_root(method)?;
        let method = method.to_string();
        blocking(move || crate::localfs::handle(&root, AGENT_ID, &method, &params)).await
    }

    /// Slash commands the agent announced + SKILL.md skills on disk.
    async fn skills_status(&self, params: &Value) -> Result<Value, RpcError> {
        check_agent(params)?;
        let on_disk = if self.is_local() {
            let dirs = self.skill_dirs();
            blocking(move || Ok(scan_skills(&dirs))).await?
        } else {
            Vec::new()
        };
        // Agents often list their skills as commands too; the skill row says more.
        let skill_names: HashSet<&str> = on_disk.iter().map(|s| s.name.as_str()).collect();
        let commands = command_skills(&self.commands.lock().unwrap());
        let mut skills: Vec<Value> =
            commands.into_iter().filter(|c| !skill_names.contains(c["name"].as_str().unwrap_or_default())).collect();
        skills.extend(on_disk.iter().map(SkillFile::row));
        Ok(json!({ "agentId": AGENT_ID, "skills": skills }))
    }

    /// The announced slash commands for the composer; ones backed by a SKILL.md are `source: "skill"`.
    async fn commands_list(&self, params: &Value) -> Result<Value, RpcError> {
        check_agent(params)?;
        let skill_names: HashSet<String> = if self.is_local() {
            let dirs = self.skill_dirs();
            blocking(move || Ok(scan_skills(&dirs))).await?.into_iter().map(|s| s.name).collect()
        } else {
            HashSet::new()
        };
        Ok(json!({ "commands": command_entries(&self.commands.lock().unwrap(), &skill_names) }))
    }

    async fn skills_read(&self, params: &Value) -> Result<Value, RpcError> {
        check_agent(params)?;
        let name = param_str(params, &["name", "skillKey"])
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad_request("name is required"))?
            .to_string();
        if !self.is_local() {
            return Err(bad_request(&format!("{name} isn't a skill on this Mac")));
        }
        let dirs = self.skill_dirs();
        blocking(move || {
            let skill = scan_skills(&dirs)
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| bad_request(&format!("There's no skill called {name}.")))?;
            let content = read_capped(&skill.path, MAX_SKILL_BYTES)
                .ok_or_else(|| bad_request(&format!("{} is too big (or unreadable) to show.", skill.path.display())))?;
            Ok(json!({ "name": skill.name, "path": skill.path.display().to_string(), "content": content }))
        })
        .await
    }

    async fn mcp_servers_list(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        check_agent(params)?;
        let cwd = self.local_root(method)?;
        let flavor = self.flavor();
        let servers = blocking(move || Ok(read_mcp_servers(flavor, &cwd, &home()))).await?;
        Ok(json!({ "servers": servers }))
    }

    async fn plugins_list(&self, method: &str) -> Result<Value, RpcError> {
        let cwd = self.local_root(method)?;
        let flavor = self.flavor();
        let plugins = blocking(move || {
            let home = home();
            Ok(match flavor {
                Flavor::Claude => read_claude_plugins(&cwd, &home),
                Flavor::Gemini => read_gemini_extensions(&home).into_iter().map(|(dir, manifest)| gemini_plugin(&dir, &manifest)).collect(),
                Flavor::Codex | Flavor::Other => Vec::new(),
            })
        })
        .await?;
        Ok(json!({ "plugins": plugins }))
    }
}

// ---- files ----------------------------------------------------------------------

/// The whole file as text, if it's at most `max` bytes.
fn read_capped(path: &Path, max: u64) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// JSON (or JSON5 / JSON with comments) config file.
fn read_json(path: &Path) -> Option<Value> {
    let text = read_capped(path, MAX_CONFIG_BYTES)?;
    serde_json::from_str(&text).ok().or_else(|| json5::from_str(&text).ok())
}

fn string_set(v: Option<&Value>) -> HashSet<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// Non-hidden subfolders (symlinks followed), sorted.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(reader) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = reader
        .flatten()
        .take(MAX_SUBDIRS)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

// ---- skills ---------------------------------------------------------------------

/// A SKILL.md found on disk.
#[derive(Debug, Clone)]
struct SkillFile {
    name: String,
    description: Option<String>,
    /// `project` or `personal`.
    source: &'static str,
    path: PathBuf,
}

impl SkillFile {
    fn row(&self) -> Value {
        json!({
            "name": self.name,
            "skillKey": self.name,
            "description": self.description.clone().unwrap_or_default(),
            "source": self.source,
            "eligible": true,
            "disabled": false,
            "filePath": self.path.display().to_string(),
        })
    }
}

/// Where a flavor keeps its skills; project folders first (they win on name clashes).
fn skill_dirs(flavor: Flavor, cwd: &Path, home: &Path) -> Vec<(PathBuf, &'static str)> {
    let own = match flavor {
        Flavor::Claude => Some(".claude/skills"),
        Flavor::Codex => Some(".codex/skills"),
        Flavor::Gemini => Some(".gemini/skills"),
        Flavor::Other => None,
    };
    let folders: Vec<&str> = own.into_iter().chain([".agents/skills"]).collect();
    let project = folders.iter().map(|f| (cwd.join(f), "project"));
    let personal = folders.iter().map(|f| (home.join(f), "personal"));
    project.chain(personal).collect()
}

/// `<dir>/<skill>/SKILL.md` and `<dir>/<group>/<skill>/SKILL.md`, first name wins.
fn scan_skills(dirs: &[(PathBuf, &'static str)]) -> Vec<SkillFile> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (dir, source) in dirs {
        for folder in subdirs(dir) {
            let nested = if folder.join("SKILL.md").is_file() { vec![folder] } else { subdirs(&folder) };
            for skill_dir in nested {
                if found.len() >= MAX_SKILLS {
                    return found;
                }
                let path = skill_dir.join("SKILL.md");
                let Some(head) = read_head(&path, SKILL_HEAD_BYTES) else { continue };
                let (name, description) = front_matter(&head);
                let Some(name) = name.or_else(|| skill_dir.file_name().map(|f| f.to_string_lossy().into_owned())) else { continue };
                if seen.insert(name.clone()) {
                    found.push(SkillFile { name, description, source, path });
                }
            }
        }
    }
    found
}

/// The first `max` bytes of a file, as text.
fn read_head(path: &Path, max: u64) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `name` and `description` from YAML front matter. A tiny line-based reader:
/// plain, quoted and multi-line values, `>` (folded) and `|` (literal) blocks.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let body: Vec<&str> = lines.take_while(|l| !matches!(l.trim_end(), "---" | "...")).collect();
    let (mut name, mut description) = (None, None);
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        if line.starts_with(char::is_whitespace) || line.starts_with('#') {
            continue;
        }
        let Some((key, head)) = line.split_once(':') else { continue };
        // Indented (or blank) lines continue the value.
        let start = i;
        while i < body.len() && (body[i].trim().is_empty() || body[i].starts_with(char::is_whitespace)) {
            i += 1;
        }
        let value = Some(yaml_value(head.trim(), &body[start..i])).filter(|v| !v.is_empty());
        match key.trim() {
            "name" => name = value,
            "description" => description = value,
            _ => {}
        }
    }
    (name, description)
}

fn yaml_value(head: &str, more: &[&str]) -> String {
    if head.starts_with('>') || head.starts_with('|') {
        let lines: Vec<&str> = more.iter().map(|l| l.trim()).collect();
        if head.starts_with('|') {
            return lines.join("\n").trim().to_string();
        }
        // Folded: lines join with spaces, blank lines become line breaks.
        return lines.split(|l| l.is_empty()).filter(|p| !p.is_empty()).map(|p| p.join(" ")).collect::<Vec<_>>().join("\n");
    }
    let mut text = head.to_string();
    for line in more.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        text.push(' ');
        text.push_str(line);
    }
    let text = text.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        let mut out = String::new();
        let mut chars = text[1..text.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    } else if text.len() >= 2 && text.starts_with('\'') && text.ends_with('\'') {
        text[1..text.len() - 1].replace("''", "'")
    } else {
        // Plain scalars end at a ` #` comment.
        text.split(" #").next().unwrap_or_default().trim().to_string()
    }
}

fn load_commands(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("commands.json")).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default()
}

fn save_commands(dir: &Path, commands: &[Value]) {
    let saved = serde_json::to_vec_pretty(commands).map_err(|e| e.to_string()).and_then(|bytes| {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        write_private(&dir.join("commands.json"), &bytes).map_err(|e| e.to_string())
    });
    if let Err(e) = saved {
        log::warn!("could not save ACP commands: {e}");
    }
}

/// ACP `availableCommands` (`{name, description, input?:{hint}}`) → OpenClaw `CommandEntry`s.
fn command_entries(commands: &[Value], skill_names: &HashSet<String>) -> Vec<Value> {
    let mut seen = HashSet::new();
    commands
        .iter()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim().trim_start_matches('/');
            if name.is_empty() || !seen.insert(name.to_string()) {
                return None;
            }
            let input = command.get("input").filter(|i| !i.is_null());
            let mut entry = json!({
                "name": name,
                "description": command.get("description").and_then(Value::as_str).unwrap_or_default(),
                "source": if skill_names.contains(name) { "skill" } else { "native" },
                "scope": "text",
                "acceptsArgs": input.is_some(),
            });
            if let Some(hint) = input.and_then(|i| i.get("hint")).and_then(Value::as_str).map(str::trim).filter(|h| !h.is_empty()) {
                entry["args"] = json!([{ "name": hint, "description": "", "type": "string" }]);
            }
            Some(entry)
        })
        .collect()
}

/// ACP `availableCommands` → skill rows marked `source: "command"`.
fn command_skills(commands: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    commands
        .iter()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim().trim_start_matches('/');
            if name.is_empty() || !seen.insert(name.to_string()) {
                return None;
            }
            let mut row = json!({
                "name": name,
                "skillKey": format!("cmd:{name}"),
                "description": command.get("description").and_then(Value::as_str).unwrap_or_default(),
                "source": "command",
                "eligible": true,
                "disabled": false,
            });
            if let Some(hint) = command.pointer("/input/hint").and_then(Value::as_str).filter(|h| !h.trim().is_empty()) {
                row["hint"] = json!(hint);
            }
            Some(row)
        })
        .collect()
}

// ---- MCP servers ----------------------------------------------------------------

/// The agent's MCP servers from its own config files, highest-priority source first.
fn read_mcp_servers(flavor: Flavor, cwd: &Path, home: &Path) -> Vec<Value> {
    let mut servers = match flavor {
        Flavor::Claude => {
            let mut keys = vec![cwd.display().to_string()];
            keys.extend(std::fs::canonicalize(cwd).ok().map(|p| p.display().to_string()));
            claude_mcp(read_json(&home.join(".claude.json")).as_ref(), read_json(&cwd.join(".mcp.json")).as_ref(), &keys)
        }
        Flavor::Codex => [(cwd.join(".codex/config.toml"), "project"), (home.join(".codex/config.toml"), "user")]
            .iter()
            .filter_map(|(path, source)| read_capped(path, MAX_CONFIG_BYTES).map(|text| codex_mcp(&text, source)))
            .flatten()
            .collect(),
        Flavor::Gemini => {
            let mut servers = Vec::new();
            for (path, source) in [(cwd.join(".gemini/settings.json"), "project"), (home.join(".gemini/settings.json"), "user")] {
                if let Some(settings) = read_json(&path) {
                    servers.extend(gemini_mcp(&settings, source));
                }
            }
            for (_, manifest) in read_gemini_extensions(home) {
                servers.extend(mcp_rows(manifest.get("mcpServers"), "extension", "sse"));
            }
            servers
        }
        Flavor::Other => Vec::new(),
    };
    let mut seen = HashSet::new();
    servers.retain(|s| seen.insert(s["name"].as_str().unwrap_or_default().to_string()));
    servers.sort_by(|a, b| a["name"].as_str().unwrap_or_default().to_lowercase().cmp(&b["name"].as_str().unwrap_or_default().to_lowercase()));
    servers
}

/// Claude Code: `projects["<cwd>"].mcpServers` (local) > `<cwd>/.mcp.json` (project)
/// > `~/.claude.json` `mcpServers` (user).
fn claude_mcp(claude_json: Option<&Value>, mcp_json: Option<&Value>, cwd_keys: &[String]) -> Vec<Value> {
    let project = claude_json.and_then(|c| cwd_keys.iter().find_map(|k| c.get("projects")?.get(k)));
    let field = |name: &str| project.and_then(|p| p.get(name));
    let mut servers = mcp_rows(field("mcpServers"), "local", "http");
    let (approved, rejected) = (string_set(field("enabledMcpjsonServers")), string_set(field("disabledMcpjsonServers")));
    for mut row in mcp_rows(mcp_json.and_then(|m| m.get("mcpServers")), "project", "http") {
        let name = row["name"].as_str().unwrap_or_default().to_string();
        if rejected.contains(&name) {
            row["enabled"] = json!(false);
        } else if approved.contains(&name) {
            row["enabled"] = json!(true);
        }
        servers.push(row);
    }
    servers.extend(mcp_rows(claude_json.and_then(|c| c.get("mcpServers")), "user", "http"));
    let disabled = string_set(field("disabledMcpServers"));
    for row in &mut servers {
        if disabled.contains(row["name"].as_str().unwrap_or_default()) {
            row["enabled"] = json!(false);
        }
    }
    servers
}

/// Codex: `[mcp_servers.<name>]` tables in `config.toml`.
fn codex_mcp(config_toml: &str, source: &str) -> Vec<Value> {
    match toml::from_str::<Value>(config_toml) {
        Ok(config) => mcp_rows(config.get("mcp_servers"), source, "http"),
        Err(e) => {
            log::debug!("codex config.toml: {e}");
            Vec::new()
        }
    }
}

/// Gemini CLI: `mcpServers` in `settings.json` (`url` = SSE, `httpUrl` = streamable HTTP).
fn gemini_mcp(settings: &Value, source: &str) -> Vec<Value> {
    let excluded = string_set(settings.pointer("/mcp/excluded"));
    let mut servers = mcp_rows(settings.get("mcpServers"), source, "sse");
    for row in &mut servers {
        if excluded.contains(row["name"].as_str().unwrap_or_default()) {
            row["enabled"] = json!(false);
        }
    }
    servers
}

/// `{name: spec}` → safe rows. `url_transport`: what a bare `url` means.
fn mcp_rows(servers: Option<&Value>, source: &str, url_transport: &'static str) -> Vec<Value> {
    let Some(servers) = servers.and_then(Value::as_object) else { return Vec::new() };
    servers.iter().filter_map(|(name, spec)| mcp_row(name, spec, source, url_transport)).collect()
}

/// One server, reduced to what's safe to show: name, transport, the program's
/// file name or a scrubbed URL, and the enabled flag.
fn mcp_row(name: &str, spec: &Value, source: &str, url_transport: &'static str) -> Option<Value> {
    let spec = spec.as_object()?;
    let str_field = |key: &str| spec.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let http_url = str_field("httpUrl");
    let url = http_url.or_else(|| str_field("url")).or_else(|| str_field("serverUrl"));
    let declared = str_field("type").or_else(|| str_field("transport")).map(str::to_lowercase);
    let transport = match declared.as_deref() {
        Some("stdio") => "stdio",
        Some("sse") => "sse",
        Some("http" | "streamable-http" | "streamable_http" | "streamablehttp") => "http",
        _ if spec.contains_key("command") => "stdio",
        _ if http_url.is_some() => "http",
        _ if url.is_some() => url_transport,
        _ => "stdio",
    };
    let mut row = json!({ "name": name, "transport": transport, "source": source });
    if transport == "stdio" {
        if let Some(program) = str_field("command").and_then(program_name) {
            row["command"] = json!(program);
        }
    } else if let Some(url) = url.and_then(safe_url) {
        row["url"] = json!(url);
    }
    if let Some(enabled) = spec.get("enabled").and_then(Value::as_bool) {
        row["enabled"] = json!(enabled);
    } else if let Some(disabled) = spec.get("disabled").and_then(Value::as_bool) {
        row["enabled"] = json!(!disabled);
    }
    Some(row)
}

/// Just the program's file name (`/opt/bin/node --x` → `node`); never arguments
/// or `KEY=value` prefixes.
fn program_name(command: &str) -> Option<String> {
    let command = command.trim();
    let base = |p: &str| Path::new(p).file_name().and_then(|f| f.to_str()).filter(|f| !f.is_empty()).map(str::to_string);
    let first = command.split_whitespace().find(|t| !t.contains('='))?;
    match base(command).filter(|b| !b.contains(char::is_whitespace) && !b.contains('=')) {
        // A path with spaces in folder names (`/Applications/Some App.app/bin/tool`).
        Some(whole) if first.contains('/') => Some(whole),
        _ => base(first),
    }
}

/// The URL without credentials, query or fragment, and with token-looking path
/// segments masked. `None` if it isn't a plain web URL.
fn safe_url(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let looks_secret = |seg: &str| {
        seg.len() >= 24
            && seg.chars().any(|c| c.is_ascii_digit())
            && seg.chars().any(|c| c.is_ascii_alphabetic())
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    let segments: Vec<String> =
        url.path_segments().map(|s| s.map(|seg| if looks_secret(seg) { "***".to_string() } else { seg.to_string() }).collect()).unwrap_or_default();
    if !segments.is_empty() {
        url.set_path(&segments.join("/"));
    }
    Some(url.to_string())
}

// ---- plugins ----------------------------------------------------------------------

fn read_claude_plugins(cwd: &Path, home: &Path) -> Vec<Value> {
    let Some(installed) = read_json(&home.join(".claude/plugins/installed_plugins.json")) else { return Vec::new() };
    // Most specific settings first.
    let settings: Vec<Value> =
        [cwd.join(".claude/settings.local.json"), cwd.join(".claude/settings.json"), home.join(".claude/settings.json")]
            .iter()
            .filter_map(|p| read_json(p))
            .collect();
    let enabled: Vec<&Value> = settings.iter().filter_map(|s| s.get("enabledPlugins")).collect();
    let manifest = |install_path: &str| read_json(&Path::new(install_path).join(".claude-plugin/plugin.json"));
    claude_plugins(&installed, &enabled, &cwd.display().to_string(), manifest)
}

/// Claude Code's `installed_plugins.json`: `{version: 2, plugins: {"<name>@<marketplace>":
/// [{scope, installPath, version, projectPath?, …}]}}` (v1: one object per id), with
/// the on/off state from settings' `enabledPlugins` (`{"<id>": bool}`, most specific first).
fn claude_plugins(installed: &Value, enabled: &[&Value], cwd: &str, manifest: impl Fn(&str) -> Option<Value>) -> Vec<Value> {
    let Some(plugins) = installed.get("plugins").and_then(Value::as_object) else { return Vec::new() };
    let mut rows = Vec::new();
    for (id, entries) in plugins {
        let entries: Vec<&Value> = match entries {
            Value::Array(list) => list.iter().collect(),
            other => vec![other],
        };
        // Project-scoped installs only count in their own folder.
        let Some(entry) = entries
            .into_iter()
            .find(|e| e.is_object() && e.get("projectPath").and_then(Value::as_str).is_none_or(|p| p == cwd))
        else {
            continue;
        };
        let manifest = entry.get("installPath").and_then(Value::as_str).and_then(&manifest).unwrap_or(Value::Null);
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| id.split('@').next().unwrap_or(id).to_string());
        let on = enabled.iter().find_map(|map| map.get(id)).is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()));
        let mut row = json!({ "id": id, "name": name, "enabled": on, "state": if on { "enabled" } else { "disabled" } });
        if let Some(version) = entry.get("version").or_else(|| manifest.get("version")).and_then(Value::as_str) {
            row["version"] = json!(version);
        }
        if let Some(description) = manifest.get("description").and_then(Value::as_str) {
            row["description"] = json!(description);
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
    rows
}

/// `~/.gemini/extensions/<dir>/gemini-extension.json`, by folder name.
fn read_gemini_extensions(home: &Path) -> Vec<(String, Value)> {
    subdirs(&home.join(".gemini/extensions"))
        .into_iter()
        .filter_map(|dir| {
            let manifest = read_json(&dir.join("gemini-extension.json"))?;
            Some((dir.file_name()?.to_string_lossy().into_owned(), manifest))
        })
        .collect()
}

fn gemini_plugin(dir: &str, manifest: &Value) -> Value {
    let name = manifest.get("name").and_then(Value::as_str).unwrap_or(dir);
    let mut row = json!({ "id": name, "name": name, "enabled": true, "state": "enabled" });
    for key in ["version", "description"] {
        if let Some(value) = manifest.get(key).and_then(Value::as_str) {
            row[key] = json!(value);
        }
    }
    row
}

// ============================================================================
// Turns: ACP session/update → OpenClaw-shaped messages and events
// ============================================================================

/// What applying an update asks the caller to do.
#[derive(Debug, Clone)]
pub(crate) enum Effect {
    /// Emit an event as-is.
    Event(&'static str, Value),
    /// A finalized transcript message: persist it and emit `session.message`.
    Message(Value),
    /// An already-persisted message changed (same `__openclaw.id`).
    Patch(Value),
}

#[derive(Debug, Clone)]
enum Block {
    Text(String),
    Thinking(String),
    Tool(String),
    Image(Value),
}

#[derive(Debug, Clone)]
struct Draft {
    id: String,
    ts: u64,
    message_id: Option<String>,
    blocks: Vec<Block>,
}

impl Draft {
    fn new(message_id: Option<&str>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), ts: now_ms(), message_id: message_id.map(str::to_string), blocks: Vec::new() }
    }

    fn has_tool(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Tool(_)))
    }

    fn has(&self, tool_id: &str) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Tool(id) if id == tool_id))
    }

    fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn push_text(&mut self, text: &str) {
        match self.blocks.last_mut() {
            Some(Block::Text(t)) => t.push_str(text),
            _ => self.blocks.push(Block::Text(text.to_string())),
        }
    }

    fn push_thought(&mut self, text: &str) {
        match self.blocks.last_mut() {
            Some(Block::Thinking(t)) => t.push_str(text),
            _ => self.blocks.push(Block::Thinking(text.to_string())),
        }
    }

    fn is_blank(&self) -> bool {
        self.blocks.iter().all(|b| match b {
            Block::Text(t) | Block::Thinking(t) => t.trim().is_empty(),
            _ => false,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ToolTrack {
    name: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    raw_input: Option<Value>,
    content: Option<Value>,
    raw_output: Option<Value>,
    status: Option<String>,
    /// Transcript id of the assistant message carrying this call, once emitted.
    message: Option<String>,
    emitted_input: Option<Value>,
    ended: bool,
}

impl ToolTrack {
    fn merge(&mut self, update: &Value) {
        let str_field = |k: &str| update.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(name) = str_field("name").or_else(|| {
            update
                .pointer("/_meta/claudeCode/toolName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }) {
            self.name = Some(name);
        }
        if let Some(title) = str_field("title") {
            self.title = Some(title);
        }
        if let Some(kind) = str_field("kind") {
            self.kind = Some(kind);
        }
        if let Some(status) = str_field("status") {
            self.status = Some(status);
        }
        if let Some(v) = update.get("rawInput").filter(|v| !v.is_null()) {
            self.raw_input = Some(v.clone());
        }
        if let Some(v) = update.get("content").filter(|v| !v.is_null()) {
            self.content = Some(v.clone());
        }
        if let Some(v) = update.get("rawOutput").filter(|v| !v.is_null()) {
            self.raw_output = Some(v.clone());
        }
    }

    fn has_input(&self) -> bool {
        match &self.raw_input {
            None | Some(Value::Null) => false,
            Some(Value::Object(m)) => !m.is_empty(),
            Some(_) => true,
        }
    }

    /// Bold label on the tool chip: the tool's own name when the agent gives
    /// one, else the title (when there are no arguments to show), else its kind.
    fn display_name(&self) -> String {
        let kind = self.kind.clone().filter(|k| k != "other");
        self.name
            .clone()
            .or_else(|| if self.has_input() { None } else { self.title.clone() })
            .or(kind)
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| "tool".into())
    }

    fn arguments(&self) -> Value {
        match &self.raw_input {
            Some(v @ Value::Object(_)) => v.clone(),
            Some(Value::Null) | None => json!({}),
            Some(v) => json!({ "input": v }),
        }
    }

    fn status(&self) -> &str {
        self.status.as_deref().unwrap_or("pending")
    }
}

/// Builds transcript messages from one turn's `session/update`s (or from a
/// `session/load` replay), OpenClaw-style:
/// assistant[thinking, text, toolCall…] → toolResult… → assistant[text].
pub(crate) struct TurnBuilder {
    key: String,
    /// None while replaying history (no events then).
    run_id: Option<String>,
    replay: bool,
    draft: Option<Draft>,
    /// Replay only: the user message being assembled.
    user: Option<Draft>,
    tools: HashMap<String, ToolTrack>,
    tool_order: Vec<String>,
    /// Assistant drafts already emitted (to patch late tool input).
    emitted: Vec<Draft>,
    last_text: Option<String>,
    last_assistant: Option<Value>,
}

impl TurnBuilder {
    pub(crate) fn live(key: &str, run_id: &str) -> Self {
        Self::new(key, Some(run_id.to_string()), false)
    }

    pub(crate) fn replay(key: &str) -> Self {
        Self::new(key, None, true)
    }

    fn new(key: &str, run_id: Option<String>, replay: bool) -> Self {
        Self {
            key: key.to_string(),
            run_id,
            replay,
            draft: None,
            user: None,
            tools: HashMap::new(),
            tool_order: Vec::new(),
            emitted: Vec::new(),
            last_text: None,
            last_assistant: None,
        }
    }

    /// The reply text streaming right now.
    pub(crate) fn live_text(&self) -> String {
        self.draft.as_ref().map(Draft::text).unwrap_or_default()
    }

    pub(crate) fn apply(&mut self, update: &Value) -> Vec<Effect> {
        let mut fx = Vec::new();
        match update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default() {
            "agent_message_chunk" => self.on_chunk(update, false, &mut fx),
            "agent_thought_chunk" => self.on_chunk(update, true, &mut fx),
            "user_message_chunk" if self.replay => self.on_user_chunk(update, &mut fx),
            "tool_call" | "tool_call_update" => self.on_tool(update, &mut fx),
            _ => {}
        }
        fx
    }

    /// A permission request for `tool`: make sure the call is on screen.
    pub(crate) fn permission(&mut self, tool: &Value) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(id) = tool.get("toolCallId").and_then(Value::as_str).map(str::to_string) else { return fx };
        let mut update = tool.clone();
        if let Some(obj) = update.as_object_mut() {
            obj.remove("status"); // the request's "pending" isn't news
            obj.insert("sessionUpdate".into(), json!("tool_call_update"));
        }
        self.on_tool(&update, &mut fx);
        if self.draft.as_ref().is_some_and(|d| d.has(&id)) {
            self.finalize(&mut fx);
        }
        fx
    }

    /// The turn is over: flush everything; tools that never finished get a
    /// result (an error if the turn was `interrupted`).
    pub(crate) fn finish(&mut self, interrupted: bool) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.flush_user(&mut fx);
        self.finalize(&mut fx);
        let open: Vec<String> = self.tool_order.iter().filter(|id| !self.tools[*id].ended).cloned().collect();
        for id in open {
            self.end_tool(&id, Some(interrupted), &mut fx);
        }
        fx
    }

    fn on_chunk(&mut self, update: &Value, thought: bool, fx: &mut Vec<Effect>) {
        self.flush_user(fx);
        let content = update.get("content").unwrap_or(&Value::Null);
        let mid = update.get("messageId").and_then(Value::as_str);
        let split = self.draft.as_ref().is_some_and(|d| {
            d.has_tool() || (mid.is_some() && d.message_id.is_some() && d.message_id.as_deref() != mid && !d.blocks.is_empty())
        });
        if split {
            self.finalize(fx);
        }
        let draft = self.draft.get_or_insert_with(|| Draft::new(mid));
        if draft.message_id.is_none() {
            draft.message_id = mid.map(str::to_string);
        }
        match content.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = content.get("text").and_then(Value::as_str).unwrap_or_default();
                if text.is_empty() {
                    return;
                }
                if thought {
                    draft.push_thought(text);
                } else {
                    draft.push_text(text);
                    if !self.replay {
                        fx.push(Effect::Event(
                            "chat",
                            json!({
                                "sessionKey": self.key,
                                "runId": self.run_id,
                                "state": "delta",
                                "message": {
                                    "role": "assistant",
                                    "content": [{ "type": "text", "text": draft.text() }],
                                    "timestamp": draft.ts,
                                },
                                "deltaText": text,
                            }),
                        ));
                    }
                }
            }
            Some("image") if !thought => {
                draft.blocks.push(Block::Image(json!({
                    "type": "image",
                    "mimeType": content.get("mimeType"),
                    "data": content.get("data"),
                })));
            }
            Some("resource_link") if !thought => {
                if let Some(link) = content_block_text(content) {
                    draft.push_text(&link);
                }
            }
            Some("resource") if !thought => {
                if let Some(text) = content.pointer("/resource/text").and_then(Value::as_str) {
                    draft.push_text(text);
                }
            }
            _ => {}
        }
    }

    fn on_user_chunk(&mut self, update: &Value, fx: &mut Vec<Effect>) {
        self.finalize(fx);
        let content = update.get("content").unwrap_or(&Value::Null);
        let mid = update.get("messageId").and_then(Value::as_str);
        if self.user.as_ref().is_some_and(|u| mid.is_some() && u.message_id.is_some() && u.message_id.as_deref() != mid) {
            self.flush_user(fx);
        }
        let user = self.user.get_or_insert_with(|| Draft::new(mid));
        match content.get("type").and_then(Value::as_str) {
            Some("text") => user.push_text(content.get("text").and_then(Value::as_str).unwrap_or_default()),
            Some("image") => user.blocks.push(Block::Image(json!({
                "type": "image",
                "mimeType": content.get("mimeType"),
                "data": content.get("data"),
            }))),
            _ => {
                if let Some(text) = content_block_text(content) {
                    user.push_text(&text);
                }
            }
        }
    }

    fn flush_user(&mut self, fx: &mut Vec<Effect>) {
        let Some(user) = self.user.take() else { return };
        if user.is_blank() {
            return;
        }
        let content = user
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(json!({ "type": "text", "text": t })),
                Block::Image(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        fx.push(Effect::Message(chat_message("user", content, user.ts, &user.id, None)));
    }

    fn on_tool(&mut self, update: &Value, fx: &mut Vec<Effect>) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str).map(str::to_string) else { return };
        self.flush_user(fx);
        let is_new = !self.tools.contains_key(&id);
        self.tools.entry(id.clone()).or_default().merge(update);
        if is_new {
            self.tool_order.push(id.clone());
            self.draft.get_or_insert_with(|| Draft::new(None)).blocks.push(Block::Tool(id.clone()));
            if !self.replay {
                let tool = &self.tools[&id];
                fx.push(self.tool_event(&id, "start", "running", false));
                fx.push(Effect::Event(
                    "chat",
                    json!({
                        "sessionKey": self.key,
                        "runId": self.run_id,
                        "state": "status",
                        "phase": tool_phase(tool.kind.as_deref()),
                    }),
                ));
            }
        } else {
            self.maybe_patch(&id, fx);
        }
        let in_draft = self.draft.as_ref().is_some_and(|d| d.has(&id));
        let (status, ended) = {
            let tool = &self.tools[&id];
            (tool.status().to_string(), tool.ended)
        };
        if status == "in_progress" && in_draft {
            self.finalize(fx);
        }
        if matches!(status.as_str(), "completed" | "failed") && !ended {
            if in_draft {
                self.finalize(fx); // the call goes before its result
            }
            self.end_tool(&id, None, fx);
        }
    }

    fn tool_event(&self, id: &str, phase: &str, status: &str, is_error: bool) -> Effect {
        let tool = self.tools.get(id).cloned().unwrap_or_default();
        let mut data = json!({
            "kind": "tool",
            "phase": phase,
            "toolCallId": id,
            "name": tool.display_name(),
            "title": tool.title,
            "status": status,
        });
        if phase == "end" {
            data["isError"] = json!(is_error);
        }
        Effect::Event(
            "agent",
            json!({ "sessionKey": self.key, "runId": self.run_id, "stream": "tool", "ts": now_ms(), "data": data }),
        )
    }

    /// `forced`: the turn ended first (`Some(is_error)`).
    fn end_tool(&mut self, id: &str, forced: Option<bool>, fx: &mut Vec<Effect>) {
        let Some(tool) = self.tools.get_mut(id) else { return };
        tool.ended = true;
        let tool = tool.clone();
        let failed = forced.unwrap_or(tool.status() == "failed");
        let text = match forced {
            Some(true) => "(stopped before it finished)".to_string(),
            _ => tool_output_text(tool.content.as_ref(), tool.raw_output.as_ref()),
        };
        let text = if text.trim().is_empty() && failed { "(failed)".to_string() } else { text };
        if !self.replay {
            fx.push(self.tool_event(id, "end", if failed { "failed" } else { "completed" }, failed));
        }
        let mut message = chat_message(
            "toolResult",
            vec![json!({ "type": "text", "text": text })],
            now_ms(),
            &uuid::Uuid::new_v4().to_string(),
            self.run_id.as_deref(),
        );
        message["toolCallId"] = json!(id);
        message["toolName"] = json!(tool.display_name());
        message["isError"] = json!(failed);
        fx.push(Effect::Message(message));
    }

    /// Tool input that arrives after its message went out updates the record.
    fn maybe_patch(&mut self, id: &str, fx: &mut Vec<Effect>) {
        let Some(tool) = self.tools.get(id) else { return };
        let Some(message_id) = tool.message.clone() else { return };
        if tool.raw_input == tool.emitted_input {
            return;
        }
        let Some(draft) = self.emitted.iter().find(|d| d.id == message_id).cloned() else { return };
        let message = self.build_assistant(&draft);
        for block in &draft.blocks {
            if let Block::Tool(tid) = block {
                if let Some(t) = self.tools.get_mut(tid) {
                    t.emitted_input = t.raw_input.clone();
                }
            }
        }
        fx.push(Effect::Patch(message));
    }

    fn finalize(&mut self, fx: &mut Vec<Effect>) {
        let Some(draft) = self.draft.take() else { return };
        if draft.blocks.is_empty() || draft.is_blank() {
            return;
        }
        let message = self.build_assistant(&draft);
        for block in &draft.blocks {
            if let Block::Tool(id) = block {
                if let Some(t) = self.tools.get_mut(id) {
                    t.message = Some(draft.id.clone());
                    t.emitted_input = t.raw_input.clone();
                }
            }
        }
        let text = draft.text();
        if !text.trim().is_empty() {
            self.last_text = Some(text);
        }
        self.last_assistant = Some(message.clone());
        self.emitted.push(draft);
        fx.push(Effect::Message(message));
    }

    fn build_assistant(&self, draft: &Draft) -> Value {
        let content = draft
            .blocks
            .iter()
            .map(|b| match b {
                Block::Text(t) => json!({ "type": "text", "text": t }),
                Block::Thinking(t) => json!({ "type": "thinking", "thinking": t }),
                Block::Tool(id) => {
                    let tool = self.tools.get(id).cloned().unwrap_or_default();
                    json!({ "type": "toolCall", "id": id, "name": tool.display_name(), "arguments": tool.arguments() })
                }
                Block::Image(v) => v.clone(),
            })
            .collect();
        chat_message("assistant", content, draft.ts, &draft.id, self.run_id.as_deref())
    }
}

/// Collects a `session/load` replay into transcript messages.
struct Collector {
    builder: TurnBuilder,
    messages: Vec<Value>,
}

impl Collector {
    fn new(key: &str) -> Self {
        Self { builder: TurnBuilder::replay(key), messages: Vec::new() }
    }

    fn apply(&mut self, update: &Value) {
        let effects = self.builder.apply(update);
        self.absorb(effects);
    }

    fn absorb(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Message(m) => self.messages.push(m),
                Effect::Patch(m) => {
                    let id = message_id(&m);
                    if let Some(slot) = self.messages.iter_mut().find(|x| message_id(x) == id) {
                        *slot = m;
                    }
                }
                Effect::Event(..) => {}
            }
        }
    }

    fn finish(mut self) -> Vec<Value> {
        let effects = self.builder.finish(false);
        self.absorb(effects);
        self.messages
    }
}

fn chat_message(role: &str, content: Vec<Value>, ts: u64, id: &str, run_id: Option<&str>) -> Value {
    let mut meta = json!({ "id": id });
    if let Some(run_id) = run_id {
        meta["runId"] = json!(run_id);
    }
    json!({ "role": role, "content": content, "timestamp": ts, "__openclaw": meta })
}

/// A muted line in the transcript (not sent to the agent).
fn note_message(text: &str, run_id: Option<&str>) -> Value {
    let mut message = chat_message("system", vec![json!({ "type": "text", "text": text })], now_ms(), &uuid::Uuid::new_v4().to_string(), run_id);
    message["__openclaw"]["kind"] = json!("note");
    message
}

fn message_id(message: &Value) -> Option<&str> {
    message.pointer("/__openclaw/id").and_then(Value::as_str)
}

fn stop_note(stop: &str) -> Option<&'static str> {
    match stop {
        "max_tokens" => Some("The reply was cut off — it hit the length limit."),
        "max_turn_requests" => Some("Stopped after too many steps in one go."),
        "refusal" => Some("The agent declined to continue with this request."),
        _ => None,
    }
}

/// `chat` status phase shown while a tool runs ("reading…", …).
fn tool_phase(kind: Option<&str>) -> &'static str {
    match kind {
        Some("read") => "reading",
        Some("edit") => "editing",
        Some("delete") => "deleting_files",
        Some("move") => "moving_files",
        Some("search") => "searching",
        Some("execute") => "running_a_command",
        Some("think") => "thinking_it_over",
        Some("fetch") => "browsing_the_web",
        Some("switch_mode") => "switching_modes",
        _ => "using_tools",
    }
}

fn content_block_text(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => block.get("text").and_then(Value::as_str).map(str::to_string),
        "image" => Some("[image]".into()),
        "audio" => Some("[audio]".into()),
        "resource_link" => {
            let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
            let name = block.get("title").or_else(|| block.get("name")).and_then(Value::as_str).unwrap_or(uri);
            Some(if uri.is_empty() { name.to_string() } else { format!("[{name}]({uri})") })
        }
        "resource" => block
            .pointer("/resource/text")
            .or_else(|| block.pointer("/resource/uri"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Readable text for a tool result: ACP content first, rawOutput as fallback.
fn tool_output_text(content: Option<&Value>, raw_output: Option<&Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in content.and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("content") => parts.extend(item.get("content").and_then(content_block_text)),
            Some("diff") => parts.push(diff_text(item)),
            _ => {}
        }
    }
    if parts.iter().all(|p| p.trim().is_empty()) {
        parts = raw_output.and_then(raw_text).into_iter().collect();
    }
    truncate(&unfence(&parts.join("\n")), MAX_TOOL_OUTPUT)
}

/// Tool chips show results in a <pre>; drop one markdown fence around the
/// whole output (claude-agent-acp sends "```console\n…\n```").
fn unfence(text: &str) -> String {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .and_then(|body| body.split_once('\n').map(|(_lang, rest)| rest));
    match inner {
        Some(body) if !body.contains("\n```") => body.trim_end_matches('\n').to_string(),
        _ => text.to_string(),
    }
}

fn raw_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let texts: Vec<String> = items.iter().filter_map(|i| content_block_text(i).or_else(|| raw_text(i))).collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        Value::Object(map) => {
            for key in ["output", "formatted_output", "aggregated_output", "stdout", "content", "text", "result", "error", "message"] {
                if let Some(v) = map.get(key) {
                    if let Some(s) = v.as_str() {
                        let mut out = s.to_string();
                        if let Some(err) = map.get("stderr").and_then(Value::as_str).filter(|e| !e.trim().is_empty()) {
                            out.push('\n');
                            out.push_str(err);
                        }
                        return Some(out);
                    }
                    if v.is_array() {
                        if let Some(t) = raw_text(v) {
                            return Some(t);
                        }
                    }
                }
            }
            serde_json::to_string_pretty(value).ok()
        }
        other => Some(other.to_string()),
    }
}

/// A compact before/after for an ACP `diff` (common head/tail lines elided).
fn diff_text(item: &Value) -> String {
    let path = item.get("path").and_then(Value::as_str).unwrap_or("a file");
    let new = item.get("newText").and_then(Value::as_str).unwrap_or_default();
    let Some(old) = item.get("oldText").and_then(Value::as_str) else {
        return format!("Created {path}\n{}", truncate(new, 4000));
    };
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let mut out = format!("Edited {path}");
    for line in &a[head..a.len() - tail] {
        out.push_str("\n- ");
        out.push_str(line);
    }
    for line in &b[head..b.len() - tail] {
        out.push_str("\n+ ");
        out.push_str(line);
    }
    truncate(&out, 4000)
}

// ============================================================================
// Approvals
// ============================================================================

/// What the approval card shows: the command for shell tools, else the title.
fn approval_command(tool: &Value) -> String {
    if let Some(command) = tool.pointer("/rawInput/command") {
        match command {
            Value::String(s) if !s.trim().is_empty() => return s.clone(),
            Value::Array(parts) => {
                let joined: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                if !joined.is_empty() {
                    return joined.join(" ");
                }
            }
            _ => {}
        }
    }
    tool.get("title")
        .and_then(Value::as_str)
        .map(|t| t.trim().trim_matches('`').trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| tool.get("kind").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "a tool".into())
}

fn option_kind(option: &Value) -> &str {
    option.get("kind").and_then(Value::as_str).unwrap_or_default()
}

/// The approval buttons to show for the agent's permission options.
fn allowed_decisions(options: &[Value]) -> Vec<&'static str> {
    let has = |k: &str| options.iter().any(|o| option_kind(o) == k);
    let mut out = Vec::new();
    if has("allow_once") || (!has("allow_always") && options.iter().any(|o| option_kind(o).starts_with("allow"))) {
        out.push("allow-once");
    }
    if has("allow_always") {
        out.push("allow-always");
    }
    out.push("deny");
    out
}

/// The agent's option id for a littleguys decision.
fn pick_option(options: &[Value], decision: &str) -> Option<String> {
    let id = |o: &Value| o.get("optionId").and_then(Value::as_str).map(str::to_string);
    let by_kind = |k: &str| options.iter().find(|o| option_kind(o) == k).and_then(id);
    match decision {
        "allow-once" => by_kind("allow_once").or_else(|| {
            options
                .iter()
                .find(|o| option_kind(o).starts_with("allow") && option_kind(o) != "allow_always")
                .and_then(id)
        }),
        "allow-always" => by_kind("allow_always"),
        "deny" => by_kind("reject_once").or_else(|| by_kind("reject_always")),
        _ => None,
    }
}

// ============================================================================
// Routines
// ============================================================================

struct Runner(Weak<Inner>);

impl RoutineRunner for Runner {
    fn run_turn(&self, agent_id: String, target: String, job_id: String, message: String) -> BoxFuture<'static, Result<TurnOutcome, String>> {
        let inner = self.0.clone();
        Box::pin(async move {
            let inner = inner.upgrade().ok_or("littleguys is shutting down")?;
            if agent_id != AGENT_ID {
                return Err(format!("no guy {agent_id} here"));
            }
            let key = match target.as_str() {
                "" | "isolated" => {
                    let conn = inner.require_conn().map_err(|e| e.describe())?;
                    let sid = inner.new_session(&conn, &inner.cwd_string()).await.map_err(|e| inner.rpc_error(&e).message)?;
                    let key = keys::routine_run(AGENT_ID, &job_id, &sid);
                    let job_name = inner
                        .scheduler
                        .handle("cron.get", json!({ "id": job_id }))
                        .await
                        .ok()
                        .and_then(|j| j.get("name").and_then(Value::as_str).map(str::to_string));
                    let now = now_ms();
                    let mut store = inner.store.lock().unwrap();
                    store.index.sessions.push(SessionMeta {
                        key: key.clone(),
                        session_id: Some(sid),
                        label: job_name,
                        created_at: now,
                        updated_at: now,
                        ..Default::default()
                    });
                    store.save();
                    key
                }
                "main" => keys::main(AGENT_ID),
                other => match other.strip_prefix("session:") {
                    Some(key) if keys::split(key).is_some_and(|(a, _)| a == AGENT_ID) => key.to_string(),
                    _ => return Err(format!("unknown routine target {other}")),
                },
            };
            let run_id = uuid::Uuid::new_v4().to_string();
            let input = PromptInput {
                prompt: vec![json!({ "type": "text", "text": message })],
                shown: vec![json!({ "type": "text", "text": message })],
                text: message,
                cwd: None,
            };
            let result = inner.run_turn(key.clone(), run_id, input).await?;
            Ok(TurnOutcome { session_key: key, summary: result.final_text.map(|t| truncate(&clean_preview(&t), 300)) })
        })
    }

    fn emit(&self, name: &str, payload: Value) {
        if let Some(inner) = self.0.upgrade() {
            inner.emit(name, payload);
        }
    }
}

// ============================================================================
// Local store: thread index + transcripts
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMeta {
    key: String,
    /// Current ACP session id (None: main thread not started yet).
    #[serde(default)]
    session_id: Option<String>,
    /// Earlier ACP session ids of this thread (replaced when they couldn't be resumed).
    #[serde(default)]
    aliases: Vec<String>,
    /// Agent-given title, else the first user message.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    title_from_agent: bool,
    /// User-given name.
    #[serde(default)]
    label: Option<String>,
    /// The thread's own folder (absolute); None: the guy's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    updated_at: u64,
    #[serde(default)]
    last_preview: Option<String>,
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    archived: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sessions: Vec<SessionMeta>,
    /// Deleted ACP session ids, kept out of the agent's `session/list`.
    #[serde(default)]
    hidden: Vec<String>,
}

struct Store {
    dir: PathBuf,
    index: Index,
}

impl Store {
    fn open(dir: PathBuf) -> Self {
        let index = std::fs::read_to_string(dir.join("index.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { dir, index }
    }

    fn save(&mut self) {
        self.index.version = 1;
        match serde_json::to_vec_pretty(&self.index) {
            Ok(bytes) => {
                if let Err(e) = write_private(&self.dir.join("index.json"), &bytes) {
                    log::warn!("could not save ACP thread index: {e}");
                }
            }
            Err(e) => log::warn!("could not encode ACP thread index: {e}"),
        }
    }

    fn get(&self, key: &str) -> Option<&SessionMeta> {
        self.index.sessions.iter().find(|m| m.key == key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut SessionMeta> {
        self.index.sessions.iter_mut().find(|m| m.key == key)
    }

    /// Update (creating if needed) and save.
    fn update(&mut self, key: &str, f: impl FnOnce(&mut SessionMeta)) {
        if self.get(key).is_none() {
            let now = now_ms();
            self.index.sessions.push(SessionMeta {
                key: key.to_string(),
                session_id: implied_session_id(key),
                created_at: now,
                updated_at: now,
                ..Default::default()
            });
        }
        if let Some(meta) = self.get_mut(key) {
            f(meta);
        }
        self.save();
    }

    fn remove(&mut self, key: &str) -> Option<SessionMeta> {
        let pos = self.index.sessions.iter().position(|m| m.key == key)?;
        let meta = self.index.sessions.remove(pos);
        self.save();
        Some(meta)
    }

    fn key_for_session(&self, sid: &str) -> Option<String> {
        self.index
            .sessions
            .iter()
            .find(|m| m.session_id.as_deref() == Some(sid) || m.aliases.iter().any(|a| a == sid))
            .map(|m| m.key.clone())
    }

    fn transcript_path(&self, key: &str) -> PathBuf {
        self.dir.join("transcripts").join(transcript_name(key))
    }

    fn has_transcript(&self, key: &str) -> bool {
        self.transcript_path(key).exists()
    }

    fn has_messages(&self, key: &str) -> bool {
        std::fs::metadata(self.transcript_path(key)).map(|m| m.len() > 0).unwrap_or(false)
    }

    fn append(&self, key: &str, message: &Value) {
        self.append_all(key, std::slice::from_ref(message));
    }

    /// Append messages (creating the transcript even when there are none).
    fn append_all(&self, key: &str, messages: &[Value]) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.transcript_path(key);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path);
        let mut file = match file {
            Ok(f) => f,
            Err(e) => {
                log::warn!("could not open {}: {e}", path.display());
                return;
            }
        };
        let mut buf = String::new();
        for m in messages {
            buf.push_str(&m.to_string());
            buf.push('\n');
        }
        if let Err(e) = file.write_all(buf.as_bytes()) {
            log::warn!("could not write {}: {e}", path.display());
        }
    }

    fn read(&self, key: &str) -> Vec<Value> {
        std::fs::read_to_string(self.transcript_path(key))
            .map(|raw| raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default()
    }

    /// Replace the message with the same `__openclaw.id`.
    fn replace(&self, key: &str, message: &Value) {
        let Some(id) = message_id(message) else { return };
        let mut messages = self.read(key);
        let Some(slot) = messages.iter_mut().find(|m| message_id(m) == Some(id)) else { return };
        *slot = message.clone();
        let mut buf = String::new();
        for m in &messages {
            buf.push_str(&m.to_string());
            buf.push('\n');
        }
        if let Err(e) = write_private(&self.transcript_path(key), buf.as_bytes()) {
            log::warn!("could not rewrite transcript: {e}");
        }
    }

    fn delete_transcript(&self, key: &str) {
        let _ = std::fs::remove_file(self.transcript_path(key));
    }
}

/// Readable, collision-free file name for a session key.
fn transcript_name(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(key.as_bytes());
    let short: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .take(80)
        .collect();
    format!("{safe}-{short}.jsonl")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

fn clean_preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ============================================================================
// ACP registry
// ============================================================================

/// A launchable agent from the public ACP registry.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub website: Option<String>,
    pub icon: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
}

pub const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// Agents from the ACP registry that littleguys can launch: `npx`/`uvx`
/// packages, plus binary-only agents already installed on this Mac.
pub async fn fetch_registry() -> Result<Vec<RegistryAgent>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("littleguys/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(REGISTRY_URL).send().await.map_err(|e| format!("Couldn't reach the ACP registry: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("The ACP registry answered {}", response.status()));
    }
    let raw: Value = response.json().await.map_err(|e| format!("The ACP registry sent something unexpected: {e}"))?;
    Ok(parse_registry(&raw, &|cmd| resolve_program(cmd).is_some()))
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn string_map(v: Option<&Value>) -> std::collections::BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default()
}

fn parse_registry(raw: &Value, installed: &dyn Fn(&str) -> bool) -> Vec<RegistryAgent> {
    let platform = format!("darwin-{}", std::env::consts::ARCH);
    let mut out = Vec::new();
    for agent in raw.get("agents").and_then(Value::as_array).into_iter().flatten() {
        let field = |k: &str| agent.get(k).and_then(Value::as_str).map(str::to_string);
        let (Some(id), Some(name)) = (field("id"), field("name")) else { continue };
        let dist = agent.get("distribution").unwrap_or(&Value::Null);
        let launch = if let Some(npx) = dist.get("npx") {
            npx.get("package").and_then(Value::as_str).map(|pkg| {
                let mut args = vec!["-y".to_string(), pkg.to_string()];
                args.extend(string_list(npx.get("args")));
                ("npx".to_string(), args, string_map(npx.get("env")))
            })
        } else if let Some(uvx) = dist.get("uvx") {
            uvx.get("package").and_then(Value::as_str).map(|pkg| {
                let mut args = vec![pkg.to_string()];
                args.extend(string_list(uvx.get("args")));
                ("uvx".to_string(), args, string_map(uvx.get("env")))
            })
        } else if let Some(bin) = dist.pointer(&format!("/binary/{platform}")) {
            // Archives need downloading; only offer ones already on PATH.
            bin.get("cmd")
                .and_then(Value::as_str)
                .and_then(|cmd| Path::new(cmd).file_name().and_then(|f| f.to_str()).map(str::to_string))
                .filter(|cmd| installed(cmd))
                .map(|cmd| (cmd, string_list(bin.get("args")), string_map(bin.get("env"))))
        } else {
            None
        };
        let Some((command, args, env)) = launch else { continue };
        out.push(RegistryAgent {
            id,
            name,
            description: field("description").unwrap_or_default(),
            version: field("version").unwrap_or_default(),
            website: field("website").or_else(|| field("repository")),
            icon: field("icon"),
            command,
            args,
            env,
        });
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(fx: &[Effect]) -> Vec<Value> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn events<'a>(fx: &'a [Effect], name: &str) -> Vec<&'a Value> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Event(n, p) if *n == name => Some(p),
                _ => None,
            })
            .collect()
    }

    fn chunk(kind: &str, text: &str) -> Value {
        json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } })
    }

    #[test]
    fn text_chunks_stream_cumulative_deltas_and_finalize_one_message() {
        let mut b = TurnBuilder::live("agent:bot:main", "run1");
        let fx1 = b.apply(&chunk("agent_message_chunk", "Hel"));
        let fx2 = b.apply(&chunk("agent_message_chunk", "lo"));
        let d1 = events(&fx1, "chat");
        let d2 = events(&fx2, "chat");
        assert_eq!(d1[0]["state"], "delta");
        assert_eq!(d1[0]["message"]["content"][0]["text"], "Hel");
        assert_eq!(d2[0]["message"]["content"][0]["text"], "Hello");
        assert_eq!(d2[0]["deltaText"], "lo");
        assert!(messages(&fx1).is_empty() && messages(&fx2).is_empty());
        let done = b.finish(false);
        let msgs = messages(&done);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], json!([{ "type": "text", "text": "Hello" }]));
        assert_eq!(msgs[0]["__openclaw"]["runId"], "run1");
        assert!(msgs[0]["__openclaw"]["id"].as_str().is_some());
        assert_eq!(b.last_text.as_deref(), Some("Hello"));
    }

    #[test]
    fn tool_calls_follow_the_openclaw_shape() {
        let mut b = TurnBuilder::live("agent:bot:main", "r");
        let mut fx = Vec::new();
        fx.extend(b.apply(&chunk("agent_thought_chunk", "Let me look.")));
        fx.extend(b.apply(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "`ls`", "kind": "execute",
            "status": "pending", "rawInput": {}, "_meta": { "claudeCode": { "toolName": "Bash" } }
        })));
        // Input arrives later (claude-agent-acp refines it).
        fx.extend(b.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "rawInput": { "command": "ls" } })));
        assert!(messages(&fx).is_empty(), "nothing final until the tool runs");
        fx.extend(b.apply(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "a.txt\nb.txt" } }]
        })));
        fx.extend(b.apply(&chunk("agent_message_chunk", "Two files.")));
        fx.extend(b.finish(false));
        let msgs = messages(&fx);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"][0], json!({ "type": "thinking", "thinking": "Let me look." }));
        assert_eq!(msgs[0]["content"][1]["type"], "toolCall");
        assert_eq!(msgs[0]["content"][1]["id"], "t1");
        assert_eq!(msgs[0]["content"][1]["name"], "Bash");
        assert_eq!(msgs[0]["content"][1]["arguments"], json!({ "command": "ls" }));
        assert_eq!(msgs[1]["role"], "toolResult");
        assert_eq!(msgs[1]["toolCallId"], "t1");
        assert_eq!(msgs[1]["isError"], false);
        assert_eq!(msgs[1]["content"][0]["text"], "a.txt\nb.txt");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], json!([{ "type": "text", "text": "Two files." }]));
        let agent = events(&fx, "agent");
        assert_eq!(agent.len(), 2);
        assert_eq!(agent[0]["stream"], "tool");
        assert_eq!(agent[0]["data"]["phase"], "start");
        assert_eq!(agent[0]["data"]["toolCallId"], "t1");
        assert_eq!(agent[1]["data"]["phase"], "end");
        assert_eq!(agent[1]["data"]["isError"], false);
        // The tool start also sets a friendly status.
        assert!(events(&fx, "chat").iter().any(|e| e["state"] == "status" && e["phase"] == "running_a_command"));
    }

    #[test]
    fn failed_and_unfinished_tools_get_error_results() {
        let mut b = TurnBuilder::live("k", "r");
        let mut fx = Vec::new();
        fx.extend(b.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "title": "Read x", "kind": "read", "rawInput": { "path": "/x" } })));
        fx.extend(b.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "failed", "rawOutput": { "error": "nope" } })));
        fx.extend(b.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "b", "title": "Run", "kind": "execute", "status": "in_progress", "rawInput": { "command": "sleep 9" } })));
        fx.extend(b.finish(true));
        let msgs = messages(&fx);
        let results: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "toolResult").collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolCallId"], "a");
        assert_eq!(results[0]["isError"], true);
        assert!(results[0]["content"][0]["text"].as_str().unwrap().contains("nope"));
        assert_eq!(results[1]["toolCallId"], "b");
        assert_eq!(results[1]["isError"], true);
        // b's call was emitted (in_progress) before its result.
        let b_call = msgs.iter().position(|m| m["content"][0]["id"] == "b").unwrap();
        let b_result = msgs.iter().position(|m| m["toolCallId"] == "b").unwrap();
        assert!(b_call < b_result);
    }

    #[test]
    fn late_tool_input_patches_the_emitted_message() {
        let mut b = TurnBuilder::live("k", "r");
        b.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Edit f", "kind": "edit", "status": "in_progress" }));
        let fx = b.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "rawInput": { "file_path": "/f" } }));
        let patch = fx.iter().find_map(|e| match e {
            Effect::Patch(m) => Some(m.clone()),
            _ => None,
        });
        let patch = patch.expect("a patch");
        assert_eq!(patch["content"][0]["arguments"], json!({ "file_path": "/f" }));
    }

    #[test]
    fn permission_requests_surface_the_tool_call() {
        let mut b = TurnBuilder::live("k", "r");
        b.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "`rm -rf x`", "kind": "execute", "status": "pending" }));
        let fx = b.permission(&json!({ "toolCallId": "t", "status": "pending", "rawInput": { "command": "rm -rf x" } }));
        let msgs = messages(&fx);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"][0]["arguments"]["command"], "rm -rf x");
        assert_eq!(b.tools["t"].status(), "pending");
    }

    #[test]
    fn replays_rebuild_user_and_assistant_turns() {
        let mut c = Collector::new("k");
        c.apply(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "hi" } }));
        c.apply(&chunk("agent_message_chunk", "hello"));
        c.apply(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "list files" } }));
        c.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "ls", "kind": "execute", "rawInput": { "command": "ls" }, "status": "completed", "rawOutput": "a\nb" }));
        c.apply(&chunk("agent_message_chunk", "a and b"));
        let msgs = c.finish();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "toolResult", "assistant"]);
        assert_eq!(msgs[0]["content"][0]["text"], "hi");
        assert_eq!(msgs[4]["content"][0]["text"], "a\nb");
        assert!(msgs.iter().all(|m| m["__openclaw"].get("runId").is_none()));
    }

    #[test]
    fn live_turns_ignore_echoed_user_chunks_and_split_on_message_id() {
        let mut b = TurnBuilder::live("k", "r");
        assert!(b.apply(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "x" } })).is_empty());
        let mut fx = Vec::new();
        fx.extend(b.apply(&json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": { "type": "text", "text": "one" } })));
        fx.extend(b.apply(&json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m2", "content": { "type": "text", "text": "two" } })));
        fx.extend(b.finish(false));
        let msgs = messages(&fx);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"][0]["text"], "two");
    }

    #[test]
    fn permission_options_map_to_decisions() {
        let options = vec![
            json!({ "optionId": "yes", "name": "Allow", "kind": "allow_once" }),
            json!({ "optionId": "always", "name": "Always", "kind": "allow_always" }),
            json!({ "optionId": "no", "name": "Reject", "kind": "reject_once" }),
        ];
        assert_eq!(allowed_decisions(&options), ["allow-once", "allow-always", "deny"]);
        assert_eq!(pick_option(&options, "allow-once").as_deref(), Some("yes"));
        assert_eq!(pick_option(&options, "allow-always").as_deref(), Some("always"));
        assert_eq!(pick_option(&options, "deny").as_deref(), Some("no"));
        assert_eq!(pick_option(&options, "maybe"), None);

        let only_once = vec![json!({ "optionId": "ok", "kind": "allow_once" })];
        assert_eq!(allowed_decisions(&only_once), ["allow-once", "deny"]);
        assert_eq!(pick_option(&only_once, "allow-always"), None);
        assert_eq!(pick_option(&only_once, "deny"), None); // → "cancelled" outcome

        let reject_always = vec![json!({ "optionId": "never", "kind": "reject_always" })];
        assert_eq!(pick_option(&reject_always, "deny").as_deref(), Some("never"));
    }

    #[test]
    fn approval_text_prefers_the_command() {
        assert_eq!(approval_command(&json!({ "title": "`ls -la`", "rawInput": { "command": "ls -la" } })), "ls -la");
        assert_eq!(approval_command(&json!({ "title": "x", "rawInput": { "command": ["git", "status"] } })), "git status");
        assert_eq!(approval_command(&json!({ "title": "`Write /tmp/a`" })), "Write /tmp/a");
        assert_eq!(approval_command(&json!({})), "a tool");
    }

    #[test]
    fn keys_round_trip_to_session_ids() {
        let key = keys::thread(AGENT_ID, "sess:42");
        assert_eq!(implied_session_id(&key).as_deref(), Some("sess:42"));
        let run = keys::routine_run(AGENT_ID, "job:1", "abc");
        assert_eq!(implied_session_id(&run).as_deref(), Some("abc"));
        assert_eq!(implied_session_id(&keys::main(AGENT_ID)), None);
        assert_eq!(implied_session_id("agent:other:dashboard:x"), None);
        assert!(session_key(&json!({ "sessionKey": "agent:bot:main" })).is_ok());
        assert!(session_key(&json!({ "sessionKey": "agent:main:main" })).is_err());
        assert_ne!(transcript_name("agent:bot:dashboard:a%3Ab"), transcript_name("agent:bot:dashboard:a_b"));
    }

    #[test]
    fn tool_output_text_reads_content_diffs_and_raw_output() {
        let content = json!([{ "type": "diff", "path": "/a.txt", "oldText": "x\ny\nz", "newText": "x\nY\nz" }]);
        assert_eq!(tool_output_text(Some(&content), None), "Edited /a.txt\n- y\n+ Y");
        assert_eq!(tool_output_text(None, Some(&json!({ "stdout": "hi", "stderr": "warn" }))), "hi\nwarn");
        assert_eq!(tool_output_text(None, Some(&json!([{ "type": "text", "text": "t" }]))), "t");
        assert_eq!(tool_output_text(None, Some(&json!({ "error": "denied" }))), "denied");
        assert_eq!(tool_output_text(None, None), "");
        let fenced = json!([{ "type": "content", "content": { "type": "text", "text": "```console\na.txt\nb.txt\n```" } }]);
        assert_eq!(tool_output_text(Some(&fenced), None), "a.txt\nb.txt");
        assert_eq!(unfence("```\n```"), "");
        assert_eq!(strip_ansi("\u{1b}[31mError:\u{1b}[0m boom"), "Error: boom");
        assert_eq!(unfence("```js\na\n```\ntext\n```py\nb\n```"), "```js\na\n```\ntext\n```py\nb\n```");
    }

    #[test]
    fn prompts_carry_images_and_files() {
        let att = vec![
            json!({ "type": "image", "mimeType": "image/png", "fileName": "a.png", "content": "AAAA" }),
            json!({ "type": "file", "mimeType": "text/plain", "fileName": "n.txt", "content": base64::engine::general_purpose::STANDARD.encode("hello") }),
        ];
        let (prompt, shown) = build_prompt("look", &att, true, true, "Claude").unwrap();
        assert_eq!(prompt[0], json!({ "type": "text", "text": "look" }));
        assert_eq!(prompt[1], json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" }));
        assert_eq!(prompt[2]["resource"]["text"], "hello");
        assert_eq!(shown.len(), 3);
        assert!(build_prompt("", &att, false, true, "X").is_err());
    }

    #[test]
    fn registry_entries_become_launch_commands() {
        let raw = json!({ "agents": [
            { "id": "claude-acp", "name": "Claude Agent", "version": "1", "description": "d",
              "distribution": { "npx": { "package": "@agentclientprotocol/claude-agent-acp@1" } } },
            { "id": "gemini", "name": "Gemini", "version": "2", "description": "g", "website": "https://g",
              "distribution": { "npx": { "package": "@google/gemini-cli@2", "args": ["--acp"], "env": { "A": "1" } } } },
            { "id": "fast", "name": "fast", "version": "3", "description": "f",
              "distribution": { "uvx": { "package": "fast-agent-acp==3", "args": ["-x"] } } },
            { "id": "opencode", "name": "OpenCode", "version": "4", "description": "o",
              "distribution": { "binary": {
                  "darwin-aarch64": { "archive": "x", "cmd": "./opencode", "args": ["acp"] },
                  "darwin-x86_64": { "archive": "x", "cmd": "./opencode", "args": ["acp"] } } } },
            { "id": "amp", "name": "Amp", "version": "5", "description": "a",
              "distribution": { "binary": {
                  "darwin-aarch64": { "archive": "x", "cmd": "./amp-acp" },
                  "darwin-x86_64": { "archive": "x", "cmd": "./amp-acp" } } } }
        ]});
        let agents = parse_registry(&raw, &|cmd| cmd == "opencode");
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["claude-acp", "gemini", "fast", "opencode"]);
        assert_eq!(agents[0].command, "npx");
        assert_eq!(agents[0].args, ["-y", "@agentclientprotocol/claude-agent-acp@1"]);
        assert_eq!(agents[1].args, ["-y", "@google/gemini-cli@2", "--acp"]);
        assert_eq!(agents[1].env.get("A").map(String::as_str), Some("1"));
        assert_eq!(agents[1].website.as_deref(), Some("https://g"));
        assert_eq!(agents[2].command, "uvx");
        assert_eq!(agents[2].args, ["fast-agent-acp==3", "-x"]);
        assert_eq!(agents[3].command, "opencode");
        assert_eq!(agents[3].args, ["acp"]);
        let json = serde_json::to_value(&agents[0]).unwrap();
        assert!(json.get("command").is_some() && json["website"].is_null());
    }

    #[test]
    fn errors_map_to_contract_codes() {
        let e = AcpError::from_wire(&json!({ "code": -32603, "message": "Internal error", "data": { "details": "boom" } }));
        assert_eq!(e.describe(), "Internal error: boom");
        assert_eq!(e.to_rpc().code, "AGENT_ERROR");
        assert_eq!(AcpError::from_wire(&json!({ "code": -32000, "message": "Authentication required" })).to_rpc().code, "AUTH_REQUIRED");
        assert_eq!(AcpError::disconnected().to_rpc().code, "DISCONNECTED");
    }

    // ---- the agent's folders ----------------------------------------------------------

    /// A scratch folder, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("littleguys-acp-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn front_matter_reads_names_and_descriptions() {
        let plain = "---\nname: pdf\ndescription: Read and write PDFs # a comment\nlicense: MIT\n---\n# body";
        assert_eq!(front_matter(plain), (Some("pdf".into()), Some("Read and write PDFs".into())));
        let quoted = "\u{feff}---\nname: \"graph-it\"\ndescription: \"Say \\\"hi\\\": now\"\n---\n";
        assert_eq!(front_matter(quoted), (Some("graph-it".into()), Some("Say \"hi\": now".into())));
        let single = "---\nname: 'it''s'\ndescription: 'a: b'\n---";
        assert_eq!(front_matter(single), (Some("it's".into()), Some("a: b".into())));
        let folded = "---\nname: x\ndescription: >-\n  First line\n  continues.\n\n  New paragraph.\nmetadata:\n  name: nested\n---";
        assert_eq!(front_matter(folded), (Some("x".into()), Some("First line continues.\nNew paragraph.".into())));
        let literal = "---\ndescription: |\n  one\n  two\nname: y\n---";
        assert_eq!(front_matter(literal), (Some("y".into()), Some("one\ntwo".into())));
        let wrapped = "---\nname: z\ndescription: a long\n  wrapped line\n---";
        assert_eq!(front_matter(wrapped).1.as_deref(), Some("a long wrapped line"));
        assert_eq!(front_matter("# no front matter\nname: nope"), (None, None));
        assert_eq!(front_matter("---\nname:\n---"), (None, None));
    }

    #[test]
    fn commands_become_command_entries() {
        let commands = vec![
            json!({ "name": "review", "description": "Review the diff", "input": { "hint": "branch or PR" } }),
            json!({ "name": "/compact", "description": "Compact the chat", "input": null }),
            json!({ "name": "pdf", "description": "Work with PDFs", "input": { "hint": "" } }),
            json!({ "name": "review", "description": "dupe" }),
        ];
        let skills: HashSet<String> = ["pdf".to_string()].into();
        let entries = command_entries(&commands, &skills);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            json!({ "name": "review", "description": "Review the diff", "source": "native", "scope": "text", "acceptsArgs": true,
                    "args": [{ "name": "branch or PR", "description": "", "type": "string" }] })
        );
        assert_eq!(entries[1]["name"], "compact");
        assert_eq!(entries[1]["acceptsArgs"], false);
        assert_eq!(entries[2]["source"], "skill");
        assert!(entries[2].get("args").is_none());
    }

    #[test]
    fn commands_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("lg-acp-cmds-{}", uuid::Uuid::new_v4()));
        assert!(load_commands(&dir).is_empty());
        let commands = vec![json!({ "name": "review", "description": "Review the diff" })];
        save_commands(&dir, &commands);
        assert_eq!(load_commands(&dir), commands);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_become_skill_rows() {
        let commands = vec![
            json!({ "name": "review", "description": "Review the diff", "input": { "hint": "branch or PR" } }),
            json!({ "name": "/init", "description": "Write AGENTS.md" }),
            json!({ "name": "review", "description": "dupe" }),
            json!({ "description": "no name" }),
        ];
        let rows = command_skills(&commands);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            json!({ "name": "review", "skillKey": "cmd:review", "description": "Review the diff", "source": "command",
                    "eligible": true, "disabled": false, "hint": "branch or PR" })
        );
        assert_eq!(rows[1]["name"], "init");
        assert!(rows[1].get("hint").is_none());
    }

    #[test]
    fn flavors_pick_their_skill_folders() {
        assert_eq!(Flavor::detect("npx -y @agentclientprotocol/claude-agent-acp@1 claude-acp"), Flavor::Claude);
        assert_eq!(Flavor::detect("/opt/homebrew/bin/codex-acp"), Flavor::Codex);
        assert_eq!(Flavor::detect("npx @google/Gemini-cli --acp"), Flavor::Gemini);
        assert_eq!(Flavor::detect("opencode acp"), Flavor::Other);
        let (cwd, home) = (Path::new("/w"), Path::new("/h"));
        let dirs: Vec<(String, &str)> =
            skill_dirs(Flavor::Claude, cwd, home).into_iter().map(|(p, s)| (p.display().to_string(), s)).collect();
        assert_eq!(
            dirs,
            [
                ("/w/.claude/skills".to_string(), "project"),
                ("/w/.agents/skills".to_string(), "project"),
                ("/h/.claude/skills".to_string(), "personal"),
                ("/h/.agents/skills".to_string(), "personal"),
            ]
        );
        assert_eq!(skill_dirs(Flavor::Codex, cwd, home)[2].0, Path::new("/h/.codex/skills"));
        assert_eq!(skill_dirs(Flavor::Other, cwd, home).len(), 2);
    }

    #[test]
    fn skills_are_found_one_or_two_levels_deep() {
        let project = Scratch::new();
        let personal = Scratch::new();
        project.write("pdf/SKILL.md", "---\nname: pdf\ndescription: project pdf\n---\n");
        project.write("no-front-matter/SKILL.md", "# just text");
        project.write(".hidden/SKILL.md", "---\nname: hidden\n---");
        personal.write("pdf/SKILL.md", "---\nname: pdf\ndescription: personal pdf\n---\n");
        personal.write("group/deep/SKILL.md", "---\nname: deep-one\ndescription: nested\n---\n");
        personal.write("group/notes.txt", "x");
        personal.write("empty/readme.md", "x");
        let skills = scan_skills(&[(project.0.clone(), "project"), (personal.0.clone(), "personal")]);
        let names: Vec<(&str, &str)> = skills.iter().map(|s| (s.name.as_str(), s.source)).collect();
        assert_eq!(names, [("no-front-matter", "project"), ("pdf", "project"), ("deep-one", "personal")]);
        let row = skills[1].row();
        assert_eq!(row["skillKey"], "pdf");
        assert_eq!(row["description"], "project pdf");
        assert_eq!(row["eligible"], true);
        assert!(row["filePath"].as_str().unwrap().ends_with("pdf/SKILL.md"));
        assert!(scan_skills(&[(project.0.join("missing"), "project")]).is_empty());
    }

    #[test]
    fn claude_mcp_servers_never_leak_secrets() {
        let claude_json = json!({
            "mcpServers": {
                "linear": { "type": "http", "url": "https://user:pw@mcp.linear.app/mcp?token=abc#frag",
                            "headers": { "Authorization": "Bearer SECRET" } },
                "shared": { "command": "npx", "args": ["-y", "pkg", "--key=SECRET"] }
            },
            "projects": {
                "/w": {
                    "mcpServers": { "local-db": { "command": "/usr/local/bin/pg-mcp", "env": { "PGPASSWORD": "SECRET" } } },
                    "disabledMcpjsonServers": ["off"],
                    "enabledMcpjsonServers": ["shared"],
                    "disabledMcpServers": ["linear"]
                }
            }
        });
        let mcp_json = json!({ "mcpServers": {
            "off": { "type": "sse", "url": "https://x.dev/sse" },
            "shared": { "command": "API_KEY=SECRET uvx", "args": ["thing"] }
        }});
        let servers = claude_mcp(Some(&claude_json), Some(&mcp_json), &["/w".to_string()]);
        let text = serde_json::to_string(&servers).unwrap();
        assert!(!text.contains("SECRET") && !text.contains("pw") && !text.contains("token") && !text.contains("frag"), "{text}");
        assert_eq!(servers[0], json!({ "name": "local-db", "transport": "stdio", "command": "pg-mcp", "source": "local" }));
        assert_eq!(servers[1], json!({ "name": "off", "transport": "sse", "url": "https://x.dev/sse", "source": "project", "enabled": false }));
        assert_eq!(servers[2], json!({ "name": "shared", "transport": "stdio", "command": "uvx", "source": "project", "enabled": true }));
        assert_eq!(
            servers[3],
            json!({ "name": "linear", "transport": "http", "url": "https://mcp.linear.app/mcp", "source": "user", "enabled": false })
        );
        assert_eq!(servers[4]["command"], "npx");
        assert!(claude_mcp(None, None, &[]).is_empty());
    }

    #[test]
    fn codex_and_gemini_mcp_servers() {
        // An installed program whose path has spaces.
        let apps = Scratch::new();
        apps.write("Some App.app/bin/node_repl", "#!/bin/sh");
        let program = apps.0.join("Some App.app/bin/node_repl");
        let toml = r#"
model = "o3"
[mcp_servers.node_repl]
command = "PROGRAM"
args = ["--secret", "SECRET"]
[mcp_servers.node_repl.env]
TOKEN = "SECRET"
[mcp_servers.docs]
url = "https://developers.openai.com/mcp"
bearer_token_env_var = "SECRET"
[mcp_servers.off]
command = "thing"
enabled = false
"#
        .replace("PROGRAM", &program.display().to_string());
        let servers = codex_mcp(&toml, "user");
        assert!(!serde_json::to_string(&servers).unwrap().contains("SECRET"));
        assert_eq!(servers.len(), 3);
        let by_name = |n: &str| servers.iter().find(|s| s["name"] == n).unwrap().clone();
        assert_eq!(by_name("node_repl")["command"], "node_repl");
        assert_eq!(by_name("docs"), json!({ "name": "docs", "transport": "http", "url": "https://developers.openai.com/mcp", "source": "user" }));
        assert_eq!(by_name("off")["enabled"], false);
        assert!(codex_mcp("not [valid toml", "user").is_empty());

        let settings = json!({
            "mcpServers": {
                "sse": { "url": "http://localhost:8080/sse" },
                "streamable": { "httpUrl": "https://api.example.com/mcp/AbCdEf0123456789AbCdEf0123", "headers": { "X-Key": "SECRET" } },
                "local": { "command": "python3", "args": ["server.py"], "env": { "K": "SECRET" } }
            },
            "mcp": { "excluded": ["local"] }
        });
        let servers = gemini_mcp(&settings, "user");
        assert!(!serde_json::to_string(&servers).unwrap().contains("SECRET"));
        let by_name = |n: &str| servers.iter().find(|s| s["name"] == n).unwrap().clone();
        assert_eq!(by_name("sse")["transport"], "sse");
        assert_eq!(by_name("streamable")["transport"], "http");
        assert_eq!(by_name("streamable")["url"], "https://api.example.com/mcp/***");
        assert_eq!(by_name("local")["command"], "python3");
        assert_eq!(by_name("local")["enabled"], false);
    }

    #[test]
    fn program_names_and_urls_are_scrubbed() {
        assert_eq!(program_name("npx").as_deref(), Some("npx"));
        assert_eq!(program_name("/opt/homebrew/bin/uvx --from x").as_deref(), Some("uvx"));
        assert_eq!(program_name("TOKEN=abc node").as_deref(), Some("node"));
        assert_eq!(program_name("./Codex Computer Use.app/Contents/MacOS/Client").as_deref(), Some("Client"));
        assert_eq!(program_name("TOKEN=abc /usr/bin/node").as_deref(), Some("node"));
        assert_eq!(program_name("   "), None);
        assert_eq!(safe_url("wss://a:b@host:9/x?y=1").as_deref(), Some("wss://host:9/x"));
        assert_eq!(safe_url("file:///etc/passwd"), None);
        assert_eq!(safe_url("not a url"), None);
    }

    #[test]
    fn claude_plugins_follow_enabled_plugins() {
        let installed = json!({
            "version": 2,
            "plugins": {
                "frontend-design@claude-plugins-official": [{ "scope": "user", "installPath": "/p/fd", "version": "d4226d062928" }],
                "lsp@claude-plugins-official": [{ "scope": "user", "installPath": "/p/lsp", "version": "1.0.0" }],
                "elsewhere@m": [{ "scope": "project", "projectPath": "/other", "installPath": "/p/e", "version": "1" }],
                "legacy@m": { "installPath": "/p/legacy", "version": "0.1" }
            }
        });
        let user = json!({ "frontend-design@claude-plugins-official": true, "lsp@claude-plugins-official": true, "legacy@m": true });
        let project = json!({ "lsp@claude-plugins-official": false });
        let manifest = |path: &str| (path == "/p/fd").then(|| json!({ "name": "frontend-design", "description": "Make it pretty" }));
        let plugins = claude_plugins(&installed, &[&project, &user], "/w", manifest);
        assert_eq!(
            plugins,
            [
                json!({ "id": "frontend-design@claude-plugins-official", "name": "frontend-design", "version": "d4226d062928",
                        "description": "Make it pretty", "enabled": true, "state": "enabled" }),
                json!({ "id": "legacy@m", "name": "legacy", "version": "0.1", "enabled": true, "state": "enabled" }),
                json!({ "id": "lsp@claude-plugins-official", "name": "lsp", "version": "1.0.0", "enabled": false, "state": "disabled" }),
            ]
        );
        assert!(claude_plugins(&json!({}), &[], "/w", |_: &str| None).is_empty());
        let ext = gemini_plugin("dir", &json!({ "name": "gemini-ext", "version": "1.2.3", "mcpServers": {} }));
        assert_eq!(ext, json!({ "id": "gemini-ext", "name": "gemini-ext", "version": "1.2.3", "enabled": true, "state": "enabled" }));
    }
    #[test]
    fn threads_report_their_own_folder_or_the_guys() {
        let own = SessionMeta { key: "agent:bot:dashboard:a".into(), cwd: Some("/proj".into()), ..Default::default() };
        let plain = SessionMeta { key: "agent:bot:dashboard:b".into(), ..Default::default() };
        assert_eq!(session_row(&own, false, "/guy")["workspaceDir"], "/proj");
        assert_eq!(session_row(&plain, false, "/guy")["workspaceDir"], "/guy");
        assert_eq!(listed_row(&json!({ "sessionId": "c", "cwd": "/term" }), "/guy")["workspaceDir"], "/term");
        assert_eq!(listed_row(&json!({ "sessionId": "d" }), "/guy")["workspaceDir"], "/guy");
        // Threads without their own folder keep the index unchanged on disk.
        assert!(!serde_json::to_string(&plain).unwrap().contains("cwd"));
    }

    #[test]
    fn folders_resolve_from_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolve_path("~"), home);
        assert_eq!(resolve_path(" ~/x "), home.join("x"));
        assert_eq!(resolve_path("x/y"), home.join("x/y"));
        assert_eq!(resolve_path("/abs"), PathBuf::from("/abs"));
        assert_eq!(resolve_dir("/"), Some(PathBuf::from("/")));
        assert_eq!(resolve_dir("/definitely/not/here"), None);
    }

    #[test]
    fn same_folder_however_spelled() {
        assert!(same_dir("/Users/x/dev/", "/Users/x/dev"));
        assert!(same_dir("/tmp", "/private/tmp"));
        assert!(!same_dir("/tmp", "/usr"));
        assert!(!same_dir("/Users/x/dev", "/Users/x/dev2"));
    }
}
