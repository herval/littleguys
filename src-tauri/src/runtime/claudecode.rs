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
//! - New threads start as drafts (`sessions.create`, or the guy's main key before
//!   it has a session); the first message starts a background session in the
//!   thread's folder: `claude "<text>" --bg`. `--bg` picks the session id, so the
//!   thread keeps the key littleguys gave it, saved in `claude-threads-<id>.json`.
//! - Routines run through a `LocalScheduler`: each run starts a background session
//!   the same way (or writes to the thread it targets) and waits for its turn to end.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify};

use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::{now_ms, write_private};
use crate::gateways::ClaudeCodeConfig;
use crate::openclaw::config::{Credential, GatewayMode};
use crate::openclaw::runtime as cli;
use crate::runtime::acp;
use crate::runtime::scheduler::{LocalScheduler, RoutineRunner, TurnOutcome};
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
/// How long a session `claude --bg` started has to show up in `claude agents`.
const LAUNCH_FIND: Duration = Duration::from_secs(30);
/// A routine's turn gets this long before littleguys stops waiting for it.
const ROUTINE_MAX: Duration = Duration::from_secs(4 * 3600);
/// Sessions littleguys started are remembered (by the key it gave them) this long.
const STARTED_KEEP_MS: u64 = 30 * 24 * 3600 * 1000;
/// …and at most this many routine runs.
const STARTED_MAX: usize = 2_000;
/// Past sessions offered to resume, newest first.
const PAST_MAX: usize = 400;
/// A transcript's start holds its folder and first prompt; its end, its latest title.
const PAST_HEAD_BYTES: u64 = 64 << 10;
const PAST_TAIL_BYTES: u64 = 64 << 10;
const MAX_COMMANDS: usize = 500;
const COMMAND_HEAD_BYTES: u64 = 4 << 10;

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
        routines: true,
        routines_local: true,
        approvals: config.approvals,
        thread_actions: false,
        new_threads: true,
        attachments: true,
        models: false,
        files: true,
        skills: true,
        commands: true,
        tools: false,
        plugins: true,
        browser: false,
        past_sessions: true,
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
    scheduler: Arc<LocalScheduler>,
    /// `<data_dir>/claude-threads-<id>.json`: [`State::started`].
    started_path: PathBuf,
}

#[derive(Default)]
struct State {
    connected: bool,
    sessions: HashMap<String, Session>,
    approvals: HashMap<String, Approval>,
    transcripts: TranscriptIndex,
    /// New threads whose session hasn't started yet, by key.
    drafts: HashMap<String, Draft>,
    /// Sessions littleguys started, by session id: the thread key each one has.
    started: HashMap<String, Started>,
    /// Sessions `claude --bg` just started (by the short id it printed) → their key,
    /// until their session id is known.
    pending: HashMap<String, String>,
}

impl State {
    /// The session `sid`, added if it's new (under the key littleguys gave it, if it started it).
    fn session(&mut self, sid: &str) -> &mut Session {
        if !self.sessions.contains_key(sid) {
            let mut s = Session::new(sid);
            if let Some(t) = self.started.get(sid) {
                s.key = Some(t.key.clone());
                s.kept = t.keep;
                s.cwd = t.cwd.clone().unwrap_or_default();
            }
            if let Some(short) = self.pending.keys().find(|short| s.key.is_none() && sid.starts_with(short.as_str())).cloned() {
                // The session `claude --bg` just started for a thread: the thread is that session now.
                let key = self.pending.remove(&short).unwrap_or_default();
                let keep = self.drafts.contains_key(&key);
                self.started.insert(sid.to_string(), Started { key: key.clone(), at: now_ms(), cwd: None, keep });
                s.kept = keep;
                if let Some(d) = self.drafts.remove(&key) {
                    s.outbox = d.outbox;
                }
                s.key = Some(key);
            }
            self.sessions.insert(sid.to_string(), s);
        }
        self.sessions.get_mut(sid).expect("just added")
    }

    /// The session thread `key` is about, or the draft it still is.
    fn thread(&self, key: &str) -> Option<Thread> {
        if self.drafts.contains_key(key) {
            return Some(Thread::Draft);
        }
        if let Some((sid, _)) = self.started.iter().find(|(_, t)| t.key == key) {
            return Some(Thread::Session(sid.clone()));
        }
        match keys::split(key) {
            Some((AGENT_ID, "main")) => Some(Thread::Draft),
            Some((AGENT_ID, rest)) if rest.starts_with("dashboard:") => keys::last_id(key).map(Thread::Session),
            _ => None,
        }
    }

    /// Where a new session starts unless you pick a folder: the one you last worked in.
    /// (Claude Code won't start a background session in your home folder.)
    fn default_folder(&self) -> String {
        let home = dirs::home_dir().unwrap_or_default();
        let mut recent: Vec<&Session> = self.sessions.values().filter(|s| !s.cwd.is_empty() && !s.cwd.contains("/.claude/")).collect();
        recent.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.started_at)));
        recent
            .into_iter()
            .map(|s| Path::new(&s.cwd))
            .find(|dir| *dir != home && dir.is_dir())
            .unwrap_or(&home)
            .display()
            .to_string()
    }
}

enum Thread {
    Session(String),
    /// Not started yet: its first message starts it.
    Draft,
}

/// A new thread, until its first message starts its session.
struct Draft {
    cwd: String,
    created_at: u64,
    /// Its session is being started.
    launching: bool,
    /// Your messages, for when it has.
    outbox: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Started {
    key: String,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    /// Listed however long ago it last ran: threads you started or opened (not routine runs).
    #[serde(default)]
    keep: bool,
}

fn load_started(path: &Path) -> HashMap<String, Started> {
    let mut started: HashMap<String, Started> =
        std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
    let now = now_ms();
    started.retain(|_, t| now.saturating_sub(t.at) < STARTED_KEEP_MS);
    started
}

/// Remembers `sid` as `thread` and saves; routine runs past the newest `STARTED_MAX` are forgotten.
fn remember(path: &Path, started: &mut HashMap<String, Started>, sid: &str, thread: Started) {
    started.insert(sid.to_string(), thread);
    let runs = started.values().filter(|t| !t.keep).count();
    if runs > STARTED_MAX {
        let mut old: Vec<(String, u64)> = started.iter().filter(|(_, t)| !t.keep).map(|(sid, t)| (sid.clone(), t.at)).collect();
        old.sort_by_key(|(_, at)| *at);
        for (sid, _) in old.into_iter().take(runs - STARTED_MAX) {
            started.remove(&sid);
        }
    }
    save_started(path, started);
}

fn save_started(path: &Path, started: &HashMap<String, Started>) {
    if let Ok(bytes) = serde_json::to_vec_pretty(started) {
        if let Err(e) = write_private(path, &bytes) {
            log::warn!("[claude] couldn't save the threads littleguys started: {e}");
        }
    }
}

#[derive(Default)]
struct Session {
    id: String,
    /// The thread key littleguys gave it, when littleguys started it.
    key: Option<String>,
    /// Listed however long ago it last ran (a thread you started or opened).
    kept: bool,
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
    /// Claude Code is done with it: a background session that's done, or a terminal session
    /// that's exited. Its thread is archived.
    finished: bool,
    /// Your Archive or Unarchive, over `finished` until its next turn.
    archived: Option<bool>,
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
    /// How many times the outbox was handed over (a routine waits for its message's turn).
    handed: u64,
}

impl Session {
    fn new(id: &str) -> Self {
        Self { id: id.to_string(), ..Default::default() }
    }

    fn key(&self) -> String {
        self.key.clone().unwrap_or_else(|| keys::thread(AGENT_ID, &self.id))
    }

    /// The messages waiting for it, handed over.
    fn take_outbox(&mut self) -> Vec<String> {
        self.handed += 1;
        std::mem::take(&mut self.outbox)
    }

    fn run_id(&self) -> String {
        format!("claude-{}-{}", self.id, self.turn)
    }

    fn listable(&self, now: u64) -> bool {
        self.running || self.busy || self.kept || now.saturating_sub(self.updated_at.max(self.started_at)) < LIST_WINDOW_MS
    }

