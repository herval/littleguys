//! Claude Code: every Claude Code session on this Mac, in a terminal or in the
//! background (`claude --bg`), is a thread on one little guy (agent id `claude`).
//!
//! - Sessions come from `claude agents --json --all` (polled) and from the
//!   littleguys plugin's hooks, which report over a unix socket in littleguys'
//!   data dir (`claude.sock`) the moment a turn starts or ends, a permission
//!   prompt opens, or a session ends. The plugin lives in `src-tauri/claude-plugin`;
//!   [`install_plugin`] writes it to `<data_dir>/claude-plugin` and installs it
//!   from there (`claude plugin marketplace add` + `claude plugin install`).
//! - A thread's conversation is its session's transcript
//!   (`~/.claude/projects/<folder>/<sessionId>.jsonl`): read whole for
//!   `chat.history`, and followed while the session is live, every new line a
//!   `session.message`.
//! - `chat.send` hands a busy session your message when its turn ends (the Stop
//!   hook blocks with it). An idle background session, or one that ended, is
//!   resumed with it: `claude "<text>" --resume <id> --bg`, the same conversation.
//!   A terminal session that's idle gets it with its next prompt.
//! - With the connection's `approvals` setting on, a background session's
//!   permission prompt waits in its PermissionRequest hook for Allow or Deny from
//!   littleguys. Terminal sessions keep asking in their terminal.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify};

use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::now_ms;
use crate::gateways::ClaudeCodeConfig;
use crate::openclaw::config::{Credential, GatewayMode};
use crate::openclaw::runtime as cli;
use crate::runtime::{keys, unsupported, Capabilities, RuntimeKind};

const AGENT_ID: &str = "claude";
pub const PLUGIN_ID: &str = "littleguys@littleguys";
const SOCKET_FILE: &str = "claude.sock";
const PLUGIN_DIR: &str = "claude-plugin";
/// `ConnStatus.detail_code` when the plugin needs installing (the UI offers the button).
pub const PLUGIN_MISSING: &str = "claude-plugin";

const TICK: Duration = Duration::from_secs(1);
/// Poll `claude agents` every this many ticks.
const AGENTS_EVERY: u32 = 3;
/// Sessions show up as threads while they run, or for this long after.
const LIST_WINDOW_MS: u64 = 3 * 24 * 3600 * 1000;
/// Transcripts are followed while their session runs, or for this long after it last wrote.
const FOLLOW_WINDOW_MS: u64 = 10 * 60 * 1000;
/// A hook's word on a turn starting or ending stands for this long against `claude agents`.
const HOOK_SETTLE: Duration = Duration::from_secs(6);
/// …and for sessions with hooks, `claude agents` only overrules them after this much quiet.
const HOOKS_QUIET: Duration = Duration::from_secs(30);
const HISTORY_MAX_BYTES: u64 = 24 << 20;
const TAIL_MAX_BYTES: u64 = 8 << 20;
/// How much of a transcript's end is read for its title and last words.
const SUMMARY_BYTES: u64 = 256 << 10;
const TOOL_TEXT_MAX: usize = 20_000;
const REQUEST_MAX_BYTES: usize = 8 << 20;
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// Starts every message littleguys hands a session, so its transcript shows it as yours.
const MARK: &str = "The person wrote to you from littleguys:";

const MARKETPLACE_JSON: &str = include_str!("../../claude-plugin/.claude-plugin/marketplace.json");
const PLUGIN_JSON: &str = include_str!("../../claude-plugin/plugin/.claude-plugin/plugin.json");
const HOOKS_JSON: &str = include_str!("../../claude-plugin/plugin/hooks/hooks.json");
const HOOK_SH: &str = include_str!("../../claude-plugin/plugin/hooks/hook.sh");