    /// In the archive rather than the thread list. Never while it runs, and never a routine's
    /// run (those stay with their routine).
    fn archived(&self) -> bool {
        let routine_run = self.key.as_deref().and_then(keys::split).is_some_and(|(_, rest)| rest.starts_with("cron:"));
        !self.running && !self.busy && !routine_run && self.archived.unwrap_or(self.finished)
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
            // At work again: whether it's archived is Claude Code's say once more.
            self.archived = None;
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
        let scheduler = LocalScheduler::new(&data_dir, &id);
        let started_path = data_dir.join(format!("claude-threads-{id}.json"));
        let mut st = State { started: load_started(&started_path), ..Default::default() };
        // Threads littleguys started or opened show up again, however old.
        for sid in st.started.keys().cloned().collect::<Vec<_>>() {
            st.session(&sid);
        }
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
                st: Mutex::new(st),
                scheduler,
                started_path,
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
        self.inner.scheduler.start(Arc::new(Runner(Arc::downgrade(&self.inner))));
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move { run(inner).await });
    }

    /// Stop for good (bot removed or turned off).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.scheduler.stop();
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
            m if m.starts_with("cron.") => return inner.scheduler.handle(m, params).await,
            _ => {}
        }
        if !inner.st.lock().unwrap().connected {
            return Err(RpcError::new("NOT_CONNECTED", "not following Claude Code yet"));
        }
        match method {
            "agents.list" => Ok(agents_list(inner)),
            "sessions.list" => Ok(sessions_list(inner, &params)),
            "sessions.create" => sessions_create(inner, &params),
            "sessions.patch" => sessions_patch(inner, &params),
            "chat.history" => chat_history(inner, &params).await,
            "chat.send" => chat_send(inner, &params),
            "chat.abort" => chat_abort(inner, &params).await,
            "sessions.past" => sessions_past(inner).await,
            "sessions.open" => sessions_open(inner, &params),
            // The folder of the thread you're on (`sessionKey`), else the one you last worked in.
            "agents.workspace.list" | "agents.workspace.get" => {
                check_agent(&params)?;
                let root = folder_of(inner, &params);
                let method = method.to_string();
                blocking(move || crate::localfs::handle(&root, AGENT_ID, &method, &params).map(|out| crate::localfs::with_root(out, &root))).await
            }
            "skills.status" => skills_status(inner, &params).await,
            "skills.read" => skills_read(inner, &params).await,
            "commands.list" => commands_list(inner, &params).await,
            "mcp.servers.list" => {
                check_agent(&params)?;
                let cwd = folder_of(inner, &params);
                blocking(move || Ok(json!({ "servers": mcp_servers(&cwd, &home()) }))).await
            }
            "plugins.list" => {
                let cwd = folder_of(inner, &params);
                blocking(move || Ok(json!({ "plugins": acp::read_claude_plugins(&cwd, &home()) }))).await
            }
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
        let s = st.session(&sid);
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
                    reply = handover(&s.take_outbox());
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
                    let reason = handover(&s.take_outbox());
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
    /// A background session's `working|blocked|done|stopped`.
    state: Option<String>,
    waiting_for: Option<String>,
    started_at: u64,
}

impl Listed {
    /// A background session that's done, with no process left. Not one that was stopped: that
    /// can be mid-turn, or with a message it never got to.
    fn finished(&self) -> bool {
        self.pid.is_none() && self.state.as_deref() == Some("done")
    }
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
                state: text(row, "state"),
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
            let s = st.session(&l.session_id);
            changed |= !known
                || s.running != running
                || s.finished != l.finished()
                || s.name != l.name
                || (s.waiting_for != l.waiting_for && !s.hooked);
            s.finished = l.finished();
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
        for s in st.sessions.values_mut().filter(|s| !ids.contains(s.id.as_str())) {
            if s.running && s.last_hook.is_none_or(|t| t.elapsed() > HOOK_SETTLE) {
                s.running = false;
                s.set_busy(false, &mut emits);
                changed = true;
            }
            if !s.running && !s.finished {
                s.finished = true;
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
    let folder = inner.st.lock().unwrap().default_folder();
    json!({
        "defaultId": AGENT_ID,
        "mainKey": "main",
        "agents": [{
            "id": AGENT_ID,
            "name": name,
            "identity": { "name": name, "avatarUrl": super::claude_face() },
            // Where new threads start (they can pick another folder).
            "workspace": folder,
        }],
    })
}

fn sessions_list(inner: &Inner, params: &Value) -> Value {
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
    let archived = super::archived_filter(params);
    let now = now_ms();
    let st = inner.st.lock().unwrap();
    let mut sessions: Vec<&Session> = st
        .sessions
        .values()
        .filter(|s| s.listable(now) && (s.running || s.transcript.is_some()) && archived.is_none_or(|a| a == s.archived()))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.started_at)));
    sessions.truncate(limit);
    // Drafts are never archived.
    let drafts = st.drafts.iter().filter(|_| archived != Some(true)).map(|(key, d)| {
        json!({
            "key": key,
            "agentId": AGENT_ID,
            "kind": "direct",
            "workspaceDir": d.cwd,
            "createdAt": d.created_at,
            "updatedAt": d.created_at,
            "unread": false,
            "pinned": false,
            "archived": false,
            "hasActiveRun": d.launching,
            "status": if d.launching { "running" } else { "done" },
        })
    });
    let rows: Vec<Value> = drafts.chain(sessions.iter().map(|s| session_row(s))).collect();
    json!({ "count": rows.len(), "sessions": rows })
}

fn session_row(s: &Session) -> Value {
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
        "archived": s.archived(),
        "hasActiveRun": s.busy,
        "status": if s.busy { "running" } else { "done" },
    })
}

/// Archive or Unarchive a session's thread, over what Claude Code says until its next turn.
/// (Sending in an archived thread unarchives it first.)
fn sessions_patch(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let Some(archived) = params.get("archived").and_then(Value::as_bool) else {
        return Err(unsupported("sessions.patch"));
    };
    let (key, thread) = thread_of(inner, params)?;
    let row = {
        let mut st = inner.st.lock().unwrap();
        match thread {
            Thread::Draft => return Err(RpcError::new("INVALID_REQUEST", "This thread hasn't started yet.")),
            Thread::Session(sid) => {
                let s = st.sessions.get_mut(&sid).ok_or_else(|| RpcError::new("NOT_FOUND", "Not a Claude Code session."))?;
                s.archived = Some(archived);
                session_row(s)
            }
        }
    };
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "patch" }));
    Ok(row)
}

/// The thread a call is about.
fn thread_of(inner: &Inner, params: &Value) -> Result<(String, Thread), RpcError> {
    let key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?;
    let thread = inner.st.lock().unwrap().thread(key).ok_or_else(|| RpcError::new("NOT_FOUND", "Not a Claude Code session."))?;
    Ok((key.to_string(), thread))
}

/// A new thread: a draft until its first message starts its session.
fn sessions_create(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    if let Some(agent) = params.get("agentId").and_then(Value::as_str).filter(|a| *a != AGENT_ID) {
        return Err(RpcError::new("NOT_FOUND", format!("no guy {agent} here")));
    }
    let key = keys::thread(AGENT_ID, &uuid::Uuid::new_v4().to_string());
    {
        let mut st = inner.st.lock().unwrap();
        let cwd = st.default_folder();
        st.drafts.insert(key.clone(), Draft { cwd, created_at: now_ms(), launching: false, outbox: Vec::new() });
    }
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "created" }));
    Ok(json!({ "ok": true, "key": key }))
}

async fn chat_history(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    let Thread::Session(sid) = thread else {
        return Ok(json!({ "sessionKey": key, "messages": [], "hasMore": false, "inFlightRun": Value::Null }));
    };
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
/// A new thread's first message starts its session (in `cwd`, when you picked a folder).
fn chat_send(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    let text = params.get("message").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    let attachments = params.get("attachments").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    if text.is_empty() && attachments.is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "Write something first."));
    }
    let text = with_attachments(&inner.data_dir, text, attachments)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let sid = match thread {
        Thread::Session(sid) => sid,
        Thread::Draft => {
            let cwd = params.get("cwd").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
            let start = {
                let mut st = inner.st.lock().unwrap();
                let fallback = st.default_folder();
                let d = st.drafts.entry(key.clone()).or_insert_with(|| Draft { cwd: fallback, created_at: now_ms(), launching: false, outbox: Vec::new() });
                if let Some(cwd) = cwd.filter(|_| !d.launching) {
                    d.cwd = cwd;
                }
                d.outbox.push(text);
                !std::mem::replace(&mut d.launching, true)
            };
            if start {
                let inner = inner.clone();
                tauri::async_runtime::spawn(async move { start_draft(inner, key).await });
            }
            return Ok(json!({ "runId": run_id, "status": "started" }));
        }
    };
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
            let text = s.take_outbox().join("\n\n");
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
    // No other flags: a background session keeps the options it started with, and any
    // passed here would start a copy of it instead.
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