type Emits = Vec<(&'static str, Value)>;

fn capabilities(config: &ClaudeCodeConfig) -> Capabilities {
    Capabilities {
        manage_guys: false,
        routines: false,
        routines_local: false,
        approvals: config.approvals,
        thread_actions: false,
        new_threads: false,
        attachments: false,
        models: false,
        files: false,
        skills: false,
        commands: false,
        tools: false,
        plugins: false,
        browser: false,
        past_sessions: false,
    }
}

#[derive(Clone)]
pub struct ClaudeCodeClient {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    config: ClaudeCodeConfig,
    data_dir: PathBuf,
    hooks: Hooks,
    status: Mutex<ConnStatus>,
    started: AtomicBool,
    stopped: AtomicBool,
    /// Leave the current connection and set up again (after installing the plugin).
    restart: AtomicBool,
    /// Poll `claude agents` on the next tick.
    poke: AtomicBool,
    wake: Notify,
    claude: Mutex<Option<PathBuf>>,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    connected: bool,
    sessions: HashMap<String, Session>,
    approvals: HashMap<String, Approval>,
    transcripts: TranscriptIndex,
}

#[derive(Default)]
struct Session {
    id: String,
    /// The short id `claude attach|stop` take (background sessions).
    short_id: Option<String>,
    background: bool,
    cwd: String,
    name: Option<String>,
    ai_title: Option<String>,
    first_prompt: Option<String>,
    /// Its last words, for the thread list.
    said: Option<String>,
    waiting_for: Option<String>,
    transcript: Option<PathBuf>,
    /// Its process is up (listed with a pid, or reporting through hooks).
    running: bool,
    /// It reports through the plugin's hooks.
    hooked: bool,
    last_hook: Option<Instant>,
    busy: bool,
    busy_changed: Option<Instant>,
    turn: u64,
    started_at: u64,
    updated_at: u64,
    tail: Option<Tail>,
    /// tool_use id → tool name, for naming tool results.
    tools: HashMap<String, String>,
    /// Your messages waiting for the session.
    outbox: Vec<String>,
    delivering: bool,
}

impl Session {
    fn new(id: &str) -> Self {
        Self { id: id.to_string(), ..Default::default() }
    }

    fn key(&self) -> String {
        keys::thread(AGENT_ID, &self.id)
    }

    fn run_id(&self) -> String {
        format!("claude-{}-{}", self.id, self.turn)
    }

    fn listable(&self, now: u64) -> bool {
        self.running || self.busy || now.saturating_sub(self.updated_at.max(self.started_at)) < LIST_WINDOW_MS
    }

    fn followed(&self, now: u64) -> bool {
        self.running || self.busy || now.saturating_sub(self.updated_at) < FOLLOW_WINDOW_MS
    }

    fn title(&self) -> Option<String> {
        let (first, second) = if self.background { (&self.name, &self.ai_title) } else { (&self.ai_title, &self.name) };
        first
            .clone()
            .or_else(|| second.clone())
            .or_else(|| self.first_prompt.as_deref().map(|p| truncate(first_line(p), 80)))
            .or_else(|| Path::new(&self.cwd).file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    fn preview(&self) -> Option<String> {
        match (&self.waiting_for, &self.said) {
            (Some(w), _) if self.running && !self.busy => Some(format!("Waiting for you: {w}")),
            (_, Some(said)) => Some(truncate(said, 200)),
            _ => None,
        }
    }

    /// A turn started or ended; returns whether it changed.
    fn set_busy(&mut self, busy: bool, emits: &mut Emits) -> bool {
        if self.busy == busy {
            return false;
        }
        self.busy = busy;
        self.busy_changed = Some(Instant::now());
        let key = self.key();
        if busy {
            self.turn += 1;
            self.waiting_for = None;
            emits.push(("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "start" })));
            emits.push(("chat", json!({ "sessionKey": key, "runId": self.run_id(), "state": "status", "phase": "thinking" })));
        } else {
            emits.push(("chat", json!({ "sessionKey": key, "runId": self.run_id(), "state": "final" })));
            emits.push(("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "end" })));
        }
        true
    }
}

#[derive(Default, Clone)]
struct Tail {
    offset: u64,
    partial: Vec<u8>,
}

struct Approval {
    session_id: String,
    payload: Value,
    tx: Option<oneshot::Sender<String>>,
}

impl ClaudeCodeClient {
    pub fn new(id: String, name: String, config: ClaudeCodeConfig, data_dir: PathBuf, hooks: Hooks) -> Self {
        let status = ConnStatus {
            gateway_id: id.clone(),
            gateway_name: name,
            kind: RuntimeKind::Claudecode,
            capabilities: capabilities(&config),
            mode: Some(GatewayMode::Local),
            url: Some("This Mac".into()),
            ..Default::default()
        };
        Self {
            inner: Arc::new(Inner {
                id,
                config,
                data_dir,
                hooks,
                status: Mutex::new(status),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                restart: AtomicBool::new(false),
                poke: AtomicBool::new(false),
                wake: Notify::new(),
                claude: Mutex::new(None),
                st: Mutex::new(State::default()),
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
        tauri::async_runtime::spawn(async move { run(inner).await });
    }

    /// Stop for good (bot removed or turned off).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
    }

    pub fn reconnect_now(&self) {
        self.inner.restart.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
    }

    pub fn set_name(&self, name: String) {
        set_status(&self.inner, |s| s.gateway_name = name);
    }

    pub fn set_credential_override(&self, _credential: Option<Credential>) {}

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, _timeout: Duration) -> Result<Value, RpcError> {
        let inner = &self.inner;
        match method {
            "sessions.subscribe" | "sessions.unsubscribe" | "sessions.messages.subscribe" | "sessions.messages.unsubscribe" => {
                return Ok(json!({ "subscribed": true }))
            }
            "exec.approval.list" => {
                let st = inner.st.lock().unwrap();
                let approvals: Vec<Value> = st.approvals.values().map(|a| a.payload.clone()).collect();
                return Ok(json!({ "approvals": approvals }));
            }
            _ => {}
        }
        if !inner.st.lock().unwrap().connected {
            return Err(RpcError::new("NOT_CONNECTED", "not following Claude Code yet"));
        }
        match method {
            "agents.list" => Ok(agents_list(inner)),
            "sessions.list" => Ok(sessions_list(inner, &params)),
            "chat.history" => chat_history(inner, &params).await,
            "chat.send" => chat_send(inner, &params),
            "chat.abort" => Err(RpcError::new("UNSUPPORTED", "Press Esc in the session's terminal (claude attach) to stop it.")),
            "exec.approval.resolve" => approval_resolve(inner, &params),
            other => Err(unsupported(other)),
        }
    }
}

// ---- status / events ------------------------------------------------------------------

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
        emit(inner, name, payload);
    }
}

/// Ask Activity to fetch the thread list again.
fn refresh() -> (&'static str, Value) {
    ("sessions.changed", json!({ "reason": "claude" }))
}

async fn wait_or_wake(inner: &Inner, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = inner.wake.notified() => true,
    }
}

// ---- supervisor -----------------------------------------------------------------------

struct Problem {
    phase: Phase,
    message: String,
    code: Option<&'static str>,
}

impl Problem {
    fn new(phase: Phase, message: impl Into<String>, code: Option<&'static str>) -> Self {
        Self { phase, message: message.into(), code }
    }
}

async fn run(inner: Arc<Inner>) {
    let mut attempt: u32 = 0;
    while !inner.stopped.load(Ordering::SeqCst) {
        inner.restart.store(false, Ordering::SeqCst);
        match prepare(&inner).await {
            Ok((listener, socket)) => {
                attempt = 0;
                serve(&inner, listener).await;
                remove_socket(&socket);
            }
            Err(problem) => set_status(&inner, |s| {
                s.phase = problem.phase;
                s.message = Some(problem.message);
                s.detail_code = problem.code.map(str::to_string);
            }),
        }
        if inner.stopped.load(Ordering::SeqCst) {
            break;
        }
        if inner.restart.load(Ordering::SeqCst) {
            continue;
        }
        let delay = Duration::from_secs((2u64 << attempt.min(4)).min(30));
        attempt += 1;
        wait_or_wake(&inner, delay).await;
    }
}

/// The socket and the inode it was bound as (so a stale cleanup never removes a newer one).
struct Socket {
    path: PathBuf,
    ino: Option<u64>,
}

fn remove_socket(socket: &Socket) {
    use std::os::unix::fs::MetadataExt;
    let ours = std::fs::symlink_metadata(&socket.path).ok().map(|m| m.ino()) == socket.ino;
    if ours {
        let _ = std::fs::remove_file(&socket.path);
    }
}

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_FILE)
}

async fn prepare(inner: &Arc<Inner>) -> Result<(UnixListener, Socket), Problem> {
    set_status(inner, |s| {
        if !matches!(s.phase, Phase::NoConfig | Phase::Error) {
            s.phase = Phase::Connecting;
        }
    });
    let claude = find_claude().ok_or_else(|| {
        Problem::new(Phase::NoConfig, "Claude Code isn't installed on this Mac. Install it from claude.com/claude-code, then try again.", None)
    })?;
    *inner.claude.lock().unwrap() = Some(claude.clone());
    let version = claude_version(&claude).await;
    set_status(inner, |s| s.server_version = version);

    // `LITTLEGUYS_CLAUDE_PLUGIN=skip` (development): sessions started with
    // `claude --plugin-dir` report without the plugin being installed.
    let skip_check = std::env::var("LITTLEGUYS_CLAUDE_PLUGIN").is_ok_and(|v| v == "skip");
    let dir = write_plugin_files(&inner.data_dir).map_err(|e| Problem::new(Phase::Error, format!("Couldn't write the littleguys plugin: {e}"), None))?;
    if !skip_check {
        match plugin_state(&claude).await {
            Some(p) if p.enabled && p.folder.as_deref().is_none_or(|f| same_path(f, &dir.join("plugin"))) => {}
            Some(p) if !p.enabled => {
                return Err(Problem::new(Phase::NoConfig, "The littleguys plugin is turned off in Claude Code.", Some(PLUGIN_MISSING)))
            }
            _ => {
                return Err(Problem::new(
                    Phase::NoConfig,
                    "Install the littleguys plugin in Claude Code so your sessions report here.",
                    Some(PLUGIN_MISSING),
                ))
            }
        }
    }
    bind_socket(&socket_path(&inner.data_dir)).await
}

fn same_path(a: &str, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(Path::new(a)) == canon(b)
}

async fn bind_socket(path: &Path) -> Result<(UnixListener, Socket), Problem> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if path.exists() {
        // A connection being replaced (settings changed) lets go within a tick.
        let mut taken = true;
        for _ in 0..20 {
            if UnixStream::connect(path).await.is_err() {
                taken = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if taken {
            return Err(Problem::new(Phase::Error, "Another littleguys is already following your Claude Code sessions.", None));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|e| Problem::new(Phase::Error, format!("Couldn't listen for Claude Code: {e}"), None))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    let ino = std::fs::symlink_metadata(path).ok().map(|m| m.ino());
    Ok((listener, Socket { path: path.to_path_buf(), ino }))
}

async fn serve(inner: &Arc<Inner>, listener: UnixListener) {
    inner.st.lock().unwrap().connected = true;
    set_status(inner, |s| {
        s.phase = Phase::Connected;
        s.message = None;
        s.detail_code = None;
        s.capabilities = capabilities(&inner.config);
        s.hello = Some(json!({ "approvals": inner.config.approvals }));
    });
    (inner.hooks.connected)();

    let acceptor = {
        let inner = inner.clone();
        tauri::async_runtime::spawn(async move { accept_loop(inner, listener).await })
    };
    poll_agents(inner).await;
    let mut tick: u32 = 1;
    loop {
        wait_or_wake(inner, TICK).await;
        if inner.stopped.load(Ordering::SeqCst) || inner.restart.load(Ordering::SeqCst) {
            break;
        }
        if tick % AGENTS_EVERY == 0 || inner.poke.swap(false, Ordering::SeqCst) {
            poll_agents(inner).await;
            deliver_due(inner);
        }
        follow_transcripts(inner).await;
        tick = tick.wrapping_add(1);
    }
    acceptor.abort();
    let waiting: Vec<oneshot::Sender<String>> = {
        let mut st = inner.st.lock().unwrap();
        st.connected = false;
        st.approvals.values_mut().filter_map(|a| a.tx.take()).collect()
    };
    for tx in waiting {
        let _ = tx.send("expired".into());
    }
    set_status(inner, |s| s.phase = Phase::Connecting);
}

// ---- hooks: a tiny HTTP server on the unix socket -------------------------------------

async fn accept_loop(inner: Arc<Inner>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let inner = inner.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = handle_conn(&inner, stream).await {
                        log::debug!("[claude] hook connection: {e}");
                    }
                });
            }
            Err(e) => {
                log::warn!("[claude] accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug, Default)]
struct Request {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Request> {
    use std::io::{Error, ErrorKind};
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(Error::new(ErrorKind::InvalidData, "headers too large"));
        }
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "closed before the headers ended"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next().and_then(|l| l.split(' ').nth(1)).unwrap_or("/").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > REQUEST_MAX_BYTES {
        return Err(Error::new(ErrorKind::InvalidData, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "closed mid-body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request { path, headers, body })
}

async fn write_response<W: AsyncWrite + Unpin>(w: &mut W, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body.as_bytes()).await?;
    let _ = w.shutdown().await;
    Ok(())
}

async fn handle_conn(inner: &Arc<Inner>, mut stream: UnixStream) -> std::io::Result<()> {
    let request = match tokio::time::timeout(Duration::from_secs(10), read_request(&mut stream)).await {
        Ok(r) => r?,
        Err(_) => return Ok(()),
    };
    let reply = match request.path.strip_prefix("/hook/") {
        Some(event) => on_hook(inner, event, &request, &mut stream).await,
        None => String::new(),
    };
    write_response(&mut stream, &reply).await
}

/// Resolves once the hook's end of the connection goes away (Claude Code gave up on it).
async fn peer_closed(stream: &mut UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// One hook event from a session; returns what the hook prints for Claude Code.
async fn on_hook(inner: &Arc<Inner>, event: &str, request: &Request, stream: &mut UnixStream) -> String {
    let Ok(input) = serde_json::from_slice::<Value>(&request.body) else { return String::new() };
    // Subagents' hooks belong to their parent's turn.
    if input.get("agent_id").and_then(Value::as_str).is_some_and(|a| !a.is_empty()) {
        return String::new();
    }
    // `claude -p`, the Agent SDK (littleguys' own ACP guys among them): not sessions you follow.
    let entrypoint = request.headers.get("x-claude-entrypoint").map(String::as_str).unwrap_or("");
    if !entrypoint.is_empty() && entrypoint != "cli" {
        return String::new();
    }
    let Some(sid) = input.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
        return String::new();
    };
    let background = request.headers.get("x-claude-background").is_some_and(|v| v == "1");
    let mut emits: Emits = Vec::new();
    let mut expire: Vec<oneshot::Sender<String>> = Vec::new();
    let mut reply = String::new();
    {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let is_new = !st.sessions.contains_key(&sid);
        let s = st.sessions.entry(sid.clone()).or_insert_with(|| Session::new(&sid));
        s.hooked = true;
        s.last_hook = Some(Instant::now());
        s.updated_at = now_ms();
        if s.started_at == 0 {
            s.started_at = s.updated_at;
        }
        s.background |= background;
        if let Some(path) = input.get("transcript_path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            s.transcript = Some(PathBuf::from(path));
        }
        if let Some(cwd) = input.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            s.cwd = cwd.to_string();
        }
        if event != "SessionEnd" {
            if !s.running {
                emits.push(refresh());
            }
            s.running = true;
        }
        if is_new {
            emits.push(refresh());
            // A brand-new session: follow its transcript from the top, its first prompt included.
            let fresh = input.get("source").and_then(Value::as_str).is_some_and(|s| s == "startup" || s == "clear");
            if event == "SessionStart" && fresh {
                s.tail = Some(Tail::default());
            }
        }
        match event {
            "UserPromptSubmit" => {
                if s.first_prompt.is_none() {
                    s.first_prompt = input.get("prompt").and_then(Value::as_str).and_then(visible_prompt);
                }
                s.set_busy(true, &mut emits);
                // A terminal session that was idle: what you sent rides along with this prompt.
                if !s.outbox.is_empty() && !s.background && !s.delivering {
                    reply = handover(&std::mem::take(&mut s.outbox));
                }
            }
            "Notification" => match input.get("notification_type").and_then(Value::as_str) {
                Some("idle_prompt") => {
                    s.set_busy(false, &mut emits);
                }
                Some("permission_prompt") => {
                    s.waiting_for = input.get("message").and_then(Value::as_str).map(|m| truncate(first_line(m), 120));
                    emits.push(refresh());
                }
                _ => {}
            },
            "Stop" => {
                s.waiting_for = None;
                if !s.outbox.is_empty() && !s.delivering {
                    // Your message keeps the turn going.
                    let reason = handover(&std::mem::take(&mut s.outbox));
                    reply = json!({ "decision": "block", "reason": reason }).to_string();
                } else {
                    s.set_busy(false, &mut emits);
                }
                if let Some(last) = input.get("last_assistant_message").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                    s.said = Some(last.trim().to_string());
                }
            }
            "SessionEnd" => {
                s.running = false;
                s.waiting_for = None;
                s.set_busy(false, &mut emits);
                emits.push(refresh());
                expire.extend(st.approvals.values_mut().filter(|a| a.session_id == sid).filter_map(|a| a.tx.take()));
            }
            _ => {}
        }
    }
    flush(inner, emits);
    for tx in expire {
        let _ = tx.send("expired".into());
    }
    if event == "PermissionRequest" {
        return permission(inner, &sid, &input, stream).await;
    }
    reply
}

/// The text a session gets for your queued messages.
fn handover(outbox: &[String]) -> String {
    format!("{MARK}\n\n{}", outbox.join("\n\n"))
}

/// Waits for Allow or Deny from littleguys, when it's the one answering this session's prompts.
async fn permission(inner: &Arc<Inner>, sid: &str, input: &Value, stream: &mut UnixStream) -> String {
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("tool").to_string();
    let (eligible, key, cwd) = {
        let st = inner.st.lock().unwrap();
        let Some(s) = st.sessions.get(sid) else { return String::new() };
        // Questions need their answers picked, which an approval card can't do.
        (inner.config.approvals && s.background && tool != "AskUserQuestion", s.key(), s.cwd.clone())
    };
    if !eligible {
        return String::new();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let command = command_text(&tool, input.get("tool_input").unwrap_or(&Value::Null));
    let suggestions = input.get("permission_suggestions").cloned().filter(|s| s.as_array().is_some_and(|a| !a.is_empty()));
    let mut allowed = vec!["allow-once"];
    if suggestions.is_some() {
        allowed.push("allow-always");
    }
    allowed.push("deny");
    let payload = json!({
        "id": id,
        "agentId": AGENT_ID,
        "sessionKey": key,
        "createdAtMs": now_ms(),
        "presentation": { "commandText": command, "allowedDecisions": allowed },
        "request": { "id": id, "command": command, "agentId": AGENT_ID, "sessionKey": key, "cwd": cwd, "toolName": tool },
    });
    let (tx, rx) = oneshot::channel::<String>();
    inner
        .st
        .lock()
        .unwrap()
        .approvals
        .insert(id.clone(), Approval { session_id: sid.to_string(), payload: payload.clone(), tx: Some(tx) });
    emit(inner, "exec.approval.requested", payload);
    let decision = tokio::select! {
        d = rx => d.ok(),
        _ = peer_closed(stream) => None,
    };
    inner.st.lock().unwrap().approvals.remove(&id);
    emit(inner, "exec.approval.resolved", json!({ "id": id, "decision": decision.as_deref().unwrap_or("expired") }));
    hook_decision(decision.as_deref(), suggestions.as_ref())
}

/// The PermissionRequest hook's answer; empty leaves the prompt to Claude Code.
fn hook_decision(decision: Option<&str>, suggestions: Option<&Value>) -> String {
    let decision = match decision {
        Some("allow-once") => json!({ "behavior": "allow" }),
        Some("allow-always") => match suggestions {
            Some(s) => json!({ "behavior": "allow", "updatedPermissions": s }),
            None => json!({ "behavior": "allow" }),
        },
        Some("deny") => json!({ "behavior": "deny", "message": "The person denied this in littleguys." }),
        _ => return String::new(),
    };
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision } }).to_string()
}

/// What an approval card shows for a tool call.
fn command_text(tool: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    match tool {
        "Bash" => field("command").map(str::to_string).unwrap_or_else(|| "Bash".into()),
        "Edit" | "MultiEdit" | "Write" | "Read" | "NotebookEdit" => match field("file_path").or_else(|| field("notebook_path")) {
            Some(path) => format!("{tool} {path}"),
            None => tool.to_string(),
        },
        "WebFetch" => format!("WebFetch {}", field("url").unwrap_or_default()).trim().to_string(),
        "WebSearch" => format!("WebSearch {}", field("query").unwrap_or_default()).trim().to_string(),
        _ => {
            let args = match input {
                Value::Null => String::new(),
                Value::Object(o) if o.is_empty() => String::new(),
                other => truncate(&other.to_string(), 300),
            };
            format!("{tool} {args}").trim().to_string()
        }
    }
}

fn approval_resolve(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "id is required"))?;
    let decision = match params.get("decision").and_then(Value::as_str).unwrap_or("deny") {
        "allow-once" | "allow" => "allow-once",
        "allow-always" => "allow-always",
        _ => "deny",
    };
    let tx = inner.st.lock().unwrap().approvals.get_mut(id).and_then(|a| a.tx.take());
    match tx {
        Some(tx) => {
            let _ = tx.send(decision.into());
            Ok(json!({ "ok": true }))
        }
        None => Err(RpcError::new("NOT_FOUND", "That prompt was already answered.")),
    }
}

// ---- sessions: `claude agents` ----------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq)]
struct Listed {
    session_id: String,
    short_id: Option<String>,
    background: bool,
    cwd: String,
    name: Option<String>,
    pid: Option<u64>,
    status: Option<String>,
    waiting_for: Option<String>,
    started_at: u64,
}

/// `claude agents --json --all`: a JSON array (after anything printed before it).
fn parse_agents(stdout: &str) -> Vec<Listed> {
    let Some(start) = stdout.find('[') else { return Vec::new() };
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(&stdout[start..]) else { return Vec::new() };
    let text = |row: &Value, k: &str| row.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    rows.iter()
        .filter_map(|row| {
            Some(Listed {
                session_id: text(row, "sessionId")?,
                short_id: text(row, "id"),
                background: row.get("kind").and_then(Value::as_str) == Some("background"),
                cwd: text(row, "cwd").unwrap_or_default(),
                name: text(row, "name"),
                pid: row.get("pid").and_then(Value::as_u64),
                status: text(row, "status"),
                waiting_for: text(row, "waitingFor"),
                started_at: row.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect()
}

async fn list_agents(claude: &Path) -> Option<Vec<Listed>> {
    let out = run_cli(claude, &["agents", "--json", "--all"], CLI_TIMEOUT).await.ok()?;
    Some(parse_agents(&out))
}

async fn poll_agents(inner: &Arc<Inner>) {
    let Some(claude) = inner.claude.lock().unwrap().clone() else { return };
    let Some(listed) = list_agents(&claude).await else { return };
    let now = now_ms();
    let mut emits: Emits = Vec::new();
    let mut changed = false;
    {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let ids: HashSet<&str> = listed.iter().map(|l| l.session_id.as_str()).collect();
        for l in &listed {
            let running = l.pid.is_some();
            let known = st.sessions.contains_key(&l.session_id);
            if !known && !running && now.saturating_sub(l.started_at) >= LIST_WINDOW_MS {
                continue;
            }
            let s = st.sessions.entry(l.session_id.clone()).or_insert_with(|| Session::new(&l.session_id));
            changed |= !known || s.running != running || s.name != l.name || (s.waiting_for != l.waiting_for && !s.hooked);
            s.background = l.background;
            s.short_id = l.short_id.clone().filter(|_| l.background);
            if !s.hooked || s.cwd.is_empty() {
                s.cwd = l.cwd.clone();
            }
            s.name = l.name.clone();
            if !s.hooked {
                s.waiting_for = l.waiting_for.clone();
            }
            s.started_at = l.started_at;
            // A hook just said otherwise: it's newer than this listing.
            let hook_fresh = s.last_hook.is_some_and(|t| t.elapsed() < HOOK_SETTLE);
            if !hook_fresh {
                s.running = running;
            }
            // Hooks say when a turn starts and ends; the listing only fills in for sessions
            // without them, or when they've gone quiet (a turn stopped with Esc never reports).
            let busy = running && l.status.as_deref() == Some("busy");
            let quiet = if s.hooked { HOOKS_QUIET } else { HOOK_SETTLE };
            let settled = s.busy_changed.is_none_or(|t| t.elapsed() > quiet) && s.last_hook.is_none_or(|t| t.elapsed() > quiet);
            if busy != s.busy && settled {
                s.set_busy(busy, &mut emits);
            }
        }
        // Terminal sessions drop off the list when they exit.
        for s in st.sessions.values_mut() {
            if s.running && !ids.contains(s.id.as_str()) && s.last_hook.is_none_or(|t| t.elapsed() > HOOK_SETTLE) {
                s.running = false;
                s.set_busy(false, &mut emits);
                changed = true;
            }
        }
    }
    if changed {
        emits.push(refresh());
    }
    flush(inner, emits);
}

// ---- transcripts --------------------------------------------------------------------------

fn claude_home() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir().unwrap_or_default().join(".claude"),
    }
}

/// `~/.claude/projects/<cwd with every non-alphanumeric as '-'>`.
fn project_slug(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Session id → transcript path, from a scan of `~/.claude/projects`.
#[derive(Default)]
struct TranscriptIndex {
    by_id: HashMap<String, PathBuf>,
    scanned: Option<Instant>,
}

impl TranscriptIndex {
    fn find(&mut self, sid: &str, cwd: &str) -> Option<PathBuf> {
        let projects = claude_home().join("projects");
        if !cwd.is_empty() {
            let guess = projects.join(project_slug(cwd)).join(format!("{sid}.jsonl"));
            if guess.is_file() {
                return Some(guess);
            }
        }
        if let Some(path) = self.by_id.get(sid).filter(|p| p.is_file()) {
            return Some(path.clone());
        }
        if self.scanned.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return None;
        }
        self.scanned = Some(Instant::now());
        self.by_id.clear();
        for dir in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                let path = file.path();
                if path.extension().is_some_and(|e| e == "jsonl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        self.by_id.insert(stem.to_string(), path.clone());
                    }
                }
            }
        }
        self.by_id.get(sid).cloned()
    }
}