// ---- new sessions -------------------------------------------------------------------------

/// Starts a draft's session with what you wrote; the thread is that session from then on.
async fn start_draft(inner: Arc<Inner>, key: String) {
    let job = {
        let mut st = inner.st.lock().unwrap();
        st.drafts.get_mut(&key).map(|d| (std::mem::take(&mut d.outbox).join("\n\n"), d.cwd.clone()))
    };
    let Some((text, cwd)) = job else { return };
    let result = launch(&inner, &key, &cwd, &text, None, true).await;
    let mut emits: Emits = vec![refresh()];
    {
        let mut st = inner.st.lock().unwrap();
        match result {
            Ok(sid) => {
                // What you wrote while it started waits for its turn to end.
                if let Some(d) = st.drafts.remove(&key) {
                    st.session(&sid).outbox.extend(d.outbox);
                }
            }
            Err(e) => {
                log::warn!("[claude] couldn't start a session in {cwd}: {e}");
                if let Some(d) = st.drafts.get_mut(&key) {
                    d.launching = false;
                    d.outbox.clear();
                }
                let run_id = uuid::Uuid::new_v4().to_string();
                emits.push(("chat", json!({ "sessionKey": key, "runId": run_id, "state": "error", "errorMessage": format!("Couldn't start a session: {e}") })));
            }
        }
    }
    flush(&inner, emits);
    inner.poke.store(true, Ordering::SeqCst);
}

/// `claude "<text>" --bg` in `cwd`: a new background session, shown as thread `key`.
/// Resolves to its session id once Claude Code lists it.
async fn launch(inner: &Arc<Inner>, key: &str, cwd: &str, text: &str, name: Option<&str>, keep: bool) -> Result<String, String> {
    let claude = inner.claude.lock().unwrap().clone().ok_or("Claude Code isn't installed")?;
    if !Path::new(cwd).is_dir() {
        return Err(format!("{cwd} isn't a folder on this Mac."));
    }
    // A prompt starting with '-' would read as a flag.
    let prompt = if text.starts_with('-') { format!(" {text}") } else { text.to_string() };
    let mut cmd = cli::command(&claude);
    cmd.arg(&prompt).arg("--bg").current_dir(cwd);
    add_attachments_dir(&mut cmd, &inner.data_dir);
    if let Some(name) = name {
        cmd.args(["--name", name]);
    }
    let out = output(cmd, Duration::from_secs(60)).await?;
    let short = backgrounded_id(&out).ok_or_else(|| format!("Claude Code didn't say which session it started ({})", first_line(&out)))?;
    inner.st.lock().unwrap().pending.insert(short.clone(), key.to_string());
    let found = find_session(inner, &claude, &short).await;
    let mut emits: Emits = vec![refresh()];
    let result = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        match found {
            // Still pending: it gets its key when it reports.
            None => Err("it started, but hasn't shown up in `claude agents` yet.".to_string()),
            Some(sid) => {
                st.pending.remove(&short);
                let thread = Started { key: key.to_string(), at: now_ms(), cwd: Some(cwd.to_string()), keep };
                remember(&inner.started_path, &mut st.started, &sid, thread);
                let s = st.session(&sid);
                s.kept = keep;
                if s.key.as_deref() != Some(key) {
                    // Its hooks reported before littleguys knew it: show it under its key, from the top.
                    s.key = Some(key.to_string());
                    s.tail = Some(Tail::default());
                }
                s.tail.get_or_insert_with(Tail::default);
                s.background = true;
                s.short_id = Some(short.clone());
                s.running = true;
                if s.cwd.is_empty() {
                    s.cwd = cwd.to_string();
                }
                if s.name.is_none() {
                    s.name = name.map(str::to_string);
                }
                let now = now_ms();
                if s.started_at == 0 {
                    s.started_at = now;
                }
                s.updated_at = s.updated_at.max(now);
                if s.turn == 0 {
                    // Its hooks take a moment to report; don't let `claude agents` say otherwise meanwhile.
                    s.last_hook = Some(Instant::now());
                    s.set_busy(true, &mut emits);
                }
                Ok(sid)
            }
        }
    };
    flush(inner, emits);
    result
}

/// The id of the session `claude --bg` printed `short` for: from its hooks, or `claude agents`.
async fn find_session(inner: &Inner, claude: &Path, short: &str) -> Option<String> {
    let deadline = Instant::now() + LAUNCH_FIND;
    loop {
        let hooked = inner.st.lock().unwrap().sessions.keys().find(|sid| sid.starts_with(short)).cloned();
        if hooked.is_some() {
            return hooked;
        }
        let listed = list_agents(claude).await.and_then(|all| all.into_iter().find(|l| l.short_id.as_deref() == Some(short)));
        if let Some(l) = listed {
            return Some(l.session_id);
        }
        if Instant::now() > deadline || inner.stopped.load(Ordering::SeqCst) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The short id `claude --bg` prints: `backgrounded · 31009426 · <name>`, then
/// `claude attach 31009426` among its hints.
fn backgrounded_id(stdout: &str) -> Option<String> {
    let id = |token: Option<&str>| token.filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric())).map(str::to_string);
    let lines = || stdout.lines().map(str::trim);
    lines()
        .find_map(|l| id(l.strip_prefix("backgrounded")?.trim_start_matches(|c: char| c.is_whitespace() || c == '·').split_whitespace().next()))
        .or_else(|| lines().find_map(|l| id(l.strip_prefix("claude attach ")?.split_whitespace().next())))
}

// ---- routines -----------------------------------------------------------------------------

struct Runner(Weak<Inner>);

impl RoutineRunner for Runner {
    /// A new background session for each run (in the routine's folder, `payload.cwd`),
    /// or a message to the thread it targets; resolves when that turn ends.
    fn run_turn(&self, agent_id: String, target: String, job_id: String, message: String) -> BoxFuture<'static, Result<TurnOutcome, String>> {
        let inner = self.0.clone();
        Box::pin(async move {
            let inner = inner.upgrade().ok_or("littleguys is shutting down")?;
            if agent_id != AGENT_ID {
                return Err(format!("no guy {agent_id} here"));
            }
            if !inner.st.lock().unwrap().connected {
                return Err("littleguys isn't following Claude Code right now.".into());
            }
            let (key, sid, done): (String, String, Box<dyn Fn(&Session) -> bool + Send + Sync>) = match target.strip_prefix("session:") {
                None => {
                    let job = inner.scheduler.handle("cron.get", json!({ "id": job_id })).await.ok();
                    let text = |p: &str| job.as_ref().and_then(|j| j.pointer(p)).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                    let cwd = text("/payload/cwd").unwrap_or_else(|| inner.st.lock().unwrap().default_folder());
                    let key = keys::routine_run(AGENT_ID, &job_id, &uuid::Uuid::new_v4().to_string());
                    let sid = launch(&inner, &key, &cwd, &message, text("/name").as_deref(), false).await?;
                    (key, sid, Box::new(|s: &Session| s.turn > 0 && !s.busy))
                }
                Some(key) => {
                    let (sid, before) = {
                        let st = inner.st.lock().unwrap();
                        let sid = match st.thread(key) {
                            Some(Thread::Session(sid)) => sid,
                            Some(Thread::Draft) => return Err("That thread hasn't started yet. Write to it once, then this routine can continue it.".into()),
                            None => return Err("That thread is gone.".into()),
                        };
                        let s = st.sessions.get(&sid);
                        if s.is_some_and(|s| s.running && !s.background) {
                            return Err("That session is open in a terminal, so only you can write to it.".into());
                        }
                        (sid.clone(), s.map_or(0, |s| s.handed))
                    };
                    chat_send(&inner, &json!({ "sessionKey": key, "message": message })).map_err(|e| e.message)?;
                    (key.to_string(), sid, Box::new(move |s: &Session| s.handed > before && !s.busy && !s.delivering))
                }
            };
            let said = wait_turn(&inner, &sid, done.as_ref()).await?;
            Ok(TurnOutcome { session_key: key, summary: said.map(|t| truncate(&t, 300)) })
        })
    }

    fn emit(&self, name: &str, payload: Value) {
        if let Some(inner) = self.0.upgrade() {
            emit(&inner, name, payload);
        }
    }
}