/// Where a transcript was read up to, and what was new.
enum Read {
    /// First look: only the end was read, for the title and last words.
    First { tail: Tail, lines: Vec<Value>, modified: u64 },
    New { tail: Tail, lines: Vec<Value>, modified: u64 },
    Same,
    Gone,
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// Reads `[from, to)` and splits it into JSON lines. Returns them, how far it
/// got (the end of the last whole line) and the unfinished rest.
fn read_lines(path: &Path, from: u64, to: u64, carry: Vec<u8>, skip_first: bool) -> std::io::Result<(Vec<Value>, u64, Vec<u8>)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::with_capacity((to - from) as usize);
    file.take(to - from).read_to_end(&mut bytes)?;
    let read = bytes.len() as u64;
    let mut data = carry;
    data.extend_from_slice(&bytes);
    let cut = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let rest = data[cut..].to_vec();
    let mut lines = Vec::new();
    for (i, raw) in data[..cut].split(|b| *b == b'\n').enumerate() {
        if (skip_first && i == 0) || raw.len() <= 1 {
            continue;
        }
        if let Ok(line) = serde_json::from_slice::<Value>(raw) {
            lines.push(line);
        }
    }
    Ok((lines, from + read, rest))
}

fn read_transcript(path: &Path, tail: Option<Tail>) -> Read {
    let Ok(meta) = std::fs::metadata(path) else { return Read::Gone };
    let len = meta.len();
    let modified = modified_ms(&meta);
    let first = |len: u64| {
        let from = len.saturating_sub(SUMMARY_BYTES);
        match read_lines(path, from, len, Vec::new(), from > 0) {
            Ok((lines, offset, partial)) => Read::First { tail: Tail { offset, partial }, lines, modified },
            Err(_) => Read::Gone,
        }
    };
    match tail {
        None => first(len),
        // Rewritten from scratch: start over from its end.
        Some(t) if len < t.offset => first(len),
        Some(t) if len == t.offset => Read::Same,
        Some(t) => {
            let to = len.min(t.offset + TAIL_MAX_BYTES);
            match read_lines(path, t.offset, to, t.partial, false) {
                Ok((lines, offset, partial)) => Read::New { tail: Tail { offset, partial }, lines, modified },
                Err(_) => Read::Gone,
            }
        }
    }
}