/// Waits for a routine's turn to end (`done`); the session's last words.
async fn wait_turn(inner: &Inner, sid: &str, done: &(dyn Fn(&Session) -> bool + Send + Sync)) -> Result<Option<String>, String> {
    let deadline = Instant::now() + ROUTINE_MAX;
    loop {
        {
            let st = inner.st.lock().unwrap();
            let s = st.sessions.get(sid).ok_or("littleguys lost track of the session")?;
            if done(s) {
                return Ok(s.said.clone());
            }
        }
        if inner.stopped.load(Ordering::SeqCst) {
            return Err("littleguys stopped following Claude Code before it finished.".into());
        }
        if Instant::now() > deadline {
            return Err(format!("Still going after {} hours, so littleguys stopped waiting.", ROUTINE_MAX.as_secs() / 3600));
        }
        tokio::time::sleep(TICK).await;
    }
}

// ---- stop, past sessions ------------------------------------------------------------------

/// Stops a background session (`claude stop`); writing to it again resumes it.
async fn chat_abort(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    let Thread::Session(sid) = thread else { return Ok(json!({ "ok": true, "aborted": false, "runIds": [] })) };
    let (short, background, live, run_id) = {
        let st = inner.st.lock().unwrap();
        let s = st.sessions.get(&sid).ok_or_else(|| RpcError::new("NOT_FOUND", "littleguys lost track of this session."))?;
        (s.short_id.clone(), s.background, s.running || s.busy, s.run_id())
    };
    if !live {
        return Ok(json!({ "ok": true, "aborted": false, "runIds": [] }));
    }
    let short = match short {
        Some(short) if background => short,
        _ => return Err(RpcError::new("UNSUPPORTED", "This session runs in a terminal: press Esc there to stop it.")),
    };
    let claude = inner.claude.lock().unwrap().clone().ok_or_else(|| RpcError::new("UNAVAILABLE", "Claude Code isn't installed"))?;
    run_cli(&claude, &["stop", &short], CLI_TIMEOUT).await.map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't stop it: {e}")))?;
    let mut emits: Emits = vec![refresh()];
    {
        let mut st = inner.st.lock().unwrap();
        if let Some(s) = st.sessions.get_mut(&sid) {
            s.running = false;
            s.waiting_for = None;
            s.set_busy(false, &mut emits);
        }
    }
    flush(inner, emits);
    emit(inner, "chat", json!({ "sessionKey": key, "runId": run_id, "state": "aborted" }));
    Ok(json!({ "ok": true, "aborted": true, "runIds": [run_id] }))
}

/// Every session Claude Code kept a transcript of, newest first (the newest `PAST_MAX`).
async fn sessions_past(inner: &Arc<Inner>) -> Result<Value, RpcError> {
    let projects = claude_home().join("projects");
    let found = blocking(move || Ok(past_sessions(&projects, PAST_MAX))).await?;
    let st = inner.st.lock().unwrap();
    let now = now_ms();
    let rows: Vec<Value> = found
        .into_iter()
        .map(|p| {
            let known = st.sessions.get(&p.session_id);
            let key = known.map_or_else(|| keys::thread(AGENT_ID, &p.session_id), Session::key);
            json!({
                "sessionId": p.session_id,
                "cwd": p.cwd,
                "title": known.and_then(Session::title).or(p.title),
                "updatedAt": p.updated_at,
                "key": key,
                "inThreads": known.is_some_and(|s| s.listable(now) && (s.running || s.transcript.is_some())),
            })
        })
        .collect();
    Ok(json!({ "sessions": rows }))
}

/// A past session, as a thread that stays in the list.
fn sessions_open(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionId is required"))?
        .to_string();
    let cwd = params.get("cwd").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
    let key = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let s = st.session(&sid);
        if s.cwd.is_empty() {
            s.cwd = cwd.clone().unwrap_or_default();
        }
        s.kept = true;
        let updated = params.get("updatedAt").and_then(Value::as_u64).unwrap_or_default();
        s.updated_at = s.updated_at.max(updated);
        let (key, cwd) = (s.key(), (!s.cwd.is_empty()).then(|| s.cwd.clone()));
        let at = now_ms();
        let thread = st.started.get(&sid).cloned().map_or(Started { key: key.clone(), at, cwd: cwd.clone(), keep: true }, |t| Started { keep: true, ..t });
        remember(&inner.started_path, &mut st.started, &sid, thread);
        key
    };
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "opened" }));
    Ok(json!({ "key": key }))
}

#[derive(Debug, PartialEq)]
struct PastSession {
    session_id: String,
    cwd: String,
    title: Option<String>,
    updated_at: u64,
}

/// `~/.claude/projects/*/<sessionId>.jsonl`, newest first: each one's folder (from its
/// lines), and its title (the last AI title, else its first prompt).
fn past_sessions(projects: &Path, max: usize) -> Vec<PastSession> {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    for dir in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let modified = file.metadata().map(|m| modified_ms(&m)).unwrap_or(0);
                files.push((path, modified));
            }
        }
    }
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    files
        .into_iter()
        .filter_map(|(path, updated_at)| {
            let session_id = path.file_stem()?.to_str()?.to_string();
            let len = std::fs::metadata(&path).ok()?.len();
            let (head, _, _) = read_lines(&path, 0, len.min(PAST_HEAD_BYTES), Vec::new(), false).ok()?;
            let cwd = head.iter().find_map(|l| l.get("cwd").and_then(Value::as_str).map(str::to_string))?;
            let from = len.saturating_sub(PAST_TAIL_BYTES);
            let (tail, _, _) = read_lines(&path, from, len, Vec::new(), from > 0).unwrap_or_default();
            let title = tail
                .iter()
                .rev()
                .find_map(|l| line_meta(l).ai_title)
                .or_else(|| head.iter().find_map(|l| line_meta(l).prompt).map(|p| truncate(first_line(&p), 80)));
            Some(PastSession { session_id, cwd, title, updated_at })
        })
        .take(max)
        .collect()
}

// ---- attachments ------------------------------------------------------------------------

/// Where attached files are saved; sessions get `--add-dir` to it so they can read them.
fn attachments_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("claude-attachments")
}

/// Saves a message's attachments and lists them after its text, the way Claude Code
/// reads attached files (and littleguys shows them): "Attached files:", `- name: /path`.
fn with_attachments(data_dir: &Path, text: &str, attachments: &[Value]) -> Result<String, RpcError> {
    use base64::Engine;
    if attachments.is_empty() {
        return Ok(text.to_string());
    }
    let dir = attachments_dir(data_dir).join(format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().to_string()[..8]));
    std::fs::create_dir_all(&dir).map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't save the attachments: {e}")))?;
    let mut list = Vec::new();
    for (i, attachment) in attachments.iter().enumerate() {
        let raw = attachment.get("content").or_else(|| attachment.get("data")).and_then(Value::as_str).unwrap_or_default();
        let data = raw.split_once(";base64,").map_or(raw, |(_, d)| d);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| RpcError::new("INVALID_REQUEST", "An attachment isn't valid base64."))?;
        let name = attachment.get("fileName").and_then(Value::as_str).map(safe_file_name).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("attachment-{}", i + 1));
        let path = dir.join(&name);
        std::fs::write(&path, bytes).map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't save {name}: {e}")))?;
        list.push(format!("- {name}: {}", path.display()));
    }
    let text = text.trim();
    let gap = if text.is_empty() { "" } else { "\n\n" };
    Ok(format!("{text}{gap}Attached files:\n{}", list.join("\n")))
}

/// Lets a session littleguys starts read the files attached to your messages (from the
/// start: resuming keeps the options a background session started with).
fn add_attachments_dir(cmd: &mut tokio::process::Command, data_dir: &Path) {
    let dir = attachments_dir(data_dir);
    if std::fs::create_dir_all(&dir).is_ok() {
        cmd.arg("--add-dir").arg(dir);
    }
}