async fn follow_transcripts(inner: &Arc<Inner>) {
    let now = now_ms();
    let jobs: Vec<(String, PathBuf, Option<Tail>)> = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let mut jobs = Vec::new();
        for s in st.sessions.values_mut() {
            let first_look = s.tail.is_none() && s.listable(now);
            if !first_look && !(s.tail.is_some() && s.followed(now)) {
                continue;
            }
            if s.transcript.as_ref().is_none_or(|p| !p.is_file()) {
                s.transcript = st.transcripts.find(&s.id, &s.cwd);
            }
            if let Some(path) = &s.transcript {
                jobs.push((s.id.clone(), path.clone(), s.tail.clone()));
            }
        }
        jobs
    };
    if jobs.is_empty() {
        return;
    }
    let reads = tauri::async_runtime::spawn_blocking(move || {
        jobs.into_iter().map(|(sid, path, tail)| (sid, read_transcript(&path, tail))).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut emits: Emits = Vec::new();
    let mut changed = false;
    {
        let mut st = inner.st.lock().unwrap();
        for (sid, read) in reads {
            let Some(s) = st.sessions.get_mut(&sid) else { continue };
            match read {
                Read::First { tail, lines, modified } => {
                    for line in &lines {
                        note_line(s, line, false, &mut emits);
                        learn_tools(line, &mut s.tools);
                    }
                    s.tail = Some(tail);
                    s.updated_at = s.updated_at.max(modified);
                    changed = true;
                }
                Read::New { tail, lines, modified } => {
                    s.tail = Some(tail);
                    s.updated_at = s.updated_at.max(modified);
                    let key = s.key();
                    for line in &lines {
                        changed |= note_line(s, line, true, &mut emits);
                        let run_id = s.busy.then(|| s.run_id());
                        for message in convert_line(line, &mut s.tools) {
                            let mut payload = json!({ "sessionKey": key, "message": message });
                            if let Some(run_id) = &run_id {
                                payload["runId"] = json!(run_id);
                            }
                            emits.push(("session.message", payload));
                        }
                    }
                }
                Read::Same | Read::Gone => {}
            }
        }
    }
    if changed {
        emits.push(refresh());
    }
    flush(inner, emits);
}

/// Folds what a line says about its session in (title, last words, turns);
/// returns whether the thread list should change.
fn note_line(s: &mut Session, line: &Value, live: bool, emits: &mut Emits) -> bool {
    let meta = line_meta(line);
    let mut changed = false;
    if let Some(title) = meta.ai_title {
        changed |= s.ai_title.as_ref() != Some(&title);
        s.ai_title = Some(title);
    }
    if let Some(said) = meta.said {
        s.said = Some(said);
    }
    if let Some(prompt) = meta.prompt {
        if s.first_prompt.is_none() {
            s.first_prompt = Some(prompt);
        }
        // Without hooks, the transcript is how a turn's start and end show.
        if live && !s.hooked {
            changed |= s.set_busy(true, emits);
        }
    }
    if meta.turn_end && live && !s.hooked {
        changed |= s.set_busy(false, emits);
    }
    changed
}

/// What one transcript line says about its session, besides its messages.
#[derive(Debug, Default, PartialEq)]
struct LineMeta {
    ai_title: Option<String>,
    prompt: Option<String>,
    said: Option<String>,
    turn_end: bool,
}

fn flag(line: &Value, k: &str) -> bool {
    line.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn line_meta(line: &Value) -> LineMeta {
    let mut meta = LineMeta::default();
    if flag(line, "isSidechain") {
        return meta;
    }
    match line.get("type").and_then(Value::as_str) {
        Some("ai-title") => meta.ai_title = line.get("aiTitle").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string),
        Some("system") => meta.turn_end = line.get("subtype").and_then(Value::as_str) == Some("turn_duration"),
        Some("user") if !flag(line, "isMeta") && !flag(line, "isCompactSummary") => {
            meta.prompt = match line.pointer("/message/content") {
                Some(Value::String(s)) => visible_prompt(s),
                Some(Value::Array(blocks)) => {
                    let text: Vec<&str> = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect();
                    if text.is_empty() {
                        None
                    } else {
                        visible_prompt(&text.join("\n"))
                    }
                }
                _ => None,
            }
        }
        Some("assistant") => {
            meta.said = line
                .pointer("/message/content")
                .and_then(Value::as_array)
                .and_then(|blocks| {
                    blocks
                        .iter()
                        .rev()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .find_map(|b| b.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()))
                })
                .map(str::to_string)
        }
        _ => {}
    }
    meta
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis().max(0) as u64)
}

fn chat_message(role: &str, content: Vec<Value>, ts: Option<u64>, id: &str) -> Value {
    let mut m = json!({ "role": role, "content": content, "__openclaw": { "id": id } });
    if let Some(ts) = ts {
        m["timestamp"] = json!(ts);
    }
    m
}

/// Tags Claude Code wraps around what it adds to a prompt (reminders, command
/// output, notifications) rather than what you typed.
const HIDDEN_TAGS: &[&str] = &[
    "system-reminder",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "bash-stdout",
    "bash-stderr",
    "command-message",
    "task-notification",
    "agent-message",
    "teammate-message",
    "channel",
];

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// Removes every `<tag …>…</tag>` block.
fn strip_blocks(text: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(at) = rest.find(&open) else { break };
        let after = &rest[at + open.len()..];
        if !after.starts_with(['>', ' ', '\n']) {
            out.push_str(&rest[..at + open.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..at]);
        rest = match after.find(&close) {
            Some(end) => &after[end + close.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// What you typed, from a prompt as Claude Code saved it; None when there's nothing of yours.
fn visible_prompt(raw: &str) -> Option<String> {
    if let Some(name) = between(raw, "<command-name>", "</command-name>") {
        let name = name.trim();
        let name = if name.starts_with('/') { name.to_string() } else { format!("/{name}") };
        let args = between(raw, "<command-args>", "</command-args>").unwrap_or("").trim();
        return Some(format!("{name} {args}").trim().to_string());
    }
    if let Some(input) = between(raw, "<bash-input>", "</bash-input>") {
        return Some(format!("! {}", input.trim()));
    }
    let mut text = raw.to_string();
    for tag in HIDDEN_TAGS {
        text = strip_blocks(&text, tag);
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Your message, when a line is littleguys handing it over (Stop hook feedback, or
/// context added to the next prompt).
fn handed_over(text: &str) -> Option<String> {
    let at = text.find(MARK)?;
    let message = text[at + MARK.len()..].trim();
    (!message.is_empty()).then(|| message.to_string())
}

fn truncate_text(text: &str) -> String {
    if text.len() <= TOOL_TEXT_MAX {
        return text.to_string();
    }
    let mut end = TOOL_TEXT_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut: {} more characters)", &text[..end], text.len() - end)
}

/// A transcript line → contract ChatMessages (none for bookkeeping lines).
fn convert_line(line: &Value, tools: &mut HashMap<String, String>) -> Vec<Value> {
    if flag(line, "isSidechain") {
        return Vec::new();
    }
    let uuid = line.get("uuid").and_then(Value::as_str).unwrap_or_default();
    let ts = line.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms);
    match line.get("type").and_then(Value::as_str) {
        Some("user") => user_messages(line, uuid, ts, tools),
        Some("assistant") => assistant_message(line, uuid, ts, tools).into_iter().collect(),
        Some("attachment") => {
            let attachment = line.get("attachment").unwrap_or(&Value::Null);
            if attachment.get("type").and_then(Value::as_str) != Some("hook_additional_context") {
                return Vec::new();
            }
            let text: Vec<&str> = attachment.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            handed_over(&text.join("\n"))
                .map(|t| chat_message("user", vec![json!({ "type": "text", "text": t })], ts, uuid))
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

fn user_messages(line: &Value, uuid: &str, ts: Option<u64>, tools: &HashMap<String, String>) -> Vec<Value> {
    if flag(line, "isCompactSummary") {
        return Vec::new();
    }
    let content = line.pointer("/message/content").unwrap_or(&Value::Null);
    if flag(line, "isMeta") {
        return content
            .as_str()
            .and_then(handed_over)
            .map(|t| chat_message("user", vec![json!({ "type": "text", "text": t })], ts, uuid))
            .into_iter()
            .collect();
    }
    match content {
        Value::String(s) => visible_prompt(s)
            .map(|t| chat_message("user", vec![json!({ "type": "text", "text": t })], ts, uuid))
            .into_iter()
            .collect(),
        Value::Array(blocks) => {
            let mut out = Vec::new();
            let mut mine = Vec::new();
            // The line's `toolUseResult` belongs to its tool result, when it has just one.
            let single = blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")).count() == 1;
            let recorded = line.get("toolUseResult").filter(|_| single);
            let diffs = recorded.and_then(edit_diffs);
            // AskUserQuestion: question → what you picked (or typed).
            let answers = recorded.and_then(|r| r.get("answers")).filter(|a| a.as_object().is_some_and(|m| !m.is_empty()));
            for (i, block) in blocks.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let mut result = tool_result(block, &format!("{uuid}:{i}"), ts, tools);
                        if let Some(diffs) = &diffs {
                            result["diffs"] = diffs.clone();
                        }
                        if let Some(answers) = answers {
                            result["answers"] = answers.clone();
                        }
                        out.push(result);
                    }
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str).and_then(visible_prompt) {
                            mine.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    Some("image") => mine.push(block.clone()),
                    _ => {}
                }
            }
            if !mine.is_empty() {
                out.insert(0, chat_message("user", mine, ts, uuid));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Most diff lines a tool result carries; the transcript shows fewer until asked.
const DIFF_MAX_LINES: usize = 2000;

/// The file an Edit, MultiEdit or Write changed, from the patch Claude Code
/// records with its result (a new file has no patch, just its content).
fn edit_diffs(result: &Value) -> Option<Value> {
    let path = result.get("filePath").and_then(Value::as_str)?;
    let mut budget = DIFF_MAX_LINES;
    let mut hunks = Vec::new();
    for hunk in result.get("structuredPatch").and_then(Value::as_array).into_iter().flatten() {
        let lines: Vec<&Value> = hunk.get("lines").and_then(Value::as_array).into_iter().flatten().take(budget).collect();
        if lines.is_empty() {
            continue;
        }
        budget -= lines.len();
        hunks.push(json!({ "oldStart": hunk.get("oldStart"), "newStart": hunk.get("newStart"), "lines": lines }));
    }
    let diff = if !hunks.is_empty() {
        json!({ "path": path, "hunks": hunks })
    } else if result.get("type").and_then(Value::as_str) == Some("create") {
        let content = result.get("content").and_then(Value::as_str).unwrap_or_default();
        let head: Vec<&str> = content.lines().take(DIFF_MAX_LINES).collect();
        json!({ "path": path, "oldText": null, "newText": head.join("\n") })
    } else {
        return None;
    };
    Some(json!([diff]))
}

fn tool_result(block: &Value, id: &str, ts: Option<u64>, tools: &HashMap<String, String>) -> Value {
    let call_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
    let mut content = Vec::new();
    match block.get("content") {
        Some(Value::String(s)) => content.push(json!({ "type": "text", "text": truncate_text(s) })),
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => content.push(json!({ "type": "text", "text": truncate_text(item.get("text").and_then(Value::as_str).unwrap_or_default()) })),
                    Some("image") => content.push(item.clone()),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }
    let mut m = chat_message("toolResult", content, ts, id);
    m["toolCallId"] = json!(call_id);
    m["toolName"] = json!(tools.get(call_id).map(String::as_str).unwrap_or("tool"));
    m["isError"] = json!(flag(block, "is_error"));
    m
}

fn assistant_message(line: &Value, uuid: &str, ts: Option<u64>, tools: &mut HashMap<String, String>) -> Option<Value> {
    let message = line.get("message")?;
    let mut blocks = Vec::new();
    for block in message.get("content").and_then(Value::as_array).into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or_default();
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            Some("thinking") => {
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or_default();
                if !thinking.trim().is_empty() {
                    blocks.push(json!({ "type": "thinking", "thinking": thinking }));
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                tools.insert(id.to_string(), name.to_string());
                blocks.push(json!({ "type": "toolCall", "id": id, "name": name, "arguments": block.get("input").cloned().unwrap_or(json!({})) }));
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    let mut m = chat_message("assistant", blocks, ts, uuid);
    if let Some(model) = message.get("model").and_then(Value::as_str).filter(|m| !m.starts_with('<')) {
        m["model"] = json!(model);
    }
    if flag(line, "isApiErrorMessage") {
        m["stopReason"] = json!("error");
    }
    Some(m)
}

/// Remembers the tool calls a line makes, so their results (read later) are named.
fn learn_tools(line: &Value, tools: &mut HashMap<String, String>) {
    if line.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    for block in line.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let (Some(id), Some(name)) = (block.get("id").and_then(Value::as_str), block.get("name").and_then(Value::as_str)) {
                tools.insert(id.to_string(), name.to_string());
            }
        }
    }
}

/// Every message in a transcript (its last `HISTORY_MAX_BYTES`).
fn read_history(path: &Path) -> std::io::Result<Vec<Value>> {
    let len = std::fs::metadata(path)?.len();
    let from = len.saturating_sub(HISTORY_MAX_BYTES);
    let (lines, _, rest) = read_lines(path, from, len, Vec::new(), from > 0)?;
    let mut tools = HashMap::new();
    let mut messages: Vec<Value> = lines.iter().flat_map(|line| convert_line(line, &mut tools)).collect();
    // A last line still being written.
    if let Ok(line) = serde_json::from_slice::<Value>(&rest) {
        messages.extend(convert_line(&line, &mut tools));
    }
    Ok(messages)
}

// ---- calls ------------------------------------------------------------------------------

fn agents_list(inner: &Inner) -> Value {
    let name = inner.status.lock().unwrap().gateway_name.clone();
    json!({
        "defaultId": AGENT_ID,
        "mainKey": "main",
        "agents": [{
            "id": AGENT_ID,
            "name": name,
            "identity": { "name": name, "avatarUrl": super::claude_face() },
            "workspace": dirs::home_dir().map(|h| h.display().to_string()),
        }],
    })
}

fn sessions_list(inner: &Inner, params: &Value) -> Value {
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
    if super::archived_filter(params) == Some(true) {
        return json!({ "count": 0, "sessions": [] });
    }
    let now = now_ms();
    let st = inner.st.lock().unwrap();
    let mut sessions: Vec<&Session> = st
        .sessions
        .values()
        .filter(|s| s.listable(now) && (s.running || s.transcript.is_some()))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.started_at)));
    sessions.truncate(limit);
    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "key": s.key(),
                "agentId": AGENT_ID,
                "kind": "direct",
                "sessionId": s.id,
                "workspaceDir": s.cwd,
                "derivedTitle": s.title(),
                "lastMessagePreview": s.preview(),
                "createdAt": s.started_at,
                "updatedAt": s.updated_at.max(s.started_at),
                "unread": false,
                "pinned": false,
                "archived": false,
                "hasActiveRun": s.busy,
                "status": if s.busy { "running" } else { "done" },
            })
        })
        .collect();
    json!({ "count": rows.len(), "sessions": rows })
}

fn session_of(params: &Value) -> Result<(String, String), RpcError> {
    let key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?;
    match keys::split(key) {
        Some((AGENT_ID, rest)) if rest.starts_with("dashboard:") => {
            let sid = keys::last_id(key).unwrap_or_default();
            Ok((key.to_string(), sid))
        }
        _ => Err(RpcError::new("NOT_FOUND", "Not a Claude Code session.")),
    }
}

async fn chat_history(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let (key, sid) = session_of(params)?;
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 1000) as usize;
    let path = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let known = st.sessions.get(&sid).and_then(|s| s.transcript.clone().filter(|p| p.is_file()));
        let cwd = st.sessions.get(&sid).map(|s| s.cwd.clone()).unwrap_or_default();
        known.or_else(|| st.transcripts.find(&sid, &cwd))
    };
    let messages = match path {
        Some(path) => tauri::async_runtime::spawn_blocking(move || read_history(&path))
            .await
            .map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
            .map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't read the session's transcript: {e}")))?,
        None => Vec::new(),
    };
    let total = messages.len();
    let messages: Vec<Value> = messages.into_iter().skip(total.saturating_sub(limit)).collect();
    Ok(json!({
        "sessionKey": key,
        "sessionId": sid,
        "messages": messages,
        "hasMore": total > limit,
        "inFlightRun": Value::Null,
    }))
}