/// A file name without folders, control characters or a leading dot.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    base.chars().filter(|c| !c.is_control()).collect::<String>().trim().trim_start_matches('.').to_string()
}

// ---- the folder you work in: skills, commands, MCP servers, plugins -----------------------

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/var/empty"))
}

/// Run file work off the async runtime.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work).await.map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
}

/// `agentId`, when given, must be this guy's.
fn check_agent(params: &Value) -> Result<(), RpcError> {
    match params.get("agentId").and_then(Value::as_str) {
        Some(id) if id != AGENT_ID => Err(RpcError::new("NOT_FOUND", format!("no guy {id} here"))),
        _ => Ok(()),
    }
}

/// The folder a call is about: its thread's, else the one you last worked in.
fn folder_of(inner: &Inner, params: &Value) -> PathBuf {
    let st = inner.st.lock().unwrap();
    let key = params.get("sessionKey").and_then(Value::as_str);
    let cwd = key.and_then(|key| match st.thread(key)? {
        Thread::Session(sid) => st.sessions.get(&sid).map(|s| s.cwd.clone()),
        Thread::Draft => st.drafts.get(key).map(|d| d.cwd.clone()),
    });
    PathBuf::from(cwd.filter(|c| Path::new(c).is_dir()).unwrap_or_else(|| st.default_folder()))
}

/// Plugins Claude Code has on for `cwd`: (name, install folder).
fn plugin_folders(cwd: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let Some(installed) = acp::read_json(&home.join(".claude/plugins/installed_plugins.json")) else { return Vec::new() };
    let here = cwd.display().to_string();
    acp::read_claude_plugins(cwd, home)
        .into_iter()
        .filter(|p| p["enabled"] == true)
        .filter_map(|p| {
            let entries = installed.get("plugins")?.get(p["id"].as_str()?)?;
            let entries: Vec<&Value> = match entries {
                Value::Array(list) => list.iter().collect(),
                other => vec![other],
            };
            let entry = entries.into_iter().find(|e| e.get("projectPath").and_then(Value::as_str).is_none_or(|p| p == here))?;
            Some((p["name"].as_str()?.to_string(), PathBuf::from(entry.get("installPath")?.as_str()?)))
        })
        .collect()
}

/// Every skill Claude Code loads in `cwd`: the folder's, yours, then your plugins' (`plugin:skill`).
fn skills_in(cwd: &Path, home: &Path) -> Vec<acp::SkillFile> {
    let mut skills = acp::scan_skills(&acp::claude_skill_dirs(cwd, home));
    for (plugin, dir) in plugin_folders(cwd, home) {
        for mut skill in acp::scan_skills(&[(dir.join("skills"), "plugin")]) {
            skill.name = format!("{plugin}:{}", skill.name);
            skills.push(skill);
        }
    }
    skills
}

/// A custom slash command: a markdown file under a `commands` folder.
struct CommandFile {
    name: String,
    description: Option<String>,
    /// `project`, `personal` or `plugin`.
    source: &'static str,
    path: PathBuf,
}

/// `.claude/commands/**/*.md` (the folder's, then yours) and your plugins' `commands/`.
/// Subfolders and plugins namespace them: `frontend/component.md` → `frontend:component`.
fn commands_in(cwd: &Path, home: &Path) -> Vec<CommandFile> {
    let mut roots: Vec<(PathBuf, Option<String>, &'static str)> =
        vec![(cwd.join(".claude/commands"), None, "project"), (home.join(".claude/commands"), None, "personal")];
    roots.extend(plugin_folders(cwd, home).into_iter().map(|(plugin, dir)| (dir.join("commands"), Some(plugin), "plugin")));
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (root, plugin, source) in roots {
        let mut stack = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten().take(MAX_COMMANDS) {
                let path = entry.path();
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    if depth < 2 {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let rel = path.strip_prefix(&root).unwrap_or(&path).with_extension("");
                let local: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                let name = plugin.iter().cloned().chain(local).collect::<Vec<_>>().join(":");
                if found.len() >= MAX_COMMANDS || !seen.insert(name.clone()) {
                    continue;
                }
                let head = acp::read_head(&path, COMMAND_HEAD_BYTES).unwrap_or_default();
                let description = acp::front_matter(&head).1.or_else(|| {
                    let body = if head.starts_with("---") { head.splitn(3, "---").nth(2).unwrap_or_default() } else { &head };
                    Some(truncate(first_line(body), 120)).filter(|l| !l.is_empty())
                });
                found.push(CommandFile { name, description, source, path });
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Built-in commands that make sense sent as a message.
const BUILT_IN_COMMANDS: &[(&str, &str)] = &[("compact", "Summarize the conversation so far, to free up context")];

/// Skills, then slash commands (marked `source: "command"`).
async fn skills_status(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    check_agent(params)?;
    let cwd = folder_of(inner, params);
    blocking(move || {
        let home = home();
        let mut rows: Vec<Value> = skills_in(&cwd, &home).iter().map(acp::SkillFile::row).collect();
        rows.extend(commands_in(&cwd, &home).iter().map(|c| {
            json!({
                "name": c.name,
                "skillKey": format!("cmd:{}", c.name),
                "description": c.description.clone().unwrap_or_default(),
                "source": "command",
                "eligible": true,
                "disabled": false,
                "filePath": c.path.display().to_string(),
            })
        }));
        Ok(json!({ "agentId": AGENT_ID, "skills": rows, "workspaceDir": cwd.display().to_string() }))
    })
    .await
}

async fn skills_read(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    check_agent(params)?;
    let name = params
        .get("name")
        .or_else(|| params.get("skillKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "name is required"))?
        .to_string();
    let cwd = folder_of(inner, params);
    blocking(move || {
        let home = home();
        let path = match name.strip_prefix("cmd:") {
            Some(command) => commands_in(&cwd, &home).into_iter().find(|c| c.name == command).map(|c| c.path),
            None => skills_in(&cwd, &home).into_iter().find(|s| s.name == name).map(|s| s.path),
        };
        let path = path.ok_or_else(|| RpcError::new("INVALID_REQUEST", format!("There's no skill called {name}.")))?;
        let content = acp::read_capped(&path, acp::MAX_SKILL_BYTES)
            .ok_or_else(|| RpcError::new("INVALID_REQUEST", format!("{} is too big (or unreadable) to show.", path.display())))?;
        Ok(json!({ "name": name.trim_start_matches("cmd:"), "path": path.display().to_string(), "content": content }))
    })
    .await
}

/// What the composer offers after `/`: skills, custom commands, and the built-ins that work here.
async fn commands_list(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    check_agent(params)?;
    let cwd = folder_of(inner, params);
    blocking(move || {
        let home = home();
        let entry = |name: &str, description: &str, source: &str| {
            json!({ "name": name, "description": description, "source": source, "scope": "text", "acceptsArgs": true })
        };
        let mut commands: Vec<Value> = BUILT_IN_COMMANDS.iter().map(|(name, description)| entry(name, description, "native")).collect();
        commands.extend(commands_in(&cwd, &home).iter().map(|c| {
            entry(&c.name, c.description.as_deref().unwrap_or_default(), if c.source == "plugin" { "plugin" } else { "native" })
        }));
        commands.extend(skills_in(&cwd, &home).iter().map(|s| entry(&s.name, s.description.as_deref().unwrap_or_default(), "skill")));
        Ok(json!({ "commands": commands }))
    })
    .await
}

/// Claude Code's MCP servers for `cwd`, then your plugins' (`plugin:server`).
fn mcp_servers(cwd: &Path, home: &Path) -> Vec<Value> {
    let mut servers = acp::claude_mcp_servers(cwd, home);
    for (plugin, dir) in plugin_folders(cwd, home) {
        let Some(config) = acp::read_json(&dir.join(".mcp.json")) else { continue };
        let specs = config.get("mcpServers").unwrap_or(&config);
        for mut row in acp::mcp_rows(Some(specs), "plugin", "http") {
            row["name"] = json!(format!("{plugin}:{}", row["name"].as_str().unwrap_or_default()));
            servers.push(row);
        }
    }
    servers
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
    fn only_done_background_sessions_are_finished() {
        let listed = parse_agents(
            r#"[{"kind":"background","sessionId":"done","state":"done"},
 {"kind":"background","sessionId":"stopped","state":"stopped"},
 {"kind":"background","sessionId":"blocked","state":"blocked"},
 {"kind":"background","sessionId":"resumed","state":"done","status":"busy","pid":9},
 {"kind":"interactive","sessionId":"terminal","status":"idle","pid":7}]"#,
        );
        let finished: Vec<&str> = listed.iter().filter(|l| l.finished()).map(|l| l.session_id.as_str()).collect();
        assert_eq!(finished, ["done"]);
    }

    #[tokio::test]
    async fn finished_sessions_are_archived_until_you_bring_them_back() {
        let dir = std::env::temp_dir().join(format!("lg-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = Hooks { status: Box::new(|_| {}), event: Box::new(|_, _, _| {}), connected: Box::new(|| {}) };
        let client = ClaudeCodeClient::new("t".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks);
        let run = keys::routine_run(AGENT_ID, "job", "r-1");
        {
            let mut st = client.inner.st.lock().unwrap();
            st.connected = true;
            st.started.insert("run".into(), Started { key: run.clone(), at: now_ms(), ..Default::default() });
            for (sid, finished) in [("done", true), ("blocked", false), ("run", true)] {
                let s = st.session(sid);
                s.finished = finished;
                s.started_at = now_ms();
                s.transcript = Some(dir.join(format!("{sid}.jsonl")));
            }
        }
        let keys_of = |listed: Value| -> Vec<String> {
            let mut keys: Vec<String> = listed["sessions"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap().to_string()).collect();
            keys.sort();
            keys
        };
        let done = keys::thread(AGENT_ID, "done");
        let blocked = keys::thread(AGENT_ID, "blocked");
        // A routine's run stays with its routine.
        assert_eq!(keys_of(client.call("sessions.list", json!({})).await.unwrap()), [run.clone(), blocked.clone()]);
        let archived = client.call("sessions.list", json!({ "archived": true })).await.unwrap();
        assert_eq!(keys_of(archived.clone()), [done.clone()]);
        assert_eq!(archived["sessions"][0]["archived"], true);
        assert_eq!(keys_of(client.call("sessions.list", json!({ "archived": "all" })).await.unwrap()).len(), 3);

        let row = client.call("sessions.patch", json!({ "key": done, "archived": false })).await.unwrap();
        assert_eq!(row["archived"], false);
        assert!(keys_of(client.call("sessions.list", json!({})).await.unwrap()).contains(&done));

        // Its next turn hands it back to Claude Code: archived again once it's done.
        let mut emits = Vec::new();
        let mut st = client.inner.st.lock().unwrap();
        let s = st.sessions.get_mut("done").unwrap();
        s.set_busy(true, &mut emits);
        assert!(!s.archived());
        s.set_busy(false, &mut emits);
        assert!(s.archived());
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backgrounded_sessions_say_their_short_id() {
        let out = "warning: --bg manages the session id\nbackgrounded · 31009426 · littleguys probe\n  claude agents             list sessions\n  claude attach 31009426    open in this terminal\n";
        assert_eq!(backgrounded_id(out).as_deref(), Some("31009426"));
        assert_eq!(backgrounded_id("backgrounded · ab12cd34").as_deref(), Some("ab12cd34"));
        assert_eq!(backgrounded_id("  claude attach 0f0f0f0f    open in this terminal").as_deref(), Some("0f0f0f0f"));
        assert_eq!(backgrounded_id("Workspace not trusted."), None);
    }

    #[test]
    fn threads_resolve_to_drafts_and_the_sessions_littleguys_started() {
        let mut st = State::default();
        let draft = keys::thread(AGENT_ID, "d-1");
        st.drafts.insert(draft.clone(), Draft { cwd: "/w".into(), created_at: 1, launching: false, outbox: Vec::new() });
        assert!(matches!(st.thread(&draft), Some(Thread::Draft)));
        // The guy's main key is a draft until a session has it.
        let main = keys::main(AGENT_ID);
        assert!(matches!(st.thread(&main), Some(Thread::Draft)));
        st.started.insert("s-main".into(), Started { key: main.clone(), at: 1, ..Default::default() });
        assert!(matches!(st.thread(&main), Some(Thread::Session(sid)) if sid == "s-main"));
        // A routine run's key, and any session's own.
        let run = keys::routine_run(AGENT_ID, "job", "r-1");
        st.started.insert("s-run".into(), Started { key: run.clone(), at: 1, ..Default::default() });
        assert!(matches!(st.thread(&run), Some(Thread::Session(sid)) if sid == "s-run"));
        assert!(matches!(st.thread(&keys::thread(AGENT_ID, "s-9")), Some(Thread::Session(sid)) if sid == "s-9"));
        assert!(st.thread("agent:other:dashboard:x").is_none());
        assert!(st.thread(&keys::routine_run(AGENT_ID, "job", "unknown")).is_none());
    }

    #[test]
    fn started_sessions_keep_their_thread_key() {
        let mut st = State::default();
        let key = keys::thread(AGENT_ID, "draft");
        st.started.insert("s-1".into(), Started { key: key.clone(), at: 1, ..Default::default() });
        assert_eq!(st.session("s-1").key(), key);
        // Reported before its id was known: matched by the short id `claude --bg` printed,
        // it takes over its draft (and what you wrote meanwhile).
        let draft = keys::thread(AGENT_ID, "d-2");
        st.drafts.insert(draft.clone(), Draft { cwd: "/w".into(), created_at: 1, launching: true, outbox: vec!["and then".into()] });
        st.pending.insert("31009426".into(), draft.clone());
        let sid = "31009426-f4da-4001-87f2-0d40829fb8af";
        assert_eq!(st.session(sid).key(), draft);
        assert_eq!(st.session(sid).outbox, vec!["and then".to_string()]);
        assert!(st.pending.is_empty() && st.drafts.is_empty());
        assert!(matches!(st.thread(&draft), Some(Thread::Session(s)) if s == sid));
        assert_eq!(st.session("other").key(), keys::thread(AGENT_ID, "other"));
    }

    #[test]
    fn new_sessions_start_where_you_last_worked() {
        let dir = std::env::temp_dir().join(format!("lg-folder-{}", uuid::Uuid::new_v4()));
        let (old, new) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let mut st = State::default();
        for (sid, cwd, at) in [("a", &old, 10), ("b", &new, 20), ("c", &dir.join("gone"), 30)] {
            let s = st.session(sid);
            s.cwd = cwd.display().to_string();
            s.updated_at = at;
        }
        let home = dirs::home_dir().unwrap();
        st.session("d").cwd = home.display().to_string();
        st.session("d").updated_at = 40;
        assert_eq!(st.default_folder(), new.display().to_string());
        assert_eq!(State::default().default_folder(), home.display().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn started_threads_are_saved_and_old_ones_dropped() {
        let path = std::env::temp_dir().join(format!("lg-started-{}.json", uuid::Uuid::new_v4()));
        let now = now_ms();
        let started = HashMap::from([
            ("new".to_string(), Started { key: "agent:claude:main".into(), at: now, ..Default::default() }),
            ("old".to_string(), Started { key: "agent:claude:dashboard:x".into(), at: now - STARTED_KEEP_MS - 1, ..Default::default() }),
        ]);
        save_started(&path, &started);
        let loaded = load_started(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["new"].key, "agent:claude:main");
        let _ = std::fs::remove_file(&path);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lg-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn past_sessions_come_from_every_folders_transcripts() {
        let projects = temp_dir("projects");
        write(
            &projects.join("-w-shop/s-old.jsonl"),
            "{\"type\":\"mode\"}\n{\"type\":\"user\",\"cwd\":\"/w/shop\",\"message\":{\"content\":\"fix the checkout\\nplease\"}}\n",
        );
        write(
            &projects.join("-w-blog/s-new.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/w/blog\",\"message\":{\"content\":\"hi\"}}\n{\"type\":\"ai-title\",\"aiTitle\":\"Blog redesign\"}\n",
        );
        write(&projects.join("-w-x/no-cwd.jsonl"), "{\"type\":\"mode\"}\n");
        let old = std::fs::File::options().write(true).open(projects.join("-w-shop/s-old.jsonl")).unwrap();
        old.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();
        let found = past_sessions(&projects, 10);
        let _ = std::fs::remove_dir_all(&projects);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].session_id, "s-new");
        assert_eq!(found[0].cwd, "/w/blog");
        assert_eq!(found[0].title.as_deref(), Some("Blog redesign"));
        assert_eq!(found[1].title.as_deref(), Some("fix the checkout"));
    }

    /// A home with one plugin (on) that has a skill, a command and an MCP server.
    fn home_with_plugin(home: &Path) {
        let plugin = home.join(".claude/plugins/cache/mk/tools/1.0");
        write(&plugin.join(".claude-plugin/plugin.json"), r#"{"name":"tools","description":"Handy"}"#);
        write(&plugin.join("skills/lint/SKILL.md"), "---\nname: lint\ndescription: Lints things\n---\nbody");
        write(&plugin.join("commands/deploy.md"), "---\ndescription: Ship it\n---\nDeploy $ARGUMENTS");
        write(&plugin.join(".mcp.json"), r#"{"mcpServers":{"db":{"command":"/usr/bin/pg-mcp --secret x"}}}"#);
        let installed = json!({ "version": 2, "plugins": { "tools@mk": [{ "scope": "user", "installPath": plugin.display().to_string(), "version": "1.0" }] } });
        write(&home.join(".claude/plugins/installed_plugins.json"), &installed.to_string());
        write(&home.join(".claude/settings.json"), r#"{"enabledPlugins":{"tools@mk":true}}"#);
    }

    #[test]
    fn skills_commands_and_mcp_servers_include_plugins() {
        let (home, cwd) = (temp_dir("home"), temp_dir("cwd"));
        home_with_plugin(&home);
        write(&home.join(".claude/skills/notes/SKILL.md"), "---\nname: notes\ndescription: Takes notes\n---\n");
        write(&cwd.join(".claude/skills/build/SKILL.md"), "---\nname: build\ndescription: Builds\n---\n");
        write(&home.join(".claude/commands/standup.md"), "Write my standup from git log");
        write(&cwd.join(".claude/commands/frontend/component.md"), "---\ndescription: New component\n---\n");

        let skills: Vec<(String, &str)> = skills_in(&cwd, &home).iter().map(|s| (s.name.clone(), s.source)).collect();
        assert_eq!(skills, vec![("build".into(), "project"), ("notes".into(), "personal"), ("tools:lint".into(), "plugin")]);

        let commands: Vec<(String, Option<String>, &str)> = commands_in(&cwd, &home).into_iter().map(|c| (c.name, c.description, c.source)).collect();
        assert_eq!(
            commands,
            vec![
                ("frontend:component".into(), Some("New component".into()), "project"),
                ("standup".into(), Some("Write my standup from git log".into()), "personal"),
                ("tools:deploy".into(), Some("Ship it".into()), "plugin"),
            ]
        );

        let servers = mcp_servers(&cwd, &home);
        assert_eq!(servers, vec![json!({ "name": "tools:db", "transport": "stdio", "source": "plugin", "command": "pg-mcp" })]);

        // Turned off: its skills, commands and servers go.
        write(&home.join(".claude/settings.json"), r#"{"enabledPlugins":{"tools@mk":false}}"#);
        assert!(plugin_folders(&cwd, &home).is_empty());
        let _ = (std::fs::remove_dir_all(&home), std::fs::remove_dir_all(&cwd));
    }

    #[test]
    fn attachments_are_saved_and_listed_after_the_text() {
        let dir = temp_dir("attach");
        let png = json!({ "type": "image", "mimeType": "image/png", "fileName": "../shot.png", "content": "data:image/png;base64,iVBORw0KGgo=" });
        let notes = json!({ "type": "file", "mimeType": "text/plain", "fileName": "notes.txt", "content": "aGk=" });
        let text = with_attachments(&dir, "look at these", &[png, notes]).unwrap();
        let (said, list) = text.split_once("\n\nAttached files:\n").expect("the list follows the text");
        assert_eq!(said, "look at these");
        let files: Vec<(&str, &str)> = list.lines().map(|l| l.strip_prefix("- ").unwrap().split_once(": ").unwrap()).collect();
        assert_eq!(files.iter().map(|(name, _)| *name).collect::<Vec<_>>(), vec!["shot.png", "notes.txt"]);
        assert!(files.iter().all(|(_, path)| Path::new(path).starts_with(attachments_dir(&dir))));
        assert_eq!(std::fs::read(files[1].1).unwrap(), b"hi");
        assert_eq!(with_attachments(&dir, "plain", &[]).unwrap(), "plain");
        assert!(with_attachments(&dir, "", &[json!({ "fileName": "x", "content": "%%%" })]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn files_are_the_selected_threads_folder() {
        let (shop, blog) = (temp_dir("shop"), temp_dir("blog"));
        write(&shop.join("cart.rs"), "fn main() {}");
        write(&blog.join("post.md"), "# hi");
        let dir = temp_dir("files");
        let hooks = Hooks { status: Box::new(|_| {}), event: Box::new(|_, _, _| {}), connected: Box::new(|| {}) };
        let client = ClaudeCodeClient::new("f".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks);
        {
            let mut st = client.inner.st.lock().unwrap();
            st.connected = true;
            let s = st.session("s-shop");
            s.cwd = shop.display().to_string();
            s.updated_at = 1;
            // The latest folder, for calls without a thread.
            let s = st.session("s-blog");
            s.cwd = blog.display().to_string();
            s.updated_at = 2;
        }
        let names = |v: &Value| v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let list = |key: Option<String>| {
            let mut params = json!({ "agentId": AGENT_ID });
            if let Some(key) = key {
                params["sessionKey"] = json!(key);
            }
            client.call("agents.workspace.list", params)
        };
        let thread = list(Some(keys::thread(AGENT_ID, "s-shop"))).await.unwrap();
        assert_eq!(names(&thread), vec!["cart.rs"]);
        assert_eq!(thread["workspaceDir"], shop.display().to_string());
        let latest = list(None).await.unwrap();
        assert_eq!(names(&latest), vec!["post.md"]);
        assert_eq!(latest["workspaceDir"], blog.display().to_string());
        let file = client
            .call("agents.workspace.get", json!({ "path": "cart.rs", "sessionKey": keys::thread(AGENT_ID, "s-shop") }))
            .await
            .unwrap();
        assert_eq!(file["file"]["content"], "fn main() {}");
        assert!(file.get("workspaceDir").is_none());
        for d in [&shop, &blog, &dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn opened_sessions_stay_listed_across_restarts() {
        let dir = temp_dir("open");
        let hooks = || Hooks { status: Box::new(|_| {}), event: Box::new(|_, _, _| {}), connected: Box::new(|| {}) };
        let client = ClaudeCodeClient::new("t".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks());
        let out = sessions_open(&client.inner, &json!({ "sessionId": "s-old", "cwd": "/w/shop", "updatedAt": 5 })).unwrap();
        assert_eq!(out["key"], keys::thread(AGENT_ID, "s-old"));
        let again = ClaudeCodeClient::new("t".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks());
        let st = again.inner.st.lock().unwrap();
        let s = st.sessions.get("s-old").expect("seeded from what littleguys remembers");
        assert!(s.kept && s.listable(now_ms()));
        assert_eq!(s.cwd, "/w/shop");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Follows sessions the way a connection does (hooks report to the app, not here).
    async fn follow(inner: &Arc<Inner>) {
        poll_agents(inner).await;
        follow_transcripts(inner).await;
    }

    async fn remove_session(claude: &Path, sid: &str) {
        let _ = run_cli(claude, &["rm", &sid[..8]], CLI_TIMEOUT).await;
    }

    /// Reads this Mac's real Claude Code setup (nothing is started or changed):
    /// `LG_E2E_CWD=<folder> cargo test --lib real_extras -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "reads this Mac's Claude Code setup"]
    async fn real_extras_read_this_macs_setup() {
        let cwd = std::env::var("LG_E2E_CWD").expect("LG_E2E_CWD: a folder you work in");
        let dir = temp_dir("extras");
        let hooks = Hooks { status: Box::new(|_| {}), event: Box::new(|_, _, _| {}), connected: Box::new(|| {}) };
        let client = ClaudeCodeClient::new("x".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks);
        {
            let mut st = client.inner.st.lock().unwrap();
            st.connected = true;
            st.session("here").cwd = cwd.clone();
            st.session("here").updated_at = now_ms();
        }
        let names = |v: &Value, list: &str| -> Vec<String> {
            v[list].as_array().unwrap().iter().map(|r| format!("{}{}", r["name"].as_str().unwrap_or_default(), r["source"].as_str().map(|s| format!(" ({s})")).unwrap_or_default())).collect()
        };
        let skills = client.call("skills.status", json!({ "agentId": AGENT_ID })).await.unwrap();
        println!("skills: {:?}", names(&skills, "skills"));
        let commands = client.call("commands.list", json!({ "agentId": AGENT_ID })).await.unwrap();
        println!("commands: {:?}", names(&commands, "commands"));
        let servers = client.call("mcp.servers.list", json!({ "agentId": AGENT_ID })).await.unwrap();
        println!("mcp: {:?}", names(&servers, "servers"));
        let plugins = client.call("plugins.list", json!({})).await.unwrap();
        println!("plugins: {:?}", names(&plugins, "plugins"));
        let files = client.call("agents.workspace.list", json!({ "agentId": AGENT_ID })).await.unwrap();
        println!("files in {cwd}: {}", files.to_string().chars().take(300).collect::<String>());
        let started = Instant::now();
        let past = client.call("sessions.past", json!({})).await.unwrap();
        let rows = past["sessions"].as_array().unwrap();
        println!("past: {} sessions in {:?}; newest: {:?}", rows.len(), started.elapsed(), rows.iter().take(3).map(|r| (r["title"].clone(), r["cwd"].clone())).collect::<Vec<_>>());
        assert!(!rows.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Starts two real (tiny) background sessions, a new thread's and a routine run's:
    /// `LG_E2E_CWD=<a folder Claude Code trusts> cargo test --lib real_sessions -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts real Claude Code sessions"]
    async fn real_sessions_start_from_new_threads_and_routines() {
        let cwd = std::env::var("LG_E2E_CWD").expect("LG_E2E_CWD: a folder Claude Code trusts");
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let seen = events.clone();
        let hooks = Hooks {
            status: Box::new(|_| {}),
            event: Box::new(move |name, payload, _| seen.lock().unwrap().push((name.to_string(), payload.clone()))),
            connected: Box::new(|| {}),
        };
        let dir = std::env::temp_dir().join(format!("lg-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let client = ClaudeCodeClient::new("e2e".into(), "Claude".into(), ClaudeCodeConfig::default(), dir.clone(), hooks);
        let inner = client.inner.clone();
        let claude = find_claude().expect("Claude Code installed");
        *inner.claude.lock().unwrap() = Some(claude.clone());
        inner.st.lock().unwrap().connected = true;

        // A new thread: a draft, then a session once you write to it.
        let created = client.call("sessions.create", json!({ "agentId": AGENT_ID })).await.unwrap();
        let key = created["key"].as_str().unwrap().to_string();
        let listed = client.call("sessions.list", json!({})).await.unwrap();
        assert!(listed["sessions"].as_array().unwrap().iter().any(|r| r["key"] == key.as_str()), "the draft is listed");
        let empty = client.call("chat.history", json!({ "sessionKey": key })).await.unwrap();
        assert_eq!(empty["messages"], json!([]));
        client
            .call("chat.send", json!({ "sessionKey": key, "message": "Reply with just the word pong. Don't use any tools.", "cwd": cwd }))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(180);
        let sid = loop {
            assert!(Instant::now() < deadline, "the new thread's turn never ended: {:?}", events.lock().unwrap());
            follow(&inner).await;
            let done = inner.st.lock().unwrap().sessions.values().find(|s| s.key() == key && s.turn > 0 && !s.busy).map(|s| s.id.clone());
            if let Some(sid) = done {
                break sid;
            }
            tokio::time::sleep(TICK).await;
        };
        println!("new thread {key} → session {sid}");
        let listed = client.call("sessions.list", json!({})).await.unwrap();
        let row = listed["sessions"].as_array().unwrap().iter().find(|r| r["key"] == key.as_str()).cloned().expect("listed under its key");
        assert_eq!(row["sessionId"], sid.as_str());
        assert_eq!(row["workspaceDir"], cwd.as_str());
        let history = client.call("chat.history", json!({ "sessionKey": key })).await.unwrap();
        let messages = history["messages"].as_array().unwrap();
        assert!(messages.iter().any(|m| m["role"] == "user"), "{messages:?}");
        assert!(messages.iter().any(|m| m["role"] == "assistant"), "{messages:?}");
        assert!(events.lock().unwrap().iter().any(|(n, p)| n == "session.message" && p["sessionKey"] == key.as_str()), "followed live");
        assert_eq!(load_started(&inner.started_path)[&sid].key, key, "remembered across restarts");

        // An attached image: saved, listed after the text, readable by the resumed session.
        let (before, said_before) = inner.st.lock().unwrap().sessions.get(&sid).map(|s| (s.handed, s.said.clone())).unwrap();
        let red = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAFWEXHbQSACj/P8Fu7N9hAAAAAElFTkSuQmCC";
        client
            .call(
                "chat.send",
                json!({
                    "sessionKey": key,
                    "message": "What single color fills the attached image? Answer with just that word.",
                    "attachments": [{ "type": "image", "mimeType": "image/png", "fileName": "swatch.png", "content": red }],
                }),
            )
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            assert!(Instant::now() < deadline, "the attachment's turn never ended");
            follow(&inner).await;
            // Without the plugin's hooks here, a new reply is the sure sign its turn ended.
            let said = inner.st.lock().unwrap().sessions.get(&sid).filter(|s| s.handed > before && !s.busy && s.said != said_before).map(|s| s.said.clone());
            if let Some(said) = said {
                println!("about the attached image: {said:?}");
                assert!(said.is_some_and(|s| s.to_lowercase().contains("red")));
                break;
            }
            tokio::time::sleep(TICK).await;
        }

        // Stop: the background session ends; writing to it again would resume it.
        client.call("chat.send", json!({ "sessionKey": key, "message": "Run `sleep 45` with the Bash tool, then say done." })).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !inner.st.lock().unwrap().sessions.get(&sid).is_some_and(|s| s.busy && s.running && s.short_id.is_some() && !s.delivering) {
            assert!(Instant::now() < deadline, "it never got going");
            follow(&inner).await;
            tokio::time::sleep(TICK).await;
        }
        let stopped = client.call("chat.abort", json!({ "sessionKey": key })).await.unwrap();
        assert_eq!(stopped["aborted"], true);
        let listed = list_agents(&claude).await.unwrap_or_default();
        assert!(!listed.iter().any(|l| l.session_id == sid && l.pid.is_some()), "still running after Stop");
        remove_session(&claude, &sid).await;

        // A routine run: its own session, named after the routine, in the routine's folder.
        let job = client
            .call(
                "cron.add",
                json!({
                    "name": "e2e routine", "agentId": AGENT_ID,
                    "schedule": { "kind": "at", "at": "2099-01-01T00:00:00Z" },
                    "payload": { "kind": "agentTurn", "message": "Reply with just the word ping. Don't use any tools.", "cwd": cwd },
                }),
            )
            .await
            .unwrap();
        let following = {
            let inner = inner.clone();
            tokio::spawn(async move {
                loop {
                    follow(&inner).await;
                    tokio::time::sleep(TICK).await;
                }
            })
        };
        let runner = Runner(Arc::downgrade(&inner));
        let message = job["payload"]["message"].as_str().unwrap().to_string();
        let outcome = tokio::time::timeout(
            Duration::from_secs(180),
            runner.run_turn(AGENT_ID.into(), "isolated".into(), job["id"].as_str().unwrap().into(), message),
        )
        .await
        .expect("the routine's turn never ended")
        .unwrap();
        following.abort();
        println!("routine run {} said {:?}", outcome.session_key, outcome.summary);
        assert!(outcome.session_key.starts_with(&format!("agent:{AGENT_ID}:cron:")));
        assert!(outcome.summary.as_deref().is_some_and(|s| s.to_lowercase().contains("ping")), "{outcome:?}");
        let run = inner.st.lock().unwrap().sessions.values().find(|s| s.key() == outcome.session_key).map(|s| (s.id.clone(), s.name.clone(), s.cwd.clone()));
        let (run_sid, name, run_cwd) = run.expect("the run's session");
        assert_eq!(name.as_deref(), Some("e2e routine"));
        assert_eq!(run_cwd, cwd);
        remove_session(&claude, &run_sid).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