/// Your reply: handed over when the session's turn ends, or the session is resumed with it.
fn chat_send(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    let (key, sid) = session_of(params)?;
    let text = params.get("message").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    if text.is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "Write something first."));
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut deliver_now = false;
    let mut settled = false;
    {
        let mut st = inner.st.lock().unwrap();
        let s = st.sessions.get_mut(&sid).ok_or_else(|| RpcError::new("NOT_FOUND", "littleguys lost track of this session."))?;
        if s.running && !s.background && !s.busy && !s.hooked {
            return Err(RpcError::new(
                "UNAVAILABLE",
                "This session is open in a terminal that started before the littleguys plugin was installed. Write to it there.",
            ));
        }
        s.outbox.push(text);
        if s.delivering || (s.busy && (s.hooked || s.background)) {
            // The Stop hook (or the delivery under way) takes it from here.
        } else if s.running && !s.background {
            // Idle in a terminal: it rides along with the next prompt typed there.
            settled = true;
        } else {
            s.delivering = true;
            deliver_now = true;
        }
    }
    if settled {
        emit(inner, "chat", json!({ "sessionKey": key, "runId": run_id, "state": "final" }));
    }
    if deliver_now {
        let inner = inner.clone();
        tauri::async_runtime::spawn(async move { deliver(inner, sid).await });
    }
    Ok(json!({ "runId": run_id, "status": "started" }))
}

/// Sessions with messages waiting that no hook will take: idle background sessions
/// (and ones without hooks once their turn ends), and sessions that ended.
fn deliver_due(inner: &Arc<Inner>) {
    let due: Vec<String> = {
        let mut st = inner.st.lock().unwrap();
        st.sessions
            .values_mut()
            .filter(|s| !s.outbox.is_empty() && !s.delivering && !s.busy && (s.background || !s.running))
            .map(|s| {
                s.delivering = true;
                s.id.clone()
            })
            .collect()
    };
    for sid in due {
        let inner = inner.clone();
        tauri::async_runtime::spawn(async move { deliver(inner, sid).await });
    }
}

async fn deliver(inner: Arc<Inner>, sid: String) {
    let job = {
        let mut st = inner.st.lock().unwrap();
        st.sessions.get_mut(&sid).map(|s| {
            let text = std::mem::take(&mut s.outbox).join("\n\n");
            (text, s.short_id.clone(), s.running && s.background, s.cwd.clone(), s.key())
        })
    };
    let Some((text, short_id, running_bg, cwd, key)) = job else { return };
    let result = resume(&inner, &sid, &text, short_id.as_deref(), running_bg, &cwd).await;
    if let Err(e) = &result {
        log::warn!("[claude] couldn't resume {sid}: {e}");
    }
    let mut emits: Emits = Vec::new();
    {
        let mut st = inner.st.lock().unwrap();
        if let Some(s) = st.sessions.get_mut(&sid) {
            s.delivering = false;
            match &result {
                Ok(()) => {
                    s.running = true;
                    // Its hooks take a moment to report; don't let `claude agents` say otherwise meanwhile.
                    s.last_hook = Some(Instant::now());
                    s.set_busy(true, &mut emits);
                }
                Err(e) => emits.push((
                    "chat",
                    json!({ "sessionKey": key, "runId": s.run_id(), "state": "error", "errorMessage": format!("Couldn't reach this session: {e}") }),
                )),
            }
        }
    }
    flush(&inner, emits);
    inner.poke.store(true, Ordering::SeqCst);
}

/// `claude "<text>" --resume <id> --bg`, after stopping it if it's up in the background
/// (resuming a running session starts a copy).
async fn resume(inner: &Inner, sid: &str, text: &str, short_id: Option<&str>, running_bg: bool, cwd: &str) -> Result<(), String> {
    let claude = inner.claude.lock().unwrap().clone().ok_or("Claude Code isn't installed")?;
    if running_bg {
        if let Some(short) = short_id {
            run_cli(&claude, &["stop", short], CLI_TIMEOUT).await?;
            for _ in 0..30 {
                let up = list_agents(&claude).await.is_some_and(|all| all.iter().any(|l| l.session_id == sid && l.pid.is_some()));
                if !up {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }
    // A prompt starting with '-' would read as a flag.
    let prompt = if text.starts_with('-') { format!(" {text}") } else { text.to_string() };
    let mut cmd = cli::command(&claude);
    cmd.arg(&prompt).args(["--resume", sid, "--bg"]);
    if Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    let out = output(cmd, Duration::from_secs(60)).await?;
    if out.contains("started a copy") {
        log::warn!("[claude] resuming {sid} started a copy: {out}");
    }
    Ok(())
}

// ---- the claude CLI -----------------------------------------------------------------------

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// The `claude` CLI: on the login shell's PATH, or where its installers put it.
pub fn find_claude() -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    cli::login_path()
        .split(':')
        .map(|dir| Path::new(dir).join("claude"))
        .chain([home.join(".local/bin/claude"), home.join(".claude/local/claude")])
        .find(|p| is_executable(p))
}

/// Environment a session launched from inside another one would inherit and shouldn't.
fn clean_env(cmd: &mut tokio::process::Command) {
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        let inherited = matches!(
            k.as_ref(),
            "CLAUDECODE"
                | "CLAUDE_CODE_ENTRYPOINT"
                | "CLAUDE_CODE_CHILD_SESSION"
                | "CLAUDE_CODE_EXECPATH"
                | "CLAUDE_PROJECT_DIR"
                | "CLAUDE_ENV_FILE"
                | "CLAUDE_PID"
                | "CLAUDE_JOB_DIR"
                | "AGENTBOOK_LAUNCH_ID"
                | "AGENTBOOK_LABEL"
                | "AGENTBOOK_PROC"
                | "AGENTBOOK_PID"
                | "AGENTBOOK_CHANNEL"
        ) || k.starts_with("CLAUDE_PLUGIN_")
            || k.starts_with("CLAUDE_CODE_SESSION")
            || k.starts_with("CLAUDE_CODE_MESSAGING");
        if inherited {
            cmd.env_remove(k.as_ref());
        }
    }
}

/// Runs a command; its stdout, or the first line it complained with.
async fn output(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    clean_env(&mut cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("it failed")
        .to_string();
    Err(why)
}

async fn run_cli(claude: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = cli::command(claude);
    cmd.args(args);
    output(cmd, timeout).await
}

async fn claude_version(claude: &Path) -> Option<String> {
    let out = run_cli(claude, &["--version"], CLI_TIMEOUT).await.ok()?;
    out.split_whitespace().next().map(str::to_string)
}

pub struct PluginState {
    pub enabled: bool,
    /// The folder Claude Code loads it from.
    pub folder: Option<String>,
}

pub async fn plugin_state(claude: &Path) -> Option<PluginState> {
    let out = run_cli(claude, &["plugin", "list", "--json"], CLI_TIMEOUT).await.ok()?;
    let start = out.find('[')?;
    let rows: Vec<Value> = serde_json::from_str(&out[start..]).ok()?;
    let row = rows.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(PLUGIN_ID))?;
    Some(PluginState {
        enabled: row.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        folder: row.get("readFromFolder").and_then(Value::as_str).map(str::to_string),
    })
}

/// Writes the plugin (a one-plugin marketplace) to `<data_dir>/claude-plugin`, its
/// hook pointed at this app's socket. Returns that folder.
pub fn write_plugin_files(data_dir: &Path) -> std::io::Result<PathBuf> {
    let root = data_dir.join(PLUGIN_DIR);
    let socket = socket_path(data_dir).display().to_string().replace('\'', r"'\''");
    let files = [
        (".claude-plugin/marketplace.json", MARKETPLACE_JSON.to_string()),
        ("plugin/.claude-plugin/plugin.json", PLUGIN_JSON.to_string()),
        ("plugin/hooks/hooks.json", HOOKS_JSON.to_string()),
        ("plugin/hooks/hook.sh", HOOK_SH.replace("__LITTLEGUYS_SOCKET__", &socket)),
    ];
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&path, content)?;
        }
    }
    Ok(root)
}

/// Installs (or repairs) the littleguys plugin in Claude Code. Sessions started from
/// now on report to littleguys.
pub async fn install_plugin(data_dir: &Path) -> Result<(), String> {
    let claude = find_claude().ok_or("Claude Code isn't installed on this Mac.")?;
    let root = write_plugin_files(data_dir).map_err(|e| format!("Couldn't write the plugin: {e}"))?;
    let root = root.display().to_string();
    run_cli(&claude, &["plugin", "marketplace", "add", &root], Duration::from_secs(60)).await?;
    run_cli(&claude, &["plugin", "install", PLUGIN_ID], Duration::from_secs(60)).await?;
    if plugin_state(&claude).await.is_some_and(|p| !p.enabled) {
        run_cli(&claude, &["plugin", "enable", PLUGIN_ID], Duration::from_secs(60)).await?;
    }
    Ok(())
}

/// Claude Code on this Mac, for the "Add a bot" tab.
pub async fn detect() -> Value {
    let Some(claude) = find_claude() else {
        return json!({ "installed": false, "version": Value::Null, "plugin": Value::Null });
    };
    let (version, plugin) = tokio::join!(claude_version(&claude), plugin_state(&claude));
    json!({
        "installed": true,
        "version": version,
        "plugin": plugin.map(|p| json!({ "enabled": p.enabled, "folder": p.folder })),
    })
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> Value {
        v
    }

    #[test]
    fn slugs_match_claude_codes_project_folders() {
        assert_eq!(project_slug("/Users/me/dev/littleguy"), "-Users-me-dev-littleguy");
        assert_eq!(
            project_slug("/Users/me/dev/littleguy/.claude/worktrees/x_y"),
            "-Users-me-dev-littleguy--claude-worktrees-x-y"
        );
    }

    #[test]
    fn prompts_keep_what_you_typed() {
        assert_eq!(visible_prompt("fix the build").as_deref(), Some("fix the build"));
        assert_eq!(
            visible_prompt("<system-reminder>\nnoise\n</system-reminder>\nfix it").as_deref(),
            Some("fix it")
        );
        assert_eq!(visible_prompt("<task-notification>\n<task-id>x</task-id>\n</task-notification>"), None);
        assert_eq!(
            visible_prompt("<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args>42</command-args>")
                .as_deref(),
            Some("/review 42")
        );
        assert_eq!(visible_prompt("<local-command-stdout>ok</local-command-stdout>"), None);
        assert_eq!(visible_prompt("<bash-input>ls</bash-input>").as_deref(), Some("! ls"));
        assert_eq!(visible_prompt("<channel source=\"x\">hi</channel>"), None);
        // Not one of the tags: kept.
        assert_eq!(visible_prompt("use <channels> here").as_deref(), Some("use <channels> here"));
    }

    #[test]
    fn transcript_lines_become_contract_messages() {
        let mut tools = HashMap::new();
        let assistant = line(json!({
            "type": "assistant", "uuid": "a1", "timestamp": "2026-10-11T20:05:23.826Z",
            "message": { "model": "claude-opus-5-5", "content": [
                { "type": "thinking", "thinking": "" },
                { "type": "text", "text": "Let me look." },
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } }
            ]}
        }));
        let out = convert_line(&assistant, &mut tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["model"], "claude-opus-5-5");
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"][1], json!({ "type": "toolCall", "id": "t1", "name": "Bash", "arguments": { "command": "ls" } }));
        assert_eq!(out[0]["__openclaw"]["id"], "a1");
        assert!(out[0]["timestamp"].as_u64().is_some());

        let result = line(json!({
            "type": "user", "uuid": "u1",
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "a\nb", "is_error": false }] }
        }));
        let out = convert_line(&result, &mut tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "toolResult");
        assert_eq!(out[0]["toolName"], "Bash");
        assert_eq!(out[0]["toolCallId"], "t1");
        assert_eq!(out[0]["content"][0]["text"], "a\nb");
        assert_eq!(out[0]["__openclaw"]["id"], "u1:0");
    }

    #[test]
    fn tool_results_carry_what_claude_code_recorded() {
        let mut tools = HashMap::new();
        let edited = line(json!({
            "type": "user", "uuid": "u2",
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2", "content": "The file /w/a.ts has been updated successfully." }] },
            "toolUseResult": { "filePath": "/w/a.ts", "oldString": "x", "newString": "y", "originalFile": "…", "structuredPatch": [
                { "oldStart": 4, "oldLines": 3, "newStart": 4, "newLines": 3, "lines": [" a", "-x", "+y", " b"] }
            ]}
        }));
        let out = convert_line(&edited, &mut tools);
        assert_eq!(out[0]["diffs"], json!([{ "path": "/w/a.ts", "hunks": [{ "oldStart": 4, "newStart": 4, "lines": [" a", "-x", "+y", " b"] }] }]));

        let created = line(json!({
            "type": "user", "uuid": "u3",
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t3", "content": "File created successfully at: /w/b.ts" }] },
            "toolUseResult": { "type": "create", "filePath": "/w/b.ts", "content": "one\ntwo\n", "structuredPatch": [] }
        }));
        let out = convert_line(&created, &mut tools);
        assert_eq!(out[0]["diffs"], json!([{ "path": "/w/b.ts", "oldText": null, "newText": "one\ntwo" }]));

        let ran = line(json!({
            "type": "user", "uuid": "u4",
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t4", "content": "ok" }] },
            "toolUseResult": { "stdout": "ok", "stderr": "" }
        }));
        assert!(convert_line(&ran, &mut tools)[0].get("diffs").is_none());

        let asked = line(json!({
            "type": "user", "uuid": "u5",
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t5", "content": "Your questions have been answered: \"Which?\"=\"A\"." }] },
            "toolUseResult": { "questions": [], "answers": { "Which?": "A" }, "annotations": {} }
        }));
        assert_eq!(convert_line(&asked, &mut tools)[0]["answers"], json!({ "Which?": "A" }));
    }

    #[test]
    fn bookkeeping_and_sidechains_are_skipped() {
        let mut tools = HashMap::new();
        for l in [
            json!({ "type": "user", "isSidechain": true, "message": { "content": "hi" } }),
            json!({ "type": "user", "isMeta": true, "message": { "content": "[Image: original 10x10]" } }),
            json!({ "type": "user", "isCompactSummary": true, "message": { "content": "This session is being continued" } }),
            json!({ "type": "ai-title", "aiTitle": "x" }),
            json!({ "type": "system", "subtype": "turn_duration", "durationMs": 10 }),
            json!({ "type": "attachment", "attachment": { "type": "total_tokens_reminder" } }),
        ] {
            assert!(convert_line(&l, &mut tools).is_empty(), "{l}");
        }
    }

    #[test]
    fn handed_over_replies_show_as_yours() {
        let mut tools = HashMap::new();
        let stop = json!({
            "type": "user", "isMeta": true, "uuid": "s1",
            "message": { "role": "user", "content": format!("Stop hook feedback:\n{}", handover(&["looks good".into(), "ship it".into()])) }
        });
        let out = convert_line(&stop, &mut tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["text"], "looks good\n\nship it");

        let context = json!({
            "type": "attachment", "uuid": "c1",
            "attachment": { "type": "hook_additional_context", "content": [handover(&["hi".into()])] }
        });
        assert_eq!(convert_line(&context, &mut tools)[0]["content"][0]["text"], "hi");

        // Other hooks' feedback isn't yours.
        let other = json!({ "type": "user", "isMeta": true, "message": { "content": "Stop hook feedback:\nNew from agentbook" } });
        assert!(convert_line(&other, &mut tools).is_empty());
    }

    #[test]
    fn line_meta_finds_titles_turns_and_last_words() {
        assert_eq!(line_meta(&json!({ "type": "ai-title", "aiTitle": "Fix the build" })).ai_title.as_deref(), Some("Fix the build"));
        assert!(line_meta(&json!({ "type": "system", "subtype": "turn_duration" })).turn_end);
        assert_eq!(line_meta(&json!({ "type": "user", "message": { "content": "go" } })).prompt.as_deref(), Some("go"));
        assert_eq!(line_meta(&json!({ "type": "user", "message": { "content": [{ "type": "tool_result" }] } })).prompt, None);
        let said = line_meta(&json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "first" }, { "type": "tool_use" }, { "type": "text", "text": " last " }
        ]}}));
        assert_eq!(said.said.as_deref(), Some("last"));
    }

    #[test]
    fn agents_listing_parses() {
        let out = r#"warning: something
[{"id":"d6e9441e","cwd":"/w","kind":"background","startedAt":5,"sessionId":"s-1","name":"War game","state":"working","status":"busy","pid":42},
 {"pid":7,"cwd":"/x","kind":"interactive","sessionId":"s-2","name":"x-79","status":"idle"},
 {"id":"nope","kind":"background"}]"#;
        let listed = parse_agents(out);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].short_id.as_deref(), Some("d6e9441e"));
        assert!(listed[0].background);
        assert_eq!(listed[0].status.as_deref(), Some("busy"));
        assert!(!listed[1].background);
        assert_eq!(listed[1].pid, Some(7));
        assert!(parse_agents("no sessions").is_empty());
    }

    #[test]
    fn approval_cards_say_what_will_run() {
        assert_eq!(command_text("Bash", &json!({ "command": "rm -rf build", "description": "x" })), "rm -rf build");
        assert_eq!(command_text("Edit", &json!({ "file_path": "/a/b.rs" })), "Edit /a/b.rs");
        assert_eq!(command_text("WebFetch", &json!({ "url": "https://x.dev" })), "WebFetch https://x.dev");
        assert_eq!(command_text("mcp__x__y", &json!({})), "mcp__x__y");
        assert!(command_text("mcp__x__y", &json!({ "a": 1 })).starts_with("mcp__x__y {"));
    }

    #[test]
    fn decisions_map_to_hook_output() {
        assert_eq!(hook_decision(None, None), "");
        let allow: Value = serde_json::from_str(&hook_decision(Some("allow-once"), None)).unwrap();
        assert_eq!(allow["hookSpecificOutput"]["decision"]["behavior"], "allow");
        assert_eq!(allow["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        let rules = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash" }] }]);
        let always: Value = serde_json::from_str(&hook_decision(Some("allow-always"), Some(&rules))).unwrap();
        assert_eq!(always["hookSpecificOutput"]["decision"]["updatedPermissions"], rules);
        let deny: Value = serde_json::from_str(&hook_decision(Some("deny"), None)).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn reads_a_hook_request() {
        let body = r#"{"session_id":"s"}"#;
        let raw = format!(
            "POST /hook/Stop HTTP/1.1\r\nHost: littleguys\r\nX-Claude-Background: 1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut reader = raw.as_bytes();
        let req = read_request(&mut reader).await.unwrap();
        assert_eq!(req.path, "/hook/Stop");
        assert_eq!(req.headers.get("x-claude-background").map(String::as_str), Some("1"));
        assert_eq!(req.body, body.as_bytes());

        let mut out = Vec::new();
        write_response(&mut out, "{}").await.unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("Content-Length: 2\r\nConnection: close\r\n\r\n{}"));
    }

    #[test]
    fn transcripts_are_followed_line_by_line() {
        let dir = std::env::temp_dir().join(format!("lg-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "{\"type\":\"ai-title\",\"aiTitle\":\"T\"}\n{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n{\"type\":\"assi").unwrap();
        let Read::First { tail, lines, .. } = read_transcript(&path, None) else { panic!("first look") };
        assert_eq!(lines.len(), 2);
        assert_eq!(tail.partial, b"{\"type\":\"assi");
        assert!(matches!(read_transcript(&path, Some(tail.clone())), Read::Same));

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, b"stant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"yo\"}]}}\n").unwrap();
        let Read::New { tail, lines, .. } = read_transcript(&path, Some(tail)) else { panic!("new lines") };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "assistant");
        assert!(tail.partial.is_empty());
        assert_eq!(read_history(&path).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_files_point_at_this_apps_socket() {
        let dir = std::env::temp_dir().join(format!("lg-plugin-{}", uuid::Uuid::new_v4()));
        let root = write_plugin_files(&dir).unwrap();
        let hook = std::fs::read_to_string(root.join("plugin/hooks/hook.sh")).unwrap();
        assert!(hook.contains(&format!("sock='{}'", socket_path(&dir).display())));
        assert!(!hook.contains("__LITTLEGUYS_SOCKET__"));
        assert!(root.join(".claude-plugin/marketplace.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_prefer_the_right_name() {
        let mut s = Session::new("s");
        s.cwd = "/Users/me/dev/shop".into();
        assert_eq!(s.title().as_deref(), Some("shop"));
        s.first_prompt = Some("fix the checkout\nand more".into());
        assert_eq!(s.title().as_deref(), Some("fix the checkout"));
        s.name = Some("shop-12".into());
        s.ai_title = Some("Fix checkout totals".into());
        assert_eq!(s.title().as_deref(), Some("Fix checkout totals"));
        s.background = true;
        s.name = Some("checkout fix".into());
        assert_eq!(s.title().as_deref(), Some("checkout fix"));
    }
}
