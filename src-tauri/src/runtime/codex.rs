//! Codex: every Codex session on this machine, in a terminal, on the shared app server
//! or from `codex exec`, is a thread on one little guy (agent id `codex`).
//!
//! - Sessions and all they say come from the rollout files every Codex host writes live
//!   (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`, compressed to `.jsonl.zst` once
//!   they're old): read whole for `chat.history`, and followed while the session is live,
//!   every new line a `session.message`. A session is live while its writer lock is held
//!   (`thread-writer-locks/<id>.lock`). Names come from `session_index.jsonl`. Reading needs
//!   neither the shared server nor the app-server protocol, which is marked experimental.
//! - The shared app server, when it's up, adds what files can't: it says which sessions
//!   wait for you, and takes replies, approvals and interrupts. littleguys reaches it
//!   through `codex app-server proxy` (WebSocket over its stdio, the same on every OS), with
//!   the experimental API off. It never starts, stops or updates the server.
//!   Plain `codex` runs on that server when it's up; `-c`, `--enable`, `--disable`,
//!   `--search`, `--no-daemon` and `codex exec` run in their own process, out of its reach.
//! - `chat.send`: see [`route`]. A turn on the shared server, else `codex queue --resume`
//!   (a live session in a terminal picks it up within seconds, a busy one after its turn),
//!   else, with no server and a session that ended, `codex exec resume`.
//! - Approvals: a session waiting on one gets joined (`thread/resume`), which replays the
//!   prompt to littleguys. The terminal keeps showing it too, and the first answer wins.
//!   Sessions outside the shared server keep asking in their own terminal.
//! - New threads start as drafts; the first message starts a session on the shared server,
//!   or with `codex exec` when it's down. The thread keeps the key littleguys gave it,
//!   saved in `codex-threads-<id>.json`, along with renames, pins and so on.
//! - Routines run through a `LocalScheduler`, each run a new session started the same way
//!   (or a message to the thread it targets).

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::{now_ms, write_private};
use crate::openclaw::config::{Credential, GatewayMode};
use crate::openclaw::runtime as cli;
use crate::runtime::claudecode::{first_line, modified_ms, read_lines};
use crate::runtime::scheduler::{LocalScheduler, RoutineRunner, TurnOutcome};
use crate::runtime::{blocking, home, keys, parse_iso_ms, truncate, unsupported, Capabilities, RuntimeKind, ThreadFlags};

const AGENT_ID: &str = "codex";

const TICK: Duration = Duration::from_secs(1);
/// Walk the sessions folder every this many ticks (sooner when a new lock shows up).
const SCAN_EVERY: u32 = 5;
/// Sessions show up as threads while they're live, or for this long after.
const LIST_WINDOW_MS: u64 = 3 * 24 * 3600 * 1000;
/// Rollouts are followed while their session is live, or for this long after it last wrote.
const FOLLOW_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Codex versions without writer locks: a session counts as live this long after it wrote.
const UNLOCKED_LIVE_MS: u64 = 2 * 60 * 1000;
const HISTORY_MAX_BYTES: u64 = 24 << 20;
const TAIL_MAX_BYTES: u64 = 8 << 20;
/// How much of a rollout's end is read for its last words and whether a turn is open.
const SUMMARY_BYTES: u64 = 256 << 10;
/// A rollout starts with Codex's instructions, so its first prompt can sit this deep.
const HEAD_BYTES: u64 = 256 << 10;
/// …and its first line, which says what it is, within this.
const PAST_HEAD_BYTES: u64 = 64 << 10;
const TOOL_TEXT_MAX: usize = 20_000;
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_TIMEOUT: Duration = Duration::from_secs(20);
/// `codex queue --resume` can wait on the shared server loading the session.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_RETRY: Duration = Duration::from_secs(20);
/// A message was handed over; its turn has this long to show up in the rollout.
const TURN_SHOWS_UP: Duration = Duration::from_secs(30);
/// Sessions joined for a turn or an approval are let go this long after they're quiet.
const JOIN_GRACE: Duration = Duration::from_secs(30);
/// A routine's turn gets this long before littleguys stops waiting for it.
const ROUTINE_MAX: Duration = Duration::from_secs(4 * 3600);
/// Threads littleguys keeps are remembered this long.
const STARTED_KEEP_MS: u64 = 30 * 24 * 3600 * 1000;
/// …and at most this many that aren't yours (routine runs).
const STARTED_MAX: usize = 2_000;
const PAST_MAX: usize = 400;

/// Notifications littleguys has no use for: it reads whole lines from the rollout.
const QUIET: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
    "rawResponseItem/completed",
    "process/outputDelta",
    "command/exec/outputDelta",
];

type Emits = Vec<(&'static str, Value)>;

fn capabilities() -> Capabilities {
    Capabilities {
        manage_guys: false,
        routines: true,
        routines_local: true,
        approvals: true,
        thread_actions: true,
        new_threads: true,
        attachments: false,
        models: false,
        files: true,
        skills: false,
        commands: false,
        tools: false,
        plugins: false,
        browser: false,
        past_sessions: true,
    }
}

#[derive(Clone)]
pub struct CodexClient {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    /// `$CODEX_HOME`, else `~/.codex`.
    home: PathBuf,
    hooks: Hooks,
    status: Mutex<ConnStatus>,
    started: AtomicBool,
    stopped: AtomicBool,
    /// Leave the current connection and set up again.
    restart: AtomicBool,
    wake: Notify,
    /// Try the shared server now instead of at the next retry.
    server_retry: Notify,
    codex: Mutex<Option<PathBuf>>,
    /// The installed Codex has `codex queue`.
    can_queue: AtomicBool,
    /// The shared server, while littleguys is connected to it.
    server: Mutex<Option<Arc<Server>>>,
    st: Mutex<State>,
    scheduler: Arc<LocalScheduler>,
    /// `<data_dir>/codex-threads-<id>.json`: [`State::started`].
    started_path: PathBuf,
    /// `LITTLEGUYS_CODEX_ONLY` (development): only follow sessions in this folder.
    only: Option<String>,
    /// `LITTLEGUYS_CODEX_MODEL` (development): the model new sessions use.
    model: Option<String>,
    /// `LITTLEGUYS_CODEX_SERVER=off` (development): act as if the shared server were down.
    no_server: bool,
}

#[derive(Default)]
struct State {
    connected: bool,
    sessions: HashMap<String, Session>,
    approvals: HashMap<String, Approval>,
    /// New threads whose session hasn't started yet, by key.
    drafts: HashMap<String, Draft>,
    /// Threads littleguys keeps, by session id: the ones it started or you opened or wrote to
    /// (under the key it gave them), and the ones you've renamed, pinned, read or deleted.
    started: HashMap<String, Started>,
    /// What `session_index.jsonl` calls sessions, and how far it's been read.
    names: HashMap<String, String>,
    names_read: u64,
    /// What the shared server says sessions wait for.
    waiting: HashMap<String, WaitingOn>,
    /// Sessions littleguys joined on the shared server, and when they were last needed.
    joined: HashMap<String, Instant>,
}

impl State {
    /// The session `sid`, added if it's new (with what littleguys kept about it).
    fn session(&mut self, sid: &str) -> &mut Session {
        if !self.sessions.contains_key(sid) {
            let mut s = Session::new(sid);
            if let Some(t) = self.started.get(sid) {
                s.key = Some(t.key.clone());
                s.kept = t.keep;
                s.cwd = t.cwd.clone().unwrap_or_default();
                s.flags = t.flags.clone();
                s.hidden = t.hidden;
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
    fn default_folder(&self) -> String {
        let home = dirs::home_dir().unwrap_or_default();
        let mut recent: Vec<&Session> = self.sessions.values().filter(|s| !s.skip && !s.cwd.is_empty() && !s.cwd.contains("/.codex/")).collect();
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
    /// Listed however long ago it last ran, and not archived for having ended.
    #[serde(default)]
    keep: bool,
    #[serde(flatten)]
    flags: ThreadFlags,
    /// You deleted it: littleguys doesn't list it anymore.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
}

fn load_started(path: &Path) -> HashMap<String, Started> {
    let mut started: HashMap<String, Started> = std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
    let now = now_ms();
    started.retain(|_, t| t.flags.pinned || now.saturating_sub(t.at) < STARTED_KEEP_MS);
    started
}

/// Remembers `sid` as `thread` and saves; the oldest past the newest `STARTED_MAX` that
/// aren't kept are forgotten.
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
            log::warn!("[codex] couldn't save the threads littleguys keeps: {e}");
        }
    }
}

#[derive(Default)]
struct Session {
    id: String,
    /// The thread key littleguys gave it, when littleguys started it.
    key: Option<String>,
    /// A thread you started, opened or wrote to: listed however long ago it last ran, and
    /// not archived for having ended.
    kept: bool,
    /// Not shown: a subagent's session (Codex's reviewers and helpers), or one
    /// `LITTLEGUYS_CODEX_ONLY` leaves out.
    skip: bool,
    cwd: String,
    /// Its name in `session_index.jsonl`.
    name: Option<String>,
    first_prompt: Option<String>,
    /// Its last words, for the thread list.
    said: Option<String>,
    path: Option<PathBuf>,
    /// Something has it open: a terminal, the shared server, `codex exec`.
    live: bool,
    /// What you set on its thread. Your Archive or Unarchive stands over whether it's
    /// live until its next turn.
    flags: ThreadFlags,
    /// `flags`, `kept` or `hidden` changed since they were saved.
    unsaved: bool,
    /// You deleted its thread.
    hidden: bool,
    busy: bool,
    /// Busy because a message was just handed over, and since when; its turn hasn't shown up yet.
    assumed: Option<Instant>,
    /// Its rollout's last turn hasn't ended.
    open_turn: bool,
    turn: u64,
    /// The turn under way, as `turn/interrupt` wants it.
    turn_id: Option<String>,
    /// Its last turn was interrupted: Codex holds queued messages back until the next prompt.
    interrupted: bool,
    started_at: u64,
    updated_at: u64,
    tail: Option<Tail>,
    /// call id → tool name, for naming tool results.
    calls: HashMap<String, String>,
    /// Your messages waiting for the session.
    outbox: Vec<String>,
    delivering: bool,
    /// How many times the outbox was handed over (a routine waits for its message's turn).
    handed: u64,
    /// Stops the `codex exec` running it.
    exec: Option<oneshot::Sender<()>>,
}

impl Session {
    fn new(id: &str) -> Self {
        Self { id: id.to_string(), ..Default::default() }
    }

    fn key(&self) -> String {
        self.key.clone().unwrap_or_else(|| keys::thread(AGENT_ID, &self.id))
    }

    fn run_id(&self) -> String {
        format!("codex-{}-{}", self.id, self.turn)
    }

    fn listable(&self, now: u64) -> bool {
        !self.hidden
            && !self.skip
            && (self.live
                || self.busy
                || self.kept
                || self.flags.pinned
                || now.saturating_sub(self.updated_at.max(self.started_at)) < LIST_WINDOW_MS)
    }

    /// There's something to show: it was read, and someone said something. (Opening `codex`
    /// and leaving writes a rollout too.)
    fn shown(&self) -> bool {
        self.tail.is_some() && (self.busy || (self.path.is_some() && (self.kept || self.first_prompt.is_some() || self.said.is_some())))
    }

    /// In the archive rather than the thread list: once it ended, unless you've kept it or
    /// said otherwise. Never while it works, and never a routine's run.
    fn archived(&self) -> bool {
        let routine_run = self.key.as_deref().and_then(keys::split).is_some_and(|(_, rest)| rest.starts_with("cron:"));
        !self.busy && !routine_run && self.flags.archived.unwrap_or(!self.live && !self.kept)
    }

    fn followed(&self, now: u64) -> bool {
        self.live || self.busy || now.saturating_sub(self.updated_at) < FOLLOW_WINDOW_MS
    }

    fn title(&self) -> Option<String> {
        self.name
            .clone()
            .or_else(|| self.first_prompt.as_deref().map(|p| truncate(first_line(p), 80)))
            .or_else(|| Path::new(&self.cwd).file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    fn preview(&self, waiting: Option<&WaitingOn>) -> Option<String> {
        match (waiting, &self.said) {
            (Some(_), _) => Some("Waiting for you in Codex".to_string()),
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
        let key = self.key();
        if busy {
            self.turn += 1;
            // At work again: whether it's archived is back to whether it's live.
            self.unsaved |= self.flags.archived.take().is_some();
            emits.push(("sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "phase": "start" })));
            emits.push(("chat", json!({ "sessionKey": key, "runId": self.run_id(), "state": "status", "phase": "thinking" })));
        } else {
            self.assumed = None;
            // A reply you haven't read (in littleguys).
            if !self.flags.unread {
                self.flags.unread = true;
                self.unsaved = true;
            }
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

/// A prompt on the shared server, waiting for an answer here or in the terminal.
struct Approval {
    session_id: String,
    /// The server's id for the request.
    request: Value,
    payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WaitingOn {
    Approval,
    Input,
}

impl CodexClient {
    pub fn new(id: String, name: String, data_dir: PathBuf, hooks: Hooks) -> Self {
        let status = ConnStatus {
            gateway_id: id.clone(),
            gateway_name: name,
            kind: RuntimeKind::Codex,
            capabilities: capabilities(),
            mode: Some(GatewayMode::Local),
            url: Some(format!("This {}", crate::platform::COMPUTER)),
            ..Default::default()
        };
        let scheduler = LocalScheduler::new(&data_dir, &id);
        let started_path = data_dir.join(format!("codex-threads-{id}.json"));
        let mut st = State { started: load_started(&started_path), ..Default::default() };
        // Threads littleguys keeps show up again, however old.
        for sid in st.started.keys().cloned().collect::<Vec<_>>() {
            st.session(&sid);
        }
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            inner: Arc::new(Inner {
                id,
                home: codex_home(),
                hooks,
                status: Mutex::new(status),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                restart: AtomicBool::new(false),
                wake: Notify::new(),
                server_retry: Notify::new(),
                codex: Mutex::new(None),
                can_queue: AtomicBool::new(false),
                server: Mutex::new(None),
                st: Mutex::new(st),
                scheduler,
                started_path,
                only: env("LITTLEGUYS_CODEX_ONLY"),
                model: env("LITTLEGUYS_CODEX_MODEL"),
                no_server: env("LITTLEGUYS_CODEX_SERVER").is_some_and(|v| v == "off"),
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
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move { follow_server(inner).await });
    }

    /// Stop for good (bot removed or turned off).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.scheduler.stop();
        self.inner.wake.notify_one();
        self.inner.server_retry.notify_one();
    }

    pub fn reconnect_now(&self) {
        self.inner.restart.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
        self.inner.server_retry.notify_one();
    }

    pub fn set_name(&self, name: String) {
        set_status(&self.inner, |s| s.gateway_name = name);
    }

    pub fn set_credential_override(&self, _credential: Option<Credential>) {}

    /// The folder a thread works in, else the one you last worked in.
    pub fn thread_folder(&self, session_key: Option<&str>) -> PathBuf {
        folder_of(&self.inner, &json!({ "sessionKey": session_key }))
    }

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
            return Err(RpcError::new("NOT_CONNECTED", "not following Codex yet"));
        }
        match method {
            "agents.list" => Ok(agents_list(inner)),
            "sessions.list" => Ok(sessions_list(inner, &params)),
            "sessions.create" => sessions_create(inner, &params),
            "sessions.patch" => sessions_patch(inner, &params),
            "sessions.delete" => sessions_delete(inner, &params),
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
    ("sessions.changed", json!({ "reason": "codex" }))
}

async fn wait_or_wake(inner: &Inner, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = inner.wake.notified() => true,
    }
}

// ---- supervisor -----------------------------------------------------------------------

async fn run(inner: Arc<Inner>) {
    let mut attempt: u32 = 0;
    while !inner.stopped.load(Ordering::SeqCst) {
        inner.restart.store(false, Ordering::SeqCst);
        match prepare(&inner).await {
            Ok(()) => {
                attempt = 0;
                inner.server_retry.notify_one();
                serve(&inner).await;
            }
            Err(message) => set_status(&inner, |s| {
                s.phase = Phase::NoConfig;
                s.message = Some(message);
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

async fn prepare(inner: &Arc<Inner>) -> Result<(), String> {
    set_status(inner, |s| {
        if s.phase != Phase::NoConfig {
            s.phase = Phase::Connecting;
        }
    });
    let codex = find_codex().ok_or_else(|| format!("Codex isn't installed on this {}. Install it from developers.openai.com/codex, then try again.", crate::platform::COMPUTER))?;
    *inner.codex.lock().unwrap() = Some(codex.clone());
    let version = codex_version(&codex).await;
    let can_queue = run_cli(&codex, &["queue", "--help"], CLI_TIMEOUT).await.is_ok();
    inner.can_queue.store(can_queue, Ordering::SeqCst);
    set_status(inner, |s| s.server_version = version);
    Ok(())
}

async fn serve(inner: &Arc<Inner>) {
    inner.st.lock().unwrap().connected = true;
    set_status(inner, |s| {
        s.phase = Phase::Connected;
        s.message = None;
        s.detail_code = None;
        s.capabilities = capabilities();
    });
    (inner.hooks.connected)();
    let mut tick: u32 = 0;
    loop {
        scan(inner, tick).await;
        follow_rollouts(inner).await;
        settle_turns(inner);
        join_waiting(inner);
        release_joined(inner);
        save_flags(inner);
        wait_or_wake(inner, TICK).await;
        if inner.stopped.load(Ordering::SeqCst) || inner.restart.load(Ordering::SeqCst) {
            break;
        }
        tick = tick.wrapping_add(1);
    }
    inner.st.lock().unwrap().connected = false;
    set_status(inner, |s| s.phase = Phase::Connecting);
}

// ---- finding sessions -----------------------------------------------------------------

/// One rollout file.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    id: String,
    path: PathBuf,
    modified: u64,
}

/// `rollout-<time>-<uuid>.jsonl` (or `.jsonl.zst`) → its session id.
fn rollout_id(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_prefix("rollout-")?;
    let stem = stem.strip_suffix(".jsonl.zst").or_else(|| stem.strip_suffix(".jsonl"))?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid::Uuid::parse_str(id).ok().map(|_| id)
}

fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
}

/// Every rollout under `sessions/YYYY/MM/DD`. (By file rather than by recent days: a
/// session resumed today still writes to the day it started.)
fn walk(sessions: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    for year in subfolders(sessions) {
        for month in subfolders(&year) {
            for day in subfolders(&month) {
                for file in std::fs::read_dir(&day).into_iter().flatten().flatten() {
                    let name = file.file_name();
                    let Some(id) = name.to_str().and_then(rollout_id) else { continue };
                    let modified = file.metadata().map(|m| modified_ms(&m)).unwrap_or(0);
                    found.push(Found { id: id.to_string(), path: file.path(), modified });
                }
            }
        }
    }
    found
}

/// Whether something holds this lock. The owner keeps it exclusively for as long as it has
/// the session open and removes the file on a clean exit, so a file nobody holds is left
/// from a crash.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    matches!(file.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
}

/// The sessions something has open, from `thread-writer-locks/`. None when this Codex
/// doesn't keep that folder.
fn live_ids(home: &Path) -> Option<HashSet<String>> {
    let entries = std::fs::read_dir(home.join("thread-writer-locks")).ok()?;
    let mut live = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "lock") {
            continue;
        }
        if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
            if lock_held(&path) {
                live.insert(id.to_string());
            }
        }
    }
    Some(live)
}

/// New lines of `session_index.jsonl`, as (id, name), and how far that got.
fn read_names(path: &Path, from: u64) -> (Vec<(String, String)>, u64) {
    let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else { return (Vec::new(), 0) };
    let from = if len < from { 0 } else { from };
    if len == from {
        return (Vec::new(), from);
    }
    let Ok((lines, end, rest)) = read_lines(path, from, len, Vec::new(), false) else { return (Vec::new(), from) };
    let names = lines
        .iter()
        .filter_map(|l| {
            let id = l.get("id")?.as_str()?;
            let name = l.get("thread_name")?.as_str()?.trim();
            (!name.is_empty()).then(|| (id.to_string(), name.to_string()))
        })
        .collect();
    (names, end - rest.len() as u64)
}

/// What was found folded in: rollouts, who's live, and names. Returns what to emit.
fn apply_scan(st: &mut State, found: Option<Vec<Found>>, live: Option<&HashSet<String>>, now: u64) -> Emits {
    let mut emits = Vec::new();
    // A rollout that's gone (`codex archive`, `codex delete`) takes its thread with it.
    let files: Option<HashSet<String>> = found.as_ref().map(|found| found.iter().map(|f| f.id.clone()).collect());
    if let Some(files) = &files {
        for s in st.sessions.values_mut().filter(|s| !files.contains(&s.id) && !s.busy) {
            s.path = None;
        }
    }
    for f in found.into_iter().flatten() {
        let is_live = live.is_some_and(|l| l.contains(&f.id));
        let recent = now.saturating_sub(f.modified) < LIST_WINDOW_MS;
        if !st.sessions.contains_key(&f.id) && !recent && !is_live {
            continue;
        }
        let s = st.session(&f.id);
        s.path = Some(f.path);
        s.updated_at = s.updated_at.max(f.modified);
    }
    let mut changed = false;
    for s in st.sessions.values_mut() {
        let up = match live {
            Some(live) => live.contains(&s.id),
            None => s.path.is_some() && now.saturating_sub(s.updated_at) < UNLOCKED_LIVE_MS,
        };
        changed |= s.live != up;
        s.live = up;
    }
    if changed {
        emits.push(refresh());
    }
    emits
}

async fn scan(inner: &Arc<Inner>, tick: u32) {
    let home = inner.home.clone();
    let Ok(live) = tauri::async_runtime::spawn_blocking(move || live_ids(&home)).await else { return };
    let new_lock = live.as_ref().is_some_and(|live| {
        let st = inner.st.lock().unwrap();
        live.iter().any(|id| !st.sessions.get(id).is_some_and(|s| s.path.is_some()))
    });
    let full = new_lock || tick.is_multiple_of(SCAN_EVERY);
    let found = if full {
        let home = inner.home.clone();
        let names_read = inner.st.lock().unwrap().names_read;
        tauri::async_runtime::spawn_blocking(move || (walk(&home.join("sessions")), read_names(&home.join("session_index.jsonl"), names_read)))
            .await
            .ok()
    } else {
        None
    };
    let emits = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let found = found.map(|(found, (names, read))| {
            st.names.extend(names);
            st.names_read = read;
            found
        });
        let mut emits = apply_scan(st, found, live.as_ref(), now_ms());
        let mut renamed = false;
        for s in st.sessions.values_mut() {
            if let Some(name) = st.names.get(&s.id).filter(|n| s.name.as_ref() != Some(n)) {
                s.name = Some(name.clone());
                renamed = true;
            }
        }
        if renamed {
            emits.push(refresh());
        }
        emits
    };
    flush(inner, emits);
}

// ---- reading rollouts -----------------------------------------------------------------

/// Where a rollout was read up to, and what was new.
enum Read {
    /// First look: its start, for what it is and its first prompt, and its end, for its
    /// last words.
    First { head: Vec<Value>, tail: Tail, lines: Vec<Value>, modified: u64 },
    New { tail: Tail, lines: Vec<Value>, modified: u64 },
    Same,
    Gone,
}

fn parse_lines(bytes: &[u8]) -> Vec<Value> {
    bytes.split(|b| *b == b'\n').filter(|raw| raw.len() > 1).filter_map(|raw| serde_json::from_slice(raw).ok()).collect()
}

/// A compressed rollout's first `max` bytes, unpacked.
fn read_zst(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(file).map_err(std::io::Error::other)?;
    let mut bytes = Vec::new();
    decoder.take(max).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The JSON lines in a rollout's first `max` bytes.
fn read_head(path: &Path, max: u64) -> std::io::Result<Vec<Value>> {
    if is_compressed(path) {
        let bytes = read_zst(path, max)?;
        return Ok(parse_lines(&bytes));
    }
    let len = std::fs::metadata(path)?.len();
    Ok(read_lines(path, 0, len.min(max), Vec::new(), false)?.0)
}

fn read_rollout(path: &Path, tail: Option<Tail>) -> Read {
    let Ok(meta) = std::fs::metadata(path) else { return Read::Gone };
    let len = meta.len();
    let modified = modified_ms(&meta);
    let first = |len: u64| {
        if is_compressed(path) {
            // Old and done with: it's read once, whole.
            return match read_zst(path, HISTORY_MAX_BYTES) {
                Ok(bytes) => {
                    let lines = parse_lines(&bytes);
                    Read::First { head: lines.clone(), tail: Tail { offset: len, partial: Vec::new() }, lines, modified }
                }
                Err(_) => Read::Gone,
            };
        }
        let Ok(head) = read_head(path, HEAD_BYTES) else { return Read::Gone };
        let from = len.saturating_sub(SUMMARY_BYTES);
        match read_lines(path, from, len, Vec::new(), from > 0) {
            Ok((lines, offset, partial)) => Read::First { head, tail: Tail { offset, partial }, lines, modified },
            Err(_) => Read::Gone,
        }
    };
    match tail {
        None => first(len),
        Some(t) if len == t.offset => Read::Same,
        Some(_) if is_compressed(path) => Read::Same,
        // Rewritten from scratch: start over from its end.
        Some(t) if len < t.offset => first(len),
        Some(t) => {
            let to = len.min(t.offset + TAIL_MAX_BYTES);
            match read_lines(path, t.offset, to, t.partial, false) {
                Ok((lines, offset, partial)) => Read::New { tail: Tail { offset, partial }, lines, modified },
                Err(_) => Read::Gone,
            }
        }
    }
}

/// Every message in a rollout (its last `HISTORY_MAX_BYTES`).
fn read_history(path: &Path) -> std::io::Result<Vec<Value>> {
    let lines = if is_compressed(path) {
        parse_lines(&read_zst(path, HISTORY_MAX_BYTES)?)
    } else {
        let len = std::fs::metadata(path)?.len();
        let from = len.saturating_sub(HISTORY_MAX_BYTES);
        let (mut lines, _, rest) = read_lines(path, from, len, Vec::new(), from > 0)?;
        // A last line still being written.
        lines.extend(serde_json::from_slice::<Value>(&rest));
        lines
    };
    let mut calls = HashMap::new();
    Ok(lines.iter().flat_map(|line| convert_line(line, &mut calls)).collect())
}

async fn follow_rollouts(inner: &Arc<Inner>) {
    let now = now_ms();
    let jobs: Vec<(String, PathBuf, Option<Tail>)> = {
        let st = inner.st.lock().unwrap();
        st.sessions
            .values()
            .filter(|s| (s.tail.is_none() && s.listable(now)) || (s.tail.is_some() && !s.skip && s.followed(now)))
            .filter_map(|s| s.path.clone().map(|path| (s.id.clone(), path, s.tail.clone())))
            .collect()
    };
    if jobs.is_empty() {
        return;
    }
    let reads = tauri::async_runtime::spawn_blocking(move || jobs.into_iter().map(|(sid, path, tail)| (sid, read_rollout(&path, tail))).collect::<Vec<_>>())
        .await
        .unwrap_or_default();
    let mut emits: Emits = Vec::new();
    let mut changed = false;
    {
        let mut st = inner.st.lock().unwrap();
        for (sid, read) in reads {
            let Some(s) = st.sessions.get_mut(&sid) else { continue };
            match read {
                Read::First { head, tail, lines, modified } => {
                    for line in &head {
                        note_start(s, line, inner.only.as_deref());
                    }
                    for line in &lines {
                        note_line(s, line, inner.only.as_deref(), false, &mut emits);
                        learn_calls(line, &mut s.calls);
                    }
                    s.tail = Some(tail);
                    s.updated_at = s.updated_at.max(modified);
                    if s.open_turn && s.live {
                        s.set_busy(true, &mut emits);
                    }
                    changed = true;
                }
                Read::New { tail, lines, modified } => {
                    s.tail = Some(tail);
                    s.updated_at = s.updated_at.max(modified);
                    let key = s.key();
                    for line in &lines {
                        changed |= note_line(s, line, inner.only.as_deref(), true, &mut emits);
                        if s.skip {
                            continue;
                        }
                        let run_id = s.busy.then(|| s.run_id());
                        for message in convert_line(line, &mut s.calls) {
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

/// A turn that won't end in its rollout: a message handed over whose turn never showed
/// up (it waits in the queue), or a session whatever had it open let go of mid-turn.
/// Runs after the rollouts are read, so a session that just finished gets to say so.
fn settle_turn(s: &mut Session, emits: &mut Emits) {
    if !s.busy {
        return;
    }
    match s.assumed {
        Some(at) if at.elapsed() > TURN_SHOWS_UP => {
            let unread = s.flags.unread;
            s.set_busy(false, emits);
            s.flags.unread = unread;
        }
        None if !s.live => {
            s.open_turn = false;
            s.set_busy(false, emits);
        }
        _ => {}
    }
}

fn settle_turns(inner: &Inner) {
    let mut emits: Emits = Vec::new();
    for s in inner.st.lock().unwrap().sessions.values_mut() {
        settle_turn(s, &mut emits);
    }
    if !emits.is_empty() {
        emits.push(refresh());
    }
    flush(inner, emits);
}

/// What a rollout's first line says about its session.
#[derive(Debug, PartialEq)]
struct Meta {
    cwd: String,
    started_at: Option<u64>,
    /// A helper of another session: one of its subagents, or an auto-review.
    subagent: bool,
}

fn session_meta(line: &Value) -> Option<Meta> {
    if line.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let p = line.get("payload")?;
    Some(Meta {
        cwd: p.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string(),
        started_at: p.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms),
        subagent: p.get("source").is_some_and(Value::is_object) || p.get("parent_thread_id").is_some_and(|v| !v.is_null()),
    })
}

/// A folder's path as Codex and `LITTLEGUYS_CODEX_ONLY` give it, ending at a folder name.
fn inside(cwd: &str, only: &str) -> bool {
    cwd == only || cwd.starts_with(&format!("{}/", only.trim_end_matches('/')))
}

/// Folds in what a rollout's start says: what the session is, and its first prompt.
fn note_start(s: &mut Session, line: &Value, only: Option<&str>) {
    if let Some(meta) = session_meta(line) {
        if !meta.cwd.is_empty() {
            s.cwd = meta.cwd;
        }
        s.started_at = s.started_at.max(meta.started_at.unwrap_or(0));
        s.skip = meta.subagent || only.is_some_and(|only| !inside(&s.cwd, only));
    }
    if s.first_prompt.is_none() {
        s.first_prompt = line_meta(line).prompt;
    }
}

/// What one rollout line says about its session, besides its messages.
#[derive(Debug, Default, PartialEq)]
struct LineMeta {
    prompt: Option<String>,
    said: Option<String>,
    /// A turn started; its id, when the line gives it.
    turn_started: Option<String>,
    turn_ended: bool,
    interrupted: bool,
}

fn kinds(line: &Value) -> (&str, &str) {
    let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
    let inner = line.pointer("/payload/type").and_then(Value::as_str).unwrap_or_default();
    (kind, inner)
}

/// The text of `[{type, text}…]` blocks.
fn blocks_text(content: Option<&Value>) -> String {
    let blocks = content.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect();
    texts.join("\n").trim().to_string()
}

/// What you typed: from Codex's own record of it, because `response_item` user messages also
/// carry the context it injects.
fn user_text(line: &Value) -> Option<String> {
    let text = match kinds(line) {
        ("event_msg", "item_completed") => {
            let item = line.pointer("/payload/item")?;
            if item.get("type").and_then(Value::as_str) != Some("UserMessage") {
                return None;
            }
            let text = blocks_text(item.get("content"));
            if text.is_empty() && item.get("content").and_then(Value::as_array).is_some_and(|c| !c.is_empty()) {
                "(attachment)".to_string()
            } else {
                text
            }
        }
        // Before items.
        ("event_msg", "user_message") => line.pointer("/payload/message").and_then(Value::as_str)?.trim().to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn assistant_text(line: &Value) -> Option<String> {
    if kinds(line) != ("response_item", "message") || line.pointer("/payload/role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let text = blocks_text(line.pointer("/payload/content"));
    (!text.is_empty()).then_some(text)
}

fn line_meta(line: &Value) -> LineMeta {
    let mut meta = LineMeta { prompt: user_text(line), said: assistant_text(line), ..Default::default() };
    match kinds(line) {
        ("event_msg", "task_started") => {
            meta.turn_started = Some(line.pointer("/payload/turn_id").and_then(Value::as_str).unwrap_or_default().to_string());
        }
        ("event_msg", "task_complete") => meta.turn_ended = true,
        ("event_msg", "turn_aborted") => {
            meta.turn_ended = true;
            meta.interrupted = true;
        }
        _ => {}
    }
    meta
}

/// Folds what a line says about its session in (last words, turns); returns whether the
/// thread list should change.
fn note_line(s: &mut Session, line: &Value, only: Option<&str>, live: bool, emits: &mut Emits) -> bool {
    note_start(s, line, only);
    let meta = line_meta(line);
    let mut changed = false;
    if let Some(said) = meta.said {
        s.said = Some(said);
        changed = live;
    }
    if meta.prompt.is_some() {
        changed |= live;
    }
    if let Some(turn) = meta.turn_started {
        s.open_turn = true;
        s.turn_id = Some(turn).filter(|t| !t.is_empty());
        s.interrupted = false;
        s.assumed = None;
        if live {
            changed |= s.set_busy(true, emits);
        }
    }
    if meta.turn_ended {
        s.open_turn = false;
        s.turn_id = None;
        s.interrupted = meta.interrupted;
        if live {
            changed |= s.set_busy(false, emits);
        }
    }
    changed
}

fn chat_message(role: &str, content: Vec<Value>, ts: Option<u64>, id: &str) -> Value {
    let mut m = json!({ "role": role, "content": content, "__openclaw": { "id": id } });
    if let Some(ts) = ts {
        m["timestamp"] = json!(ts);
    }
    m
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn cut(text: &str) -> String {
    if text.len() <= TOOL_TEXT_MAX {
        return text.to_string();
    }
    let mut end = TOOL_TEXT_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut: {} more characters)", &text[..end], text.len() - end)
}

fn tool_call(id: &str, name: &str, arguments: Value, ts: Option<u64>) -> Value {
    chat_message("assistant", vec![json!({ "type": "toolCall", "id": id, "name": name, "arguments": arguments })], ts, id)
}

fn tool_result(id: &str, name: &str, text: &str, is_error: bool, ts: Option<u64>) -> Value {
    let mut m = chat_message("toolResult", vec![text_block(&cut(text))], ts, &format!("{id}:result"));
    m["toolCallId"] = json!(id);
    m["toolName"] = json!(name);
    m["isError"] = json!(is_error);
    m
}

/// A finished tool, as its call and its result.
fn tool_pair(id: &str, name: &str, arguments: Value, text: &str, is_error: bool, ts: Option<u64>) -> Vec<Value> {
    vec![tool_call(id, name, arguments, ts), tool_result(id, name, text, is_error, ts)]
}

/// The command you'd type: `["/bin/zsh", "-lc", "ls"]` → `ls`.
fn command_line(command: Option<&Value>) -> String {
    match command {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [_, flag, script] if flag.starts_with('-') && flag.ends_with('c') => script.to_string(),
                _ => parts.join(" "),
            }
        }
        _ => String::new(),
    }
}

fn failed(item: &Value) -> bool {
    matches!(item.get("status").and_then(Value::as_str), Some("failed" | "declined" | "Failed" | "Declined"))
}

/// A tool's output: a string, `[{text}]` blocks, or `{content: …}`.
fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => blocks_text(output),
        Some(Value::Object(o)) => output_text(o.get("content").or_else(|| o.get("output"))),
        _ => String::new(),
    }
}

/// A finished `item_completed` item that's worth showing → messages.
fn convert_item(item: &Value, ts: Option<u64>) -> Vec<Value> {
    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    if id.is_empty() {
        return Vec::new();
    }
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "CommandExecution" => {
            let output = ["aggregated_output", "formatted_output", "stdout"]
                .iter()
                .find_map(|k| item.get(*k).and_then(Value::as_str).filter(|t| !t.is_empty()))
                .unwrap_or_default();
            let exit = item.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
            tool_pair(id, "exec", json!({ "command": command_line(item.get("command")) }), output, exit != 0 || failed(item), ts)
        }
        "McpToolCall" => {
            let name = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let error = item.pointer("/error/message").and_then(Value::as_str);
            let text = error.map_or_else(|| output_text(item.get("result")), str::to_string);
            let is_error = error.is_some() || failed(item) || item.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(false);
            tool_pair(id, name, item.get("arguments").cloned().unwrap_or_else(|| json!({})), &text, is_error, ts)
        }
        "FileChange" => {
            let paths: Vec<&str> = item.get("changes").and_then(Value::as_object).map(|c| c.keys().map(String::as_str).collect()).unwrap_or_default();
            let arguments = json!({ "path": paths.first().copied().unwrap_or_default() });
            tool_pair(id, "apply_patch", arguments, &paths.join("\n"), failed(item), ts)
        }
        "Extension" => {
            let query = item.pointer("/action/query").or_else(|| item.get("query")).and_then(Value::as_str);
            let arguments = match (query.filter(|q| !q.is_empty()), item.pointer("/action/url").and_then(Value::as_str)) {
                (_, Some(url)) => json!({ "url": url }),
                (Some(query), None) => json!({ "query": query }),
                (None, None) => json!({}),
            };
            let results = item.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            let found: Vec<String> = results
                .iter()
                .filter_map(|r| {
                    let title = r.get("title").and_then(Value::as_str)?;
                    Some(r.get("url").and_then(Value::as_str).map_or_else(|| title.to_string(), |url| format!("{title}: {url}")))
                })
                .collect();
            tool_pair(id, "web_search", arguments, &found.join("\n"), failed(item), ts)
        }
        "ImageView" => {
            let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
            tool_pair(id, "view_image", json!({ "path": path }), "", false, ts)
        }
        _ => Vec::new(),
    }
}

fn learn_calls(line: &Value, calls: &mut HashMap<String, String>) {
    let p = line.get("payload").unwrap_or(&Value::Null);
    if let ("response_item", "function_call" | "custom_tool_call") = kinds(line) {
        if let (Some(id), Some(name)) = (p.get("call_id").and_then(Value::as_str), p.get("name").and_then(Value::as_str)) {
            if shown_call(name, kinds(line).1) {
                calls.insert(id.to_string(), name.to_string());
            }
        }
    }
}

/// Which calls are shown as such. `exec` wraps a script around the real tools, which show up
/// as items of their own; `wait` is it polling them.
fn shown_call(name: &str, kind: &str) -> bool {
    !(kind == "custom_tool_call" && name == "exec") && name != "wait"
}

/// A rollout line → contract ChatMessages (none for bookkeeping lines).
fn convert_line(line: &Value, calls: &mut HashMap<String, String>) -> Vec<Value> {
    let ts = line.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms);
    let p = line.get("payload").unwrap_or(&Value::Null);
    let stamp = || line.get("ordinal").and_then(Value::as_u64).or(ts).unwrap_or(0);
    let id_or = |prefix: &str| p.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map_or_else(|| format!("{prefix}-{}", stamp()), str::to_string);
    if let Some(text) = user_text(line) {
        let id = p.pointer("/item/id").and_then(Value::as_str).map_or_else(|| format!("user-{}", stamp()), str::to_string);
        return vec![chat_message("user", vec![text_block(&text)], ts, &id)];
    }
    if let Some(text) = assistant_text(line) {
        return vec![chat_message("assistant", vec![text_block(&text)], ts, &id_or("assistant"))];
    }
    let call_id = p.get("call_id").and_then(Value::as_str).unwrap_or_default();
    match kinds(line) {
        ("event_msg", "item_completed") => p.get("item").map(|item| convert_item(item, ts)).unwrap_or_default(),
        ("response_item", kind @ ("function_call" | "custom_tool_call")) => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
            if call_id.is_empty() || !shown_call(name, kind) {
                return Vec::new();
            }
            learn_calls(line, calls);
            let arguments = match p.get("arguments") {
                Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| json!({ "input": raw })),
                Some(other) => other.clone(),
                None => json!({ "input": p.get("input").and_then(Value::as_str).unwrap_or_default() }),
            };
            vec![tool_call(call_id, name, arguments, ts)]
        }
        ("response_item", "function_call_output" | "custom_tool_call_output") => match calls.get(call_id) {
            Some(name) => vec![tool_result(call_id, name, &output_text(p.get("output")), false, ts)],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

// ---- the shared app server -------------------------------------------------------------

type Socket = WebSocketStream<tokio::io::Join<ChildStdout, ChildStdin>>;

struct Server {
    out: mpsc::UnboundedSender<Message>,
    calls: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next: AtomicU64,
}

impl Server {
    fn send(&self, frame: Value) {
        let _ = self.out.send(Message::text(frame.to_string()));
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.calls.lock().unwrap().insert(id, tx);
        self.send(json!({ "id": id, "method": method, "params": params }));
        let answer = tokio::time::timeout(SERVER_TIMEOUT, rx).await;
        self.calls.lock().unwrap().remove(&id);
        match answer {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("lost the connection to Codex".to_string()),
            Err(_) => Err(format!("Codex didn't answer {method}")),
        }
    }

    fn settle(&self, frame: &Value) {
        let Some(tx) = frame.get("id").and_then(Value::as_u64).and_then(|id| self.calls.lock().unwrap().remove(&id)) else { return };
        let result = match frame.get("error") {
            Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("Codex refused").to_string()),
            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(result);
    }
}

fn server(inner: &Inner) -> Option<Arc<Server>> {
    inner.server.lock().unwrap().clone()
}

/// Stays connected to the shared app server for as long as it's up. It may never be:
/// reading doesn't need it, and replies have other ways in.
async fn follow_server(inner: Arc<Inner>) {
    while !inner.stopped.load(Ordering::SeqCst) {
        let codex = inner.codex.lock().unwrap().clone();
        if let Some(codex) = codex.filter(|_| !inner.no_server && server_may_be_up(&inner.home)) {
            match open_server(&codex).await {
                Ok((child, socket)) => {
                    talk(&inner, socket).await;
                    drop(child);
                    let emits = forget_server(&inner);
                    flush(&inner, emits);
                }
                Err(e) => log::debug!("[codex] no shared app server: {e}"),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(SERVER_RETRY) => {}
            _ = inner.server_retry.notified() => {}
        }
    }
}

/// Saves starting `codex app-server proxy` every few seconds on a machine with no server.
#[cfg(unix)]
fn server_may_be_up(home: &Path) -> bool {
    home.join("app-server-control/app-server-control.sock").exists()
}

#[cfg(not(unix))]
fn server_may_be_up(_home: &Path) -> bool {
    true
}

/// `codex app-server proxy` copies its stdio to and from the control socket, whatever
/// that is on this OS, and the socket speaks WebSocket.
async fn open_server(codex: &Path) -> Result<(Child, Socket), String> {
    let mut cmd = cli::command(codex);
    cmd.args(["app-server", "proxy"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let handshake = tokio_tungstenite::client_async("ws://localhost/", tokio::io::join(stdout, stdin));
    let (socket, _) = tokio::time::timeout(Duration::from_secs(5), handshake).await.map_err(|_| "the handshake timed out".to_string())?.map_err(|e| e.to_string())?;
    Ok((child, socket))
}

/// Pumps one connection until it drops or littleguys stops.
async fn talk(inner: &Arc<Inner>, mut socket: Socket) {
    let (out, mut outgoing) = mpsc::unbounded_channel();
    let srv = Arc::new(Server { out, calls: Mutex::new(HashMap::new()), next: AtomicU64::new(1) });
    let greeting = {
        let (inner, srv) = (inner.clone(), srv.clone());
        tauri::async_runtime::spawn(async move {
            match greet(&inner, &srv).await {
                Ok(()) => *inner.server.lock().unwrap() = Some(srv),
                Err(e) => {
                    log::warn!("[codex] the shared app server won't talk: {e}");
                    let _ = srv.out.send(Message::Close(None));
                }
            }
        })
    };
    let mut check = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(text))) => on_frame(inner, &srv, text.as_str()),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            Some(frame) = outgoing.recv() => {
                let closing = matches!(frame, Message::Close(_));
                if socket.send(frame).await.is_err() || closing {
                    break;
                }
            }
            _ = check.tick() => if inner.stopped.load(Ordering::SeqCst) { break },
        }
    }
    greeting.abort();
}

/// Says hello (with the experimental API off) and asks what's waiting already.
async fn greet(inner: &Arc<Inner>, srv: &Server) -> Result<(), String> {
    let hello = json!({
        "clientInfo": { "name": "littleguys", "title": "littleguys", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": false, "requestAttestation": false, "optOutNotificationMethods": QUIET },
    });
    srv.call("initialize", hello).await?;
    srv.send(json!({ "method": "initialized" }));
    let loaded = srv.call("thread/loaded/list", json!({})).await?;
    let ids: Vec<&str> = loaded.get("data").and_then(Value::as_array).map(|ids| ids.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    for id in ids {
        if let Ok(read) = srv.call("thread/read", json!({ "threadId": id })).await {
            if let Some(status) = read.pointer("/thread/status") {
                note_status(inner, id, status);
            }
        }
    }
    Ok(())
}

/// Lost the server: what it said no longer stands.
fn forget_server(inner: &Inner) -> Emits {
    *inner.server.lock().unwrap() = None;
    let mut st = inner.st.lock().unwrap();
    st.waiting.clear();
    st.joined.clear();
    let mut emits: Emits = st.approvals.drain().map(|(id, _)| ("exec.approval.resolved", json!({ "id": id }))).collect();
    emits.push(refresh());
    emits
}

fn on_frame(inner: &Arc<Inner>, srv: &Server, raw: &str) {
    let Ok(frame) = serde_json::from_str::<Value>(raw) else { return };
    let params = frame.get("params").unwrap_or(&Value::Null);
    match (frame.get("method").and_then(Value::as_str), frame.get("id")) {
        (Some(method), Some(id)) => on_request(inner, id, method, params),
        (Some(method), None) => on_notification(inner, method, params),
        (None, Some(_)) => srv.settle(&frame),
        (None, None) => {}
    }
}

/// `active` with flags → what the session waits for.
fn waiting_on(status: &Value) -> Option<WaitingOn> {
    let flags = status.get("activeFlags").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let has = |flag: &str| flags.iter().any(|f| f == flag);
    if has("waitingOnApproval") {
        Some(WaitingOn::Approval)
    } else if has("waitingOnUserInput") {
        Some(WaitingOn::Input)
    } else {
        None
    }
}

fn note_status(inner: &Inner, id: &str, status: &Value) {
    let waiting = waiting_on(status);
    let mut emits: Emits = Vec::new();
    {
        let mut st = inner.st.lock().unwrap();
        let before = match waiting {
            Some(on) => st.waiting.insert(id.to_string(), on),
            None => st.waiting.remove(id),
        };
        if before == waiting {
            return;
        }
        if waiting != Some(WaitingOn::Approval) {
            // Answered in the terminal, or the turn moved on.
            let gone: Vec<String> = st.approvals.iter().filter(|(_, a)| a.session_id == id).map(|(k, _)| k.clone()).collect();
            for approval in gone {
                st.approvals.remove(&approval);
                emits.push(("exec.approval.resolved", json!({ "id": approval })));
            }
        }
        if st.sessions.contains_key(id) {
            emits.push(refresh());
        }
    }
    flush(inner, emits);
}

fn on_notification(inner: &Arc<Inner>, method: &str, params: &Value) {
    match method {
        "thread/status/changed" => {
            if let (Some(id), Some(status)) = (params.get("threadId").and_then(Value::as_str), params.get("status")) {
                note_status(inner, id, status);
            }
        }
        "serverRequest/resolved" => {
            let request = params.get("requestId");
            let mut st = inner.st.lock().unwrap();
            let done = st.approvals.iter().find(|(_, a)| Some(&a.request) == request).map(|(k, _)| k.clone());
            if let Some(approval) = done {
                st.approvals.remove(&approval);
                drop(st);
                emit(inner, "exec.approval.resolved", json!({ "id": approval }));
            }
        }
        _ => {}
    }
}

/// A prompt from a session littleguys joined. Only the two kinds an approval card can
/// answer: the rest stay in Codex (the thread says it's waiting).
fn on_request(inner: &Arc<Inner>, id: &Value, method: &str, params: &Value) {
    let (fallback, tool) = match method {
        "item/commandExecution/requestApproval" => ("Run a command", "exec"),
        "item/fileChange/requestApproval" => ("Change files", "apply_patch"),
        _ => return,
    };
    let Some(sid) = params.get("threadId").and_then(Value::as_str) else { return };
    let approval = {
        let mut st = inner.st.lock().unwrap();
        let Some(s) = st.sessions.get(sid).filter(|s| s.tail.is_some() && !s.skip) else { return };
        let (key, cwd) = (s.key(), s.cwd.clone());
        let item = params.get("approvalId").or_else(|| params.get("itemId")).and_then(Value::as_str).unwrap_or(sid);
        let approval = format!("codex-{item}");
        let payload = approval_payload(&approval, &key, &cwd, fallback, tool, params);
        // A rejoin replays a prompt that's still open: same card, a new request id.
        let known = st.approvals.insert(approval, Approval { session_id: sid.to_string(), request: id.clone(), payload: payload.clone() }).is_some();
        st.joined.insert(sid.to_string(), Instant::now());
        (!known).then_some(payload)
    };
    if let Some(payload) = approval {
        emit(inner, "exec.approval.requested", payload);
    }
}

/// The command you'd type: `/bin/zsh -lc 'ls -la'` → `ls -la`.
fn unwrap_shell(command: &str) -> &str {
    let Some((program, rest)) = command.split_once(' ') else { return command };
    let shell = matches!(program.rsplit('/').next(), Some("sh" | "bash" | "zsh"));
    let script = rest.strip_prefix("-lc '").or_else(|| rest.strip_prefix("-c '")).and_then(|s| s.strip_suffix('\''));
    script.filter(|s| shell && !s.contains('\'')).unwrap_or(command)
}

fn approval_payload(id: &str, key: &str, cwd: &str, fallback: &str, tool: &str, params: &Value) -> Value {
    let reason = params.get("reason").and_then(Value::as_str).filter(|r| !r.is_empty());
    let command = params.get("command").and_then(Value::as_str).filter(|c| !c.is_empty()).map(unwrap_shell).or(reason).unwrap_or(fallback);
    let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or(cwd);
    let mut presentation = json!({ "commandText": command, "allowedDecisions": ["allow-once", "allow-always", "deny"] });
    if let Some(reason) = reason.filter(|r| *r != command) {
        presentation["warningText"] = json!(reason);
    }
    json!({
        "id": id,
        "agentId": AGENT_ID,
        "sessionKey": key,
        "createdAtMs": now_ms(),
        "presentation": presentation,
        "request": { "id": id, "command": command, "agentId": AGENT_ID, "sessionKey": key, "cwd": cwd, "toolName": tool },
    })
}

fn approval_resolve(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let id = params.get("id").and_then(Value::as_str).ok_or_else(|| RpcError::new("INVALID_REQUEST", "id is required"))?;
    let decision = match params.get("decision").and_then(Value::as_str).unwrap_or("deny") {
        "allow-once" | "allow" => "accept",
        "allow-always" => "acceptForSession",
        _ => "decline",
    };
    let request = inner.st.lock().unwrap().approvals.get(id).map(|a| a.request.clone());
    let Some(request) = request else { return Err(RpcError::new("NOT_FOUND", "That prompt was already answered.")) };
    let srv = server(inner).ok_or_else(|| RpcError::new("UNAVAILABLE", "Lost the connection to Codex. Answer it in the terminal."))?;
    // It goes when the server says it's settled (`serverRequest/resolved`).
    srv.send(json!({ "id": request, "result": { "decision": decision } }));
    Ok(json!({ "ok": true }))
}

/// Joins sessions waiting on an approval: the server then replays their prompt.
fn join_waiting(inner: &Arc<Inner>) {
    let Some(srv) = server(inner) else { return };
    let todo: Vec<String> = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let todo: Vec<String> = st
            .waiting
            .iter()
            .filter(|(id, on)| **on == WaitingOn::Approval && !st.joined.contains_key(*id) && st.sessions.get(*id).is_some_and(|s| s.tail.is_some() && !s.skip))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &todo {
            st.joined.insert(id.clone(), Instant::now());
        }
        todo
    };
    for id in todo {
        let (inner, srv) = (inner.clone(), srv.clone());
        tauri::async_runtime::spawn(async move {
            if let Err(e) = srv.call("thread/resume", json!({ "threadId": id, "excludeTurns": true })).await {
                log::warn!("[codex] couldn't join {id}: {e}");
                inner.st.lock().unwrap().joined.remove(&id);
            }
        });
    }
}

/// Lets go of joined sessions that went quiet, so the server can unload them.
fn release_joined(inner: &Arc<Inner>) {
    let Some(srv) = server(inner) else { return };
    let done: Vec<String> = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let quiet = |id: &String| st.sessions.get(id).is_none_or(|s| !s.busy && s.outbox.is_empty() && !s.delivering) && !st.waiting.contains_key(id);
        let due: Vec<String> = st.joined.iter().filter(|(id, at)| at.elapsed() > JOIN_GRACE && quiet(id)).map(|(id, _)| id.clone()).collect();
        for id in &due {
            st.joined.remove(id);
        }
        due
    };
    for id in done {
        let srv = srv.clone();
        tauri::async_runtime::spawn(async move {
            let left = srv.call("thread/unsubscribe", json!({ "threadId": id })).await;
            log::debug!("[codex] let go of {id}: {left:?}");
        });
    }
}

// ---- sending -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Route {
    /// A turn on the shared server (it steers a busy turn, loads a session that ended).
    Turn,
    /// `codex queue --resume`.
    Queue,
    /// `codex exec resume`: a headless turn, for a session nothing has open.
    Exec,
}

const TOO_OLD: &str = "This version of Codex can't take messages from outside its terminal. Update Codex, or write to it there.";

/// How a message reaches a session. Busy, it waits for the turn to end, so it goes in the
/// queue. Otherwise the shared server is the quickest way in; a session open somewhere else
/// reads its queue every few seconds; one nothing has open only `codex exec` can run.
fn route(busy: bool, live: bool, server: bool, can_queue: bool) -> Result<Route, &'static str> {
    match (busy, server, live, can_queue) {
        (true, _, _, true) => Ok(Route::Queue),
        (_, true, _, _) => Ok(Route::Turn),
        (false, false, true, true) => Ok(Route::Queue),
        (true, false, _, false) | (false, false, true, false) => Err(TOO_OLD),
        (false, false, false, _) => Ok(Route::Exec),
    }
}

/// `chat.send` on a thread: handed over in the background, the rest of what you write after it.
fn chat_send(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    let text = params.get("message").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "Write something first."));
    }
    let text = text.to_string();
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
    let deliver_now = {
        let mut st = inner.st.lock().unwrap();
        let s = st.sessions.get_mut(&sid).filter(|s| s.tail.is_some() && !s.skip).ok_or_else(|| RpcError::new("NOT_FOUND", "littleguys lost track of this session."))?;
        s.outbox.push(text);
        // You wrote to it: it stays in the list once it ends.
        if !s.kept {
            s.kept = true;
            s.unsaved = true;
        }
        !std::mem::replace(&mut s.delivering, true)
    };
    if deliver_now {
        let inner = inner.clone();
        tauri::async_runtime::spawn(async move { deliver(inner, sid).await });
    }
    Ok(json!({ "runId": run_id, "status": "started" }))
}

#[derive(Debug, PartialEq)]
enum Handed {
    Turn,
    Queued,
}

/// Hands over what's waiting for a session, until nothing is.
async fn deliver(inner: Arc<Inner>, sid: String) {
    loop {
        let job = {
            let mut st = inner.st.lock().unwrap();
            let Some(s) = st.sessions.get_mut(&sid) else { return };
            if s.outbox.is_empty() {
                s.delivering = false;
                return;
            }
            s.handed += 1;
            (std::mem::take(&mut s.outbox).join("\n\n"), s.busy, s.live, s.interrupted, s.cwd.clone(), s.key())
        };
        let (text, busy, live, interrupted, cwd, key) = job;
        let result = hand_over(&inner, &sid, &text, busy, live, &cwd).await;
        if let Err(e) = &result {
            log::warn!("[codex] couldn't reach {sid}: {e}");
        }
        let mut emits: Emits = Vec::new();
        {
            let mut st = inner.st.lock().unwrap();
            if let Some(s) = st.sessions.get_mut(&sid) {
                match result {
                    // Codex holds the queue back after an interrupted turn, until the next prompt.
                    Ok(Handed::Queued) if interrupted && !busy => emits.push((
                        "chat",
                        json!({ "sessionKey": key, "runId": s.run_id(), "state": "error", "errorMessage": "It's queued, but this session was interrupted in its terminal, so it goes out with the next prompt typed there." }),
                    )),
                    Ok(_) if s.busy => {}
                    Ok(_) => {
                        s.set_busy(true, &mut emits);
                        s.assumed = Some(Instant::now());
                    }
                    Err(e) => emits.push(("chat", json!({ "sessionKey": key, "runId": s.run_id(), "state": "error", "errorMessage": format!("Couldn't reach this session: {e}") }))),
                }
            }
        }
        emits.push(refresh());
        flush(&inner, emits);
    }
}

async fn hand_over(inner: &Arc<Inner>, sid: &str, text: &str, busy: bool, live: bool, cwd: &str) -> Result<Handed, String> {
    let srv = server(inner);
    let can_queue = inner.can_queue.load(Ordering::SeqCst);
    match route(busy, live, srv.is_some(), can_queue)? {
        Route::Queue => queue(inner, sid, text).await.map(|()| Handed::Queued),
        Route::Exec => exec_resume(inner, sid, text, cwd).await.map(|()| Handed::Turn),
        Route::Turn => {
            let srv = srv.ok_or("lost the connection to Codex")?;
            match start_turn(inner, &srv, sid, text).await {
                Ok(()) => Ok(Handed::Turn),
                // It's open somewhere the server can't reach: an active writer of its own.
                Err(e) if can_queue => {
                    log::debug!("[codex] no turn on the shared server for {sid}: {e}");
                    queue(inner, sid, text).await.map(|()| Handed::Queued)
                }
                Err(e) => Err(e),
            }
        }
    }
}

fn turn_input(text: &str) -> Value {
    json!([{ "type": "text", "text": text, "text_elements": [] }])
}

/// Joins the session on the shared server (which loads it if it ended) and starts a turn.
async fn start_turn(inner: &Inner, srv: &Server, sid: &str, text: &str) -> Result<(), String> {
    srv.call("thread/resume", json!({ "threadId": sid, "excludeTurns": true })).await?;
    inner.st.lock().unwrap().joined.insert(sid.to_string(), Instant::now());
    srv.call("turn/start", json!({ "threadId": sid, "input": turn_input(text) })).await?;
    Ok(())
}

/// `codex queue --thread <id> --message <text> --resume`: safe however the session is
/// hosted. It warns, but queues, when `--resume` doesn't apply.
async fn queue(inner: &Inner, sid: &str, text: &str) -> Result<(), String> {
    let codex = codex_path(inner)?;
    // `--message=` so that a message starting with '-' isn't read as a flag.
    let mut cmd = cli::command(&codex);
    cmd.args(["queue", "--thread", sid, "--resume"]).arg(format!("--message={text}"));
    output(cmd, QUEUE_TIMEOUT).await.map(|_| ())
}

fn codex_path(inner: &Inner) -> Result<PathBuf, String> {
    inner.codex.lock().unwrap().clone().ok_or_else(|| "Codex isn't installed".to_string())
}

// A prompt starting with '-' would read as a flag.
fn prompt_arg(text: &str) -> String {
    if text.starts_with('-') {
        format!(" {text}")
    } else {
        text.to_string()
    }
}

/// What `codex exec --json` says on stdout that matters here.
#[derive(Debug, PartialEq)]
enum ExecEvent {
    Thread(String),
    Failed(String),
    Other,
}

fn exec_event(line: &str) -> ExecEvent {
    let Ok(event) = serde_json::from_str::<Value>(line) else { return ExecEvent::Other };
    match event.get("type").and_then(Value::as_str) {
        Some("thread.started") => event.get("thread_id").and_then(Value::as_str).map_or(ExecEvent::Other, |id| ExecEvent::Thread(id.to_string())),
        Some("turn.failed") | Some("error") => {
            let why = event.pointer("/error/message").or_else(|| event.get("message")).and_then(Value::as_str).unwrap_or("Codex stopped with an error");
            ExecEvent::Failed(why.to_string())
        }
        _ => ExecEvent::Other,
    }
}

/// Runs `codex <args>` (`exec …--json`), which works without the shared server. It runs on in
/// the background; this resolves to the session's id, and a way to stop it, once it says.
/// Approvals can't be asked for there, so it does what its config allows without asking.
async fn run_exec(inner: &Arc<Inner>, args: Vec<String>, cwd: &str) -> Result<(String, oneshot::Sender<()>), String> {
    let codex = codex_path(inner)?;
    let mut cmd = cli::command(&codex);
    cmd.args(&args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(false);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (id_tx, id_rx) = oneshot::channel::<Result<String, String>>();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let inner = inner.clone();
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut id_tx = Some(id_tx);
        let mut sid: Option<String> = None;
        let mut why: Option<String> = None;
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => match exec_event(&line) {
                        ExecEvent::Thread(id) => {
                            sid = Some(id.clone());
                            if let Some(tx) = id_tx.take() {
                                let _ = tx.send(Ok(id));
                            }
                        }
                        ExecEvent::Failed(message) => why = Some(message),
                        ExecEvent::Other => {}
                    },
                    _ => break,
                },
                Ok(()) = &mut stop_rx => {
                    let _ = child.start_kill();
                    break;
                }
            }
        }
        let ok = child.wait().await.is_ok_and(|status| status.success());
        let why = why.or_else(|| (!ok).then(|| "`codex exec` stopped with an error".to_string()));
        if let Some(tx) = id_tx.take() {
            let _ = tx.send(Err(why.unwrap_or_else(|| "Codex didn't say which session it started".to_string())));
            return;
        }
        let Some(sid) = sid else { return };
        let mut emits: Emits = Vec::new();
        {
            let mut st = inner.st.lock().unwrap();
            if let Some(s) = st.sessions.get_mut(&sid) {
                s.exec = None;
                if let Some(why) = why {
                    emits.push(("chat", json!({ "sessionKey": s.key(), "runId": s.run_id(), "state": "error", "errorMessage": why })));
                    s.set_busy(false, &mut emits);
                }
            }
        }
        flush(&inner, emits);
    });
    match tokio::time::timeout(Duration::from_secs(60), id_rx).await {
        Ok(Ok(Ok(sid))) => Ok((sid, stop_tx)),
        Ok(Ok(Err(why))) => Err(why),
        _ => Err("Codex didn't say which session it started".to_string()),
    }
}

async fn exec_resume(inner: &Arc<Inner>, sid: &str, text: &str, cwd: &str) -> Result<(), String> {
    let dir = if Path::new(cwd).is_dir() { cwd.to_string() } else { home().display().to_string() };
    let args = ["exec", "resume", "--json", "--skip-git-repo-check", sid].iter().map(|a| a.to_string()).chain([prompt_arg(text)]).collect();
    let (_, stop) = run_exec(inner, args, &dir).await?;
    if let Some(s) = inner.st.lock().unwrap().sessions.get_mut(sid) {
        s.exec = Some(stop);
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
    let result = launch(&inner, &key, &cwd, &text, true).await;
    let mut emits: Emits = vec![refresh()];
    {
        let mut st = inner.st.lock().unwrap();
        match result {
            Ok(sid) => {
                // What you wrote while it started goes out when its turn ends.
                if let Some(d) = st.drafts.remove(&key) {
                    if !d.outbox.is_empty() {
                        let s = st.session(&sid);
                        s.outbox.extend(d.outbox);
                        if !std::mem::replace(&mut s.delivering, true) {
                            let inner = inner.clone();
                            tauri::async_runtime::spawn(async move { deliver(inner, sid).await });
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("[codex] couldn't start a session in {cwd}: {e}");
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
}

/// Makes `sid` thread `key`, from the top of its rollout, at work on its first turn.
fn bind(inner: &Inner, sid: &str, key: &str, cwd: &str, keep: bool, path: Option<PathBuf>) {
    let mut emits: Emits = vec![refresh()];
    {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        let known = st.started.remove(sid).unwrap_or_default();
        let thread = Started { key: key.to_string(), at: now_ms(), cwd: Some(cwd.to_string()), keep, ..known };
        remember(&inner.started_path, &mut st.started, sid, thread);
        let s = st.session(sid);
        s.kept = keep;
        s.key = Some(key.to_string());
        s.tail = Some(Tail::default());
        s.cwd = cwd.to_string();
        s.path = path.or_else(|| s.path.take());
        s.live = true;
        let now = now_ms();
        s.started_at = s.started_at.max(now);
        s.updated_at = s.updated_at.max(now);
        s.set_busy(true, &mut emits);
        s.assumed = Some(Instant::now());
    }
    flush(inner, emits);
}

/// A new session in `cwd`, shown as thread `key`: on the shared server, else with `codex exec`.
async fn launch(inner: &Arc<Inner>, key: &str, cwd: &str, text: &str, keep: bool) -> Result<String, String> {
    if !Path::new(cwd).is_dir() {
        return Err(format!("{cwd} isn't a folder on this {}.", crate::platform::COMPUTER));
    }
    if let Some(srv) = server(inner) {
        let mut params = json!({ "cwd": cwd });
        if let Some(model) = &inner.model {
            params["model"] = json!(model);
        }
        match srv.call("thread/start", params).await {
            Ok(started) => {
                let sid = started.pointer("/thread/id").and_then(Value::as_str).ok_or("Codex didn't say which session it started")?.to_string();
                let path = started.pointer("/thread/path").and_then(Value::as_str).map(PathBuf::from);
                inner.st.lock().unwrap().joined.insert(sid.clone(), Instant::now());
                bind(inner, &sid, key, cwd, keep, path);
                srv.call("turn/start", json!({ "threadId": sid, "input": turn_input(text) })).await?;
                return Ok(sid);
            }
            Err(e) => log::warn!("[codex] the shared app server wouldn't start a session: {e}"),
        }
    }
    let mut args: Vec<String> = ["exec", "--json", "--skip-git-repo-check"].iter().map(|a| a.to_string()).collect();
    if let Some(model) = &inner.model {
        args.extend(["-m".to_string(), model.clone()]);
    }
    args.push(prompt_arg(text));
    let (sid, stop) = run_exec(inner, args, cwd).await?;
    bind(inner, &sid, key, cwd, keep, None);
    if let Some(s) = inner.st.lock().unwrap().sessions.get_mut(&sid) {
        s.exec = Some(stop);
    }
    Ok(sid)
}

// ---- routines -----------------------------------------------------------------------------

struct Runner(Weak<Inner>);

/// What says a routine's turn is over.
type TurnDone = Box<dyn Fn(&Session) -> bool + Send + Sync>;

impl RoutineRunner for Runner {
    /// A new session for each run (in the routine's folder, `payload.cwd`), or a message to
    /// the thread it targets; resolves when that turn ends.
    fn run_turn(&self, agent_id: String, target: String, job_id: String, message: String) -> BoxFuture<'static, Result<TurnOutcome, String>> {
        let inner = self.0.clone();
        Box::pin(async move {
            let inner = inner.upgrade().ok_or("littleguys is shutting down")?;
            if agent_id != AGENT_ID {
                return Err(format!("no guy {agent_id} here"));
            }
            if !inner.st.lock().unwrap().connected {
                return Err("littleguys isn't following Codex right now.".into());
            }
            let (key, sid, done): (String, String, TurnDone) = match target.strip_prefix("session:") {
                None => {
                    let job = inner.scheduler.handle("cron.get", json!({ "id": job_id })).await.ok();
                    let folder = job.as_ref().and_then(|j| j.pointer("/payload/cwd")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                    let cwd = folder.unwrap_or_else(|| inner.st.lock().unwrap().default_folder());
                    let key = keys::routine_run(AGENT_ID, &job_id, &uuid::Uuid::new_v4().to_string());
                    let sid = launch(&inner, &key, &cwd, &message, false).await?;
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
                        let handed = st.sessions.get(&sid).map_or(0, |s| s.handed);
                        (sid, handed)
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
            return Err("littleguys stopped following Codex before it finished.".into());
        }
        if Instant::now() > deadline {
            return Err(format!("Still going after {} hours, so littleguys stopped waiting.", ROUTINE_MAX.as_secs() / 3600));
        }
        tokio::time::sleep(TICK).await;
    }
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
            "identity": { "name": name, "emoji": "🌀" },
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
    let mut sessions: Vec<&Session> = st.sessions.values().filter(|s| s.listable(now) && s.shown() && archived.is_none_or(|a| a == s.archived())).collect();
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
    let rows: Vec<Value> = drafts.chain(sessions.iter().map(|s| session_row(s, st.waiting.get(&s.id)))).collect();
    json!({ "count": rows.len(), "sessions": rows })
}

fn session_row(s: &Session, waiting: Option<&WaitingOn>) -> Value {
    let mut row = json!({
        "key": s.key(),
        "agentId": AGENT_ID,
        "kind": "direct",
        "sessionId": s.id,
        "workspaceDir": s.cwd,
        "derivedTitle": s.title(),
        "lastMessagePreview": s.preview(waiting),
        "createdAt": s.started_at,
        "updatedAt": s.updated_at.max(s.started_at),
        "archived": s.archived(),
        "hasActiveRun": s.busy,
        "status": if s.busy { "running" } else { "done" },
    });
    s.flags.add_to(&mut row);
    row
}

/// Rename, pin, read or unread, archive or unarchive a thread, all kept by littleguys:
/// Codex's own name and archive aren't touched. Archiving a thread that hasn't started
/// drops it.
fn sessions_patch(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    let row = {
        let mut st = inner.st.lock().unwrap();
        let st = &mut *st;
        match thread {
            Thread::Draft if params.get("archived").and_then(Value::as_bool) == Some(true) => {
                st.drafts.remove(&key);
                json!({ "key": key, "agentId": AGENT_ID, "archived": true })
            }
            Thread::Draft => return Err(RpcError::new("INVALID_REQUEST", "This thread hasn't started yet.")),
            Thread::Session(sid) => {
                let s = st.sessions.get_mut(&sid).ok_or_else(|| RpcError::new("NOT_FOUND", "Not a Codex session."))?;
                s.unsaved |= s.flags.patch(params);
                session_row(s, st.waiting.get(&sid))
            }
        }
    };
    save_flags(inner);
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "patch" }));
    Ok(row)
}

/// Delete a thread: littleguys stops listing its session. The session keeps running, and
/// its rollout stays, for `codex resume`.
fn sessions_delete(inner: &Inner, params: &Value) -> Result<Value, RpcError> {
    let (key, thread) = thread_of(inner, params)?;
    {
        let mut st = inner.st.lock().unwrap();
        match thread {
            Thread::Draft => {
                st.drafts.remove(&key);
            }
            Thread::Session(sid) => {
                if let Some(s) = st.sessions.get_mut(&sid) {
                    s.hidden = true;
                    s.unsaved = true;
                }
            }
        }
    }
    save_flags(inner);
    emit(inner, "sessions.changed", json!({ "sessionKey": key, "agentId": AGENT_ID, "reason": "deleted" }));
    Ok(json!({ "ok": true, "deleted": true, "key": key }))
}

/// Saves what changed on threads (your flags, a delete) with the threads littleguys keeps.
fn save_flags(inner: &Inner) {
    let mut st = inner.st.lock().unwrap();
    let st = &mut *st;
    let mut changed = false;
    for s in st.sessions.values_mut().filter(|s| s.unsaved) {
        s.unsaved = false;
        let cwd = (!s.cwd.is_empty()).then(|| s.cwd.clone());
        let thread = st.started.entry(s.id.clone()).or_insert_with(|| Started { key: s.key(), at: now_ms(), cwd, ..Default::default() });
        thread.flags = s.flags.clone();
        thread.hidden = s.hidden;
        thread.keep = s.kept;
        changed = true;
    }
    if changed {
        save_started(&inner.started_path, &st.started);
    }
}

/// The thread a call is about.
fn thread_of(inner: &Inner, params: &Value) -> Result<(String, Thread), RpcError> {
    let key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("INVALID_REQUEST", "sessionKey is required"))?;
    let thread = inner.st.lock().unwrap().thread(key).ok_or_else(|| RpcError::new("NOT_FOUND", "Not a Codex session."))?;
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
    let path = inner.st.lock().unwrap().sessions.get(&sid).filter(|s| !s.skip).and_then(|s| s.path.clone());
    let messages = match path {
        Some(path) => tauri::async_runtime::spawn_blocking(move || read_history(&path))
            .await
            .map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
            .map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't read the session's log: {e}")))?,
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

/// Stops a turn: the `codex exec` running it, or through the shared server. A session
/// hosted anywhere else can only be stopped where it runs.
async fn chat_abort(inner: &Arc<Inner>, params: &Value) -> Result<Value, RpcError> {
    let (_, thread) = thread_of(inner, params)?;
    let idle = || json!({ "ok": true, "aborted": false, "runIds": [] });
    let Thread::Session(sid) = thread else { return Ok(idle()) };
    let (stop, turn, run_id, busy) = {
        let mut st = inner.st.lock().unwrap();
        let s = st.sessions.get_mut(&sid).ok_or_else(|| RpcError::new("NOT_FOUND", "littleguys lost track of this session."))?;
        (s.exec.take(), s.turn_id.clone(), s.run_id(), s.busy)
    };
    if !busy {
        return Ok(idle());
    }
    if let Some(stop) = stop {
        let _ = stop.send(());
        return Ok(json!({ "ok": true, "aborted": true, "runIds": [run_id] }));
    }
    let in_terminal = || RpcError::new("UNSUPPORTED", "This session runs in a terminal: press Esc there to stop it.");
    let (srv, turn) = server(inner).zip(turn).ok_or_else(in_terminal)?;
    srv.call("turn/interrupt", json!({ "threadId": sid, "turnId": turn })).await.map_err(|_| in_terminal())?;
    // Its turn ends in the rollout, a moment later.
    Ok(json!({ "ok": true, "aborted": true, "runIds": [run_id] }))
}

/// Every session Codex kept a rollout of, newest first (the newest `PAST_MAX`).
async fn sessions_past(inner: &Arc<Inner>) -> Result<Value, RpcError> {
    let sessions = inner.home.join("sessions");
    let (names, only) = (inner.st.lock().unwrap().names.clone(), inner.only.clone());
    let found = blocking(move || Ok(past_sessions(&sessions, &names, only.as_deref(), PAST_MAX))).await?;
    let st = inner.st.lock().unwrap();
    let now = now_ms();
    let rows: Vec<Value> = found
        .into_iter()
        .filter(|p| !st.sessions.get(&p.session_id).is_some_and(|s| s.hidden))
        .map(|p| {
            let known = st.sessions.get(&p.session_id);
            let key = known.map_or_else(|| keys::thread(AGENT_ID, &p.session_id), Session::key);
            json!({
                "sessionId": p.session_id,
                "cwd": p.cwd,
                "title": known.and_then(Session::title).or(p.title),
                "updatedAt": p.updated_at,
                "key": key,
                "inThreads": known.is_some_and(|s| s.listable(now) && s.shown()),
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
        s.hidden = false;
        let updated = params.get("updatedAt").and_then(Value::as_u64).unwrap_or_default();
        s.updated_at = s.updated_at.max(updated);
        let (key, cwd) = (s.key(), (!s.cwd.is_empty()).then(|| s.cwd.clone()));
        let at = now_ms();
        let thread = match st.started.get(&sid).cloned() {
            Some(t) => Started { keep: true, hidden: false, ..t },
            None => Started { key: key.clone(), at, cwd, keep: true, ..Default::default() },
        };
        remember(&inner.started_path, &mut st.started, &sid, thread);
        key
    };
    inner.wake.notify_one();
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

/// Rollouts under `sessions/`, newest first, leaving out subagents' and any that never got
/// a prompt. Its name if it has one, else its first prompt, which takes reading further in.
fn past_sessions(sessions: &Path, names: &HashMap<String, String>, only: Option<&str>, max: usize) -> Vec<PastSession> {
    let mut files = walk(sessions);
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files
        .into_iter()
        .filter_map(|f| {
            let name = names.get(&f.id);
            let head = read_head(&f.path, if name.is_some() { PAST_HEAD_BYTES } else { HEAD_BYTES }).ok()?;
            let meta = head.iter().find_map(session_meta)?;
            if meta.subagent || only.is_some_and(|only| !inside(&meta.cwd, only)) {
                return None;
            }
            let title = name.cloned().or_else(|| head.iter().find_map(user_text).map(|p| truncate(first_line(&p), 80)))?;
            Some(PastSession { session_id: f.id, cwd: meta.cwd, title: Some(title), updated_at: f.modified })
        })
        .take(max)
        .collect()
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

// ---- the codex CLI ------------------------------------------------------------------------

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()).map_or_else(|| home().join(".codex"), PathBuf::from)
}

/// The `codex` CLI: on the login shell's PATH, or where its installer puts it.
pub fn find_codex() -> Option<PathBuf> {
    let dirs = crate::platform::path_dirs().into_iter().chain([home().join(".local").join("bin"), codex_home().join("packages").join("standalone").join("current").join("bin")]);
    crate::platform::find_in(dirs, "codex")
}

/// Runs a command; its stdout, or the first line it complained with.
async fn output(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(timeout, cmd.output()).await.map_err(|_| format!("timed out after {}s", timeout.as_secs()))?.map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr.lines().chain(stdout.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed").to_string())
}

async fn run_cli(codex: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = cli::command(codex);
    cmd.args(args);
    output(cmd, timeout).await
}

/// `codex-cli 0.166.0` → `0.166.0`.
async fn codex_version(codex: &Path) -> Option<String> {
    let out = run_cli(codex, &["--version"], CLI_TIMEOUT).await.ok()?;
    out.split_whitespace().last().map(str::to_string)
}

/// Codex on this machine, for Add a bot and the first-run screen:
/// `{installed, version, sessions: whether it has run here}`.
pub async fn detect() -> Value {
    let Some(codex) = find_codex() else {
        return json!({ "installed": false, "version": Value::Null, "sessions": false });
    };
    json!({ "installed": true, "version": codex_version(&codex).await, "sessions": codex_home().join("sessions").is_dir() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01a1a900-0000-7000-8000-00000000abcd";

    fn event(kind: &str, rest: Value) -> Value {
        let mut payload = json!({ "type": kind });
        payload.as_object_mut().unwrap().extend(rest.as_object().unwrap().clone());
        json!({ "timestamp": "2026-11-01T10:00:00.000Z", "type": "event_msg", "payload": payload })
    }

    fn item(item: Value) -> Value {
        event("item_completed", json!({ "item": item, "thread_id": ID, "turn_id": "t1" }))
    }

    fn response(kind: &str, rest: Value) -> Value {
        let mut line = event(kind, rest);
        line["type"] = json!("response_item");
        line
    }

    fn user(id: &str, text: &str) -> Value {
        item(json!({ "type": "UserMessage", "id": id, "content": [{ "type": "text", "text": text, "text_elements": [] }] }))
    }

    fn assistant(id: &str, text: &str) -> Value {
        response("message", json!({ "id": id, "role": "assistant", "content": [{ "type": "output_text", "text": text }], "phase": "final_answer" }))
    }

    fn meta(cwd: &str, source: Value) -> Value {
        json!({
            "timestamp": "2026-11-01T10:00:00.000Z",
            "type": "session_meta",
            "payload": { "id": ID, "timestamp": "2026-11-01T09:59:59.000Z", "cwd": cwd, "originator": "codex", "source": source },
        })
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lg-codex-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn jsonl(lines: &[Value]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn convert(line: &Value) -> Vec<Value> {
        convert_line(line, &mut HashMap::new())
    }

    #[test]
    fn rollout_names_give_the_session_id() {
        assert_eq!(rollout_id(&format!("rollout-2026-11-01T10-00-00-{ID}.jsonl")), Some(ID));
        assert_eq!(rollout_id(&format!("rollout-2026-11-01T10-00-00-{ID}.jsonl.zst")), Some(ID));
        assert_eq!(rollout_id("rollout-short.jsonl"), None);
        assert_eq!(rollout_id(&format!("{ID}.jsonl")), None);
        assert_eq!(rollout_id(&format!("rollout-2026-11-01T10-00-00-{ID}.txt")), None);
    }

    #[test]
    fn walks_year_month_day_folders() {
        let root = temp("walk");
        let day = root.join("2026/10/02");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-10-02T10-00-00-{ID}.jsonl"));
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::write(day.join("notes.txt"), "not a rollout").unwrap();
        let found = walk(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, ID);
        assert_eq!(found[0].path, path);
        assert!(found[0].modified > 0);
    }

    #[test]
    fn subagents_are_left_out() {
        assert!(!session_meta(&meta("/home/pat/app", json!("cli"))).unwrap().subagent);
        assert!(session_meta(&meta("/home/pat/app", json!({ "subagent": { "review": {} } }))).unwrap().subagent);
        let mut child = meta("/home/pat/app", json!("exec"));
        child["payload"]["parent_thread_id"] = json!("01a1a900-1111-7000-8000-000000000000");
        assert!(session_meta(&child).unwrap().subagent);
        assert_eq!(session_meta(&user("u1", "hi")), None);

        let mut s = Session::new(ID);
        note_start(&mut s, &meta("/home/pat/app", json!({ "subagent": "review" })), None);
        assert!(s.skip);
        assert_eq!(s.cwd, "/home/pat/app");
    }

    #[test]
    fn only_limits_to_one_folder() {
        assert!(inside("/tmp/a", "/tmp/a"));
        assert!(inside("/tmp/a/b", "/tmp/a/"));
        assert!(!inside("/tmp/ab", "/tmp/a"));
        let mut s = Session::new(ID);
        note_start(&mut s, &meta("/home/pat/app", json!("cli")), Some("/tmp/test"));
        assert!(s.skip);
        note_start(&mut s, &meta("/tmp/test/x", json!("cli")), Some("/tmp/test"));
        assert!(!s.skip);
    }

    #[test]
    fn prompts_come_from_codexs_record_not_the_injected_context() {
        let injected = response("message", json!({ "id": "m1", "role": "user", "content": [{ "type": "input_text", "text": "<environment_context>…" }] }));
        assert_eq!(line_meta(&injected), LineMeta::default());
        assert!(convert(&injected).is_empty());
        let developer = response("message", json!({ "id": "m2", "role": "developer", "content": [{ "type": "input_text", "text": "rules" }] }));
        assert!(convert(&developer).is_empty());

        assert_eq!(line_meta(&user("u1", "fix the tests")).prompt.as_deref(), Some("fix the tests"));
        let m = convert(&user("u1", "fix the tests"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[0]["content"][0]["text"], "fix the tests");
        assert_eq!(m[0]["__openclaw"]["id"], "u1");
        assert!(m[0]["timestamp"].is_u64());
    }

    #[test]
    fn older_rollouts_say_it_in_events() {
        let line = event("user_message", json!({ "message": "  hello  ", "images": [] }));
        assert_eq!(user_text(&line).as_deref(), Some("hello"));
        assert_eq!(convert(&line)[0]["role"], "user");
    }

    #[test]
    fn an_image_only_message_still_shows() {
        let line = item(json!({ "type": "UserMessage", "id": "u9", "content": [{ "type": "local_image", "path": "/tmp/a.png" }] }));
        assert_eq!(convert(&line)[0]["content"][0]["text"], "(attachment)");
    }

    #[test]
    fn assistant_text_comes_once() {
        let answer = assistant("msg_1", "Done.");
        let m = convert(&answer);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["__openclaw"]["id"], "msg_1");
        assert_eq!(line_meta(&answer).said.as_deref(), Some("Done."));
        // The same message as an item: the response line above is the one that counts.
        assert!(convert(&item(json!({ "type": "AgentMessage", "id": "msg_1", "content": [{ "type": "Text", "text": "Done." }] }))).is_empty());
    }

    #[test]
    fn turns_open_and_close() {
        let started = event("task_started", json!({ "turn_id": "t1" }));
        assert_eq!(line_meta(&started).turn_started.as_deref(), Some("t1"));
        assert!(line_meta(&event("task_complete", json!({ "turn_id": "t1" }))).turn_ended);
        let aborted = line_meta(&event("turn_aborted", json!({ "turn_id": "t1", "reason": "interrupted" })));
        assert!(aborted.turn_ended && aborted.interrupted);

        let mut s = Session::new(ID);
        let mut emits = Vec::new();
        assert!(note_line(&mut s, &started, None, true, &mut emits));
        assert!(s.busy);
        assert_eq!(s.turn_id.as_deref(), Some("t1"));
        assert!(note_line(&mut s, &event("turn_aborted", json!({})), None, true, &mut emits));
        assert!(!s.busy && s.interrupted && s.flags.unread);
        assert_eq!(s.turn_id, None);
        let states: Vec<&str> = emits.iter().filter(|(n, _)| *n == "chat").filter_map(|(_, p)| p["state"].as_str()).collect();
        assert_eq!(states, ["status", "final"]);
        // A new turn clears it.
        note_line(&mut s, &started, None, true, &mut Vec::new());
        assert!(!s.interrupted);
    }

    #[test]
    fn a_first_look_only_remembers() {
        let mut s = Session::new(ID);
        let mut emits = Vec::new();
        note_line(&mut s, &event("task_started", json!({ "turn_id": "t1" })), None, false, &mut emits);
        assert!(s.open_turn && !s.busy && emits.is_empty());
    }

    #[test]
    fn commands_show_what_was_run() {
        let line = item(json!({
            "type": "CommandExecution", "id": "exec-1", "command": ["/bin/zsh", "-lc", "cargo test"], "cwd": "file:///home/pat/app",
            "status": "completed", "aggregated_output": "ok", "exit_code": 0,
        }));
        let m = convert(&line);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "assistant");
        assert_eq!(m[0]["content"][0], json!({ "type": "toolCall", "id": "exec-1", "name": "exec", "arguments": { "command": "cargo test" } }));
        assert_eq!(m[1]["role"], "toolResult");
        assert_eq!(m[1]["toolCallId"], "exec-1");
        assert_eq!(m[1]["toolName"], "exec");
        assert_eq!(m[1]["isError"], false);
        assert_eq!(m[1]["content"][0]["text"], "ok");
        assert_ne!(m[0]["__openclaw"]["id"], m[1]["__openclaw"]["id"]);

        let bad = item(json!({ "type": "CommandExecution", "id": "exec-2", "command": ["ls", "/nope"], "formatted_output": "no such file", "exit_code": 1 }));
        let m = convert(&bad);
        assert_eq!(m[0]["content"][0]["arguments"]["command"], "ls /nope");
        assert_eq!(m[1]["isError"], true);
        assert_eq!(m[1]["content"][0]["text"], "no such file");
    }

    #[test]
    fn the_script_wrapper_around_tools_stays_hidden() {
        let wrapper = response("custom_tool_call", json!({ "id": "c1", "call_id": "call_1", "name": "exec", "input": "const r = await tools.exec_command({cmd:\"ls\"})" }));
        let output = response("custom_tool_call_output", json!({ "id": "c2", "call_id": "call_1", "output": "ran" }));
        let mut calls = HashMap::new();
        assert!(convert_line(&wrapper, &mut calls).is_empty());
        assert!(convert_line(&output, &mut calls).is_empty());
        let polling = response("function_call", json!({ "call_id": "call_2", "name": "wait", "arguments": "{}" }));
        assert!(convert_line(&polling, &mut calls).is_empty());
    }

    #[test]
    fn older_tool_calls_pair_up() {
        let mut calls = HashMap::new();
        let call = response("function_call", json!({ "call_id": "call_3", "name": "shell", "arguments": "{\"command\":[\"ls\"]}" }));
        let m = convert_line(&call, &mut calls);
        assert_eq!(m[0]["content"][0]["name"], "shell");
        assert_eq!(m[0]["content"][0]["arguments"]["command"][0], "ls");
        let out = response("function_call_output", json!({ "call_id": "call_3", "output": { "content": "a.txt" } }));
        let m = convert_line(&out, &mut calls);
        assert_eq!(m[0]["toolName"], "shell");
        assert_eq!(m[0]["content"][0]["text"], "a.txt");
    }

    #[test]
    fn other_tools_become_chips() {
        let mcp = item(json!({ "type": "McpToolCall", "id": "m1", "server": "notes", "tool": "search", "arguments": { "text": "x" }, "result": { "content": [{ "type": "text", "text": "2 hits" }], "isError": false }, "status": "completed" }));
        let m = convert(&mcp);
        assert_eq!(m[0]["content"][0]["name"], "search");
        assert_eq!(m[1]["content"][0]["text"], "2 hits");
        assert_eq!(m[1]["isError"], false);
        let broken = item(json!({ "type": "McpToolCall", "id": "m2", "tool": "search", "error": { "message": "timed out" }, "status": "failed" }));
        let m = convert(&broken);
        assert_eq!(m[1]["isError"], true);
        assert_eq!(m[1]["content"][0]["text"], "timed out");

        let edit = item(json!({ "type": "FileChange", "id": "f1", "changes": { "/home/pat/app/a.rs": { "type": "update" } }, "status": "completed" }));
        assert_eq!(convert(&edit)[0]["content"][0]["arguments"]["path"], "/home/pat/app/a.rs");

        let search = item(json!({ "type": "Extension", "id": "w1", "kind": "web_search", "action": { "type": "search", "query": "rust" }, "results": [{ "title": "Rust", "url": "https://example.com" }] }));
        let m = convert(&search);
        assert_eq!(m[0]["content"][0]["arguments"]["query"], "rust");
        assert_eq!(m[1]["content"][0]["text"], "Rust: https://example.com");

        assert_eq!(convert(&item(json!({ "type": "ImageView", "id": "i1", "path": "/tmp/a.png" })))[0]["content"][0]["arguments"]["path"], "/tmp/a.png");
        assert!(convert(&item(json!({ "type": "Reasoning", "id": "r1", "summary_text": [] }))).is_empty());
        assert!(convert(&item(json!({ "type": "ContextCompaction", "id": "c1" }))).is_empty());
    }

    #[test]
    fn long_output_is_cut_on_a_character_boundary() {
        let text = "é".repeat(TOOL_TEXT_MAX);
        let out = cut(&text);
        assert!(out.len() < text.len());
        assert!(out.contains("more characters"));
    }

    #[test]
    fn reads_a_rollout_whole_then_only_what_is_new() {
        let dir = temp("read");
        let path = dir.join(format!("rollout-2026-11-01T10-00-00-{ID}.jsonl"));
        let first = jsonl(&[meta("/home/pat/app", json!("cli")), event("task_started", json!({ "turn_id": "t1" })), user("u1", "fix it")]);
        std::fs::write(&path, &first).unwrap();

        let Read::First { head, tail, lines, .. } = read_rollout(&path, None) else { panic!("expected a first look") };
        assert_eq!(head.len(), 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(tail.offset, first.len() as u64);
        assert!(matches!(read_rollout(&path, Some(tail.clone())), Read::Same));

        let mut more = first.clone();
        more.push_str(&jsonl(&[assistant("a1", "Fixed.")]));
        more.push_str("{\"type\":\"event_msg\",\"pay"); // still being written
        std::fs::write(&path, &more).unwrap();
        let Read::New { tail, lines, .. } = read_rollout(&path, Some(tail)) else { panic!("expected new lines") };
        assert_eq!(lines.len(), 1);
        assert!(!tail.partial.is_empty());

        more.push_str("load\":{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}\n");
        std::fs::write(&path, &more).unwrap();
        let Read::New { lines, .. } = read_rollout(&path, Some(tail)) else { panic!("expected the rest of the line") };
        assert!(line_meta(&lines[0]).turn_ended);

        let history = read_history(&path).unwrap();
        let roles: Vec<&str> = history.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, ["user", "assistant"]);
    }

    #[test]
    fn a_rollout_that_is_not_there_yet_is_gone_not_an_error() {
        assert!(matches!(read_rollout(Path::new("/nowhere/rollout.jsonl"), Some(Tail::default())), Read::Gone));
    }

    /// The smallest valid zstd frame: one raw block.
    fn zst(data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, data.len() as u8];
        let header = ((data.len() as u32) << 3) | 1; // last block, raw
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(data);
        frame
    }

    #[test]
    fn compressed_rollouts_read_the_same() {
        let dir = temp("zst");
        let path = dir.join(format!("rollout-2026-11-01T10-00-00-{ID}.jsonl.zst"));
        let lines = jsonl(&[json!({ "type": "session_meta", "payload": { "id": "a", "cwd": "/w" } }), assistant("a1", "Old.")]);
        assert!(lines.len() < 256);
        std::fs::write(&path, zst(lines.as_bytes())).unwrap();
        assert_eq!(read_head(&path, 4096).unwrap().len(), 2);
        let Read::First { head, tail, lines, .. } = read_rollout(&path, None) else { panic!("expected a first look") };
        assert_eq!((head.len(), lines.len()), (2, 2));
        assert!(matches!(read_rollout(&path, Some(tail)), Read::Same));
        assert_eq!(read_history(&path).unwrap()[0]["role"], "assistant");
    }

    #[test]
    fn names_are_read_as_they_grow_and_the_last_one_wins() {
        let dir = temp("names");
        let path = dir.join("session_index.jsonl");
        std::fs::write(&path, jsonl(&[json!({ "id": ID, "thread_name": "First", "updated_at": "x" })])).unwrap();
        let (names, read) = read_names(&path, 0);
        assert_eq!(names, [(ID.to_string(), "First".to_string())]);
        assert_eq!(read_names(&path, read).0.len(), 0);
        let mut grown = std::fs::read_to_string(&path).unwrap();
        grown.push_str(&jsonl(&[json!({ "id": ID, "thread_name": "Second" }), json!({ "id": "b", "thread_name": "  " })]));
        grown.push_str("{\"id\":\"c\""); // still being written
        std::fs::write(&path, &grown).unwrap();
        let (names, read2) = read_names(&path, read);
        assert_eq!(names, [(ID.to_string(), "Second".to_string())]);
        assert_eq!(read2 as usize, grown.len() - "{\"id\":\"c\"".len());
        assert_eq!(read_names(Path::new("/nowhere/index"), 9), (Vec::new(), 0));
    }

    #[test]
    fn a_lock_counts_while_it_is_held() {
        let home = temp("locks");
        assert_eq!(live_ids(&home), None);
        let locks = home.join("thread-writer-locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join(format!("{ID}.lock"));
        std::fs::write(&path, "").unwrap();
        std::fs::write(locks.join("readme.txt"), "").unwrap();
        // Left from a crash.
        assert_eq!(live_ids(&home), Some(HashSet::new()));
        let owner = std::fs::File::open(&path).unwrap();
        owner.lock().unwrap();
        assert!(live_ids(&home).unwrap().contains(ID));
        owner.unlock().unwrap();
        assert_eq!(live_ids(&home), Some(HashSet::new()));
    }

    fn found(id: &str, modified: u64) -> Found {
        Found { id: id.to_string(), path: PathBuf::from(format!("/codex/{id}.jsonl")), modified }
    }

    #[test]
    fn scans_add_recent_or_live_sessions() {
        let now = 10 * LIST_WINDOW_MS;
        let mut st = State::default();
        let live: HashSet<String> = ["up".to_string()].into();
        apply_scan(&mut st, Some(vec![found("recent", now - 1000), found("old", 1), found("up", 2)]), Some(&live), now);
        assert!(st.sessions.contains_key("recent"));
        assert!(!st.sessions.contains_key("old"));
        assert!(st.sessions["up"].live);
        assert!(!st.sessions["recent"].live);
        assert_eq!(st.sessions["recent"].path, Some(PathBuf::from("/codex/recent.jsonl")));

        // Archived or deleted in Codex.
        apply_scan(&mut st, Some(vec![found("up", 2)]), Some(&live), now);
        assert_eq!(st.sessions["recent"].path, None);
        st.sessions.get_mut("recent").unwrap().tail = Some(Tail::default());
        st.sessions.get_mut("recent").unwrap().first_prompt = Some("hi".into());
        assert!(!st.sessions["recent"].shown());
    }

    #[test]
    fn a_session_that_goes_away_mid_turn_stops_being_busy() {
        let mut st = State::default();
        let live: HashSet<String> = [ID.to_string()].into();
        apply_scan(&mut st, Some(vec![found(ID, 5)]), Some(&live), 10);
        let s = st.sessions.get_mut(ID).unwrap();
        s.set_busy(true, &mut Vec::new());
        let emits = apply_scan(&mut st, None, Some(&HashSet::new()), 20);
        assert!(emits.iter().any(|(n, p)| *n == "sessions.changed" && p["reason"] == "codex"));
        // Its last lines get read first.
        let s = st.sessions.get_mut(ID).unwrap();
        assert!(s.busy);
        let mut emits = Vec::new();
        settle_turn(s, &mut emits);
        assert!(!s.busy);
        assert!(emits.iter().any(|(n, p)| *n == "chat" && p["state"] == "final"));
    }

    #[test]
    fn a_handed_over_message_keeps_it_busy_until_its_turn_shows() {
        let mut s = Session::new(ID);
        s.set_busy(true, &mut Vec::new());
        s.assumed = Some(Instant::now());
        settle_turn(&mut s, &mut Vec::new());
        assert!(s.busy);
        // It never came: it waits in the queue. Nothing new to read, though.
        s.assumed = Instant::now().checked_sub(TURN_SHOWS_UP * 2);
        settle_turn(&mut s, &mut Vec::new());
        assert!(!s.busy && !s.flags.unread);
    }

    #[test]
    fn codex_without_locks_goes_by_recent_writes() {
        let mut st = State::default();
        apply_scan(&mut st, Some(vec![found(ID, 1000)]), None, 2000);
        assert!(st.sessions[ID].live);
        apply_scan(&mut st, None, None, 2000 + UNLOCKED_LIVE_MS);
        assert!(!st.sessions[ID].live);
    }

    #[test]
    fn ended_sessions_go_to_the_archive_unless_kept() {
        let mut s = Session::new(ID);
        s.live = true;
        assert!(!s.archived());
        s.live = false;
        assert!(s.archived());
        s.kept = true;
        assert!(!s.archived());
        s.kept = false;
        s.flags.archived = Some(false);
        assert!(!s.archived());
        s.flags.archived = Some(true);
        s.live = true;
        assert!(s.archived());
        // At work again, it's back.
        s.set_busy(true, &mut Vec::new());
        assert_eq!(s.flags.archived, None);
        assert!(!s.archived());
        // A routine's run stays with its routine.
        let mut run = Session::new(ID);
        run.key = Some(keys::routine_run(AGENT_ID, "job", "run"));
        assert!(!run.archived());
    }

    #[test]
    fn listing_needs_something_said() {
        let now = LIST_WINDOW_MS * 2;
        let mut s = Session::new(ID);
        s.updated_at = now - 1000;
        s.path = Some(PathBuf::from("/codex/a.jsonl"));
        assert!(s.listable(now));
        assert!(!s.shown());
        s.tail = Some(Tail::default());
        assert!(!s.shown());
        s.first_prompt = Some("hi".into());
        assert!(s.shown());
        s.skip = true;
        assert!(!s.listable(now));
        s.skip = false;
        s.updated_at = 1;
        assert!(!s.listable(now));
        s.kept = true;
        assert!(s.listable(now));
        s.hidden = true;
        assert!(!s.listable(now));
    }

    #[test]
    fn titles_prefer_the_name_then_the_first_prompt_then_the_folder() {
        let mut s = Session::new(ID);
        s.cwd = "/home/pat/app".into();
        assert_eq!(s.title().as_deref(), Some("app"));
        s.first_prompt = Some("\n  Fix the login page\nand the tests".into());
        assert_eq!(s.title().as_deref(), Some("Fix the login page"));
        s.name = Some("Login".into());
        assert_eq!(s.title().as_deref(), Some("Login"));
    }

    #[test]
    fn previews_say_when_it_waits_for_you() {
        let mut s = Session::new(ID);
        assert_eq!(s.preview(None), None);
        s.said = Some("All done.".into());
        assert_eq!(s.preview(None).as_deref(), Some("All done."));
        assert_eq!(s.preview(Some(&WaitingOn::Input)).as_deref(), Some("Waiting for you in Codex"));
    }

    #[test]
    fn routes_pick_the_way_in() {
        // busy, live, server, can_queue
        assert_eq!(route(true, true, true, true), Ok(Route::Queue));
        assert_eq!(route(true, true, false, true), Ok(Route::Queue));
        assert_eq!(route(true, true, true, false), Ok(Route::Turn));
        assert_eq!(route(true, true, false, false), Err(TOO_OLD));
        assert_eq!(route(false, true, true, true), Ok(Route::Turn));
        assert_eq!(route(false, false, true, true), Ok(Route::Turn));
        assert_eq!(route(false, true, false, true), Ok(Route::Queue));
        assert_eq!(route(false, true, false, false), Err(TOO_OLD));
        assert_eq!(route(false, false, false, true), Ok(Route::Exec));
        assert_eq!(route(false, false, false, false), Ok(Route::Exec));
    }

    #[test]
    fn exec_output_gives_the_thread_or_why_not() {
        assert_eq!(exec_event(&format!("{{\"type\":\"thread.started\",\"thread_id\":\"{ID}\"}}")), ExecEvent::Thread(ID.to_string()));
        assert_eq!(exec_event(r#"{"type":"turn.failed","error":{"message":"out of credits"}}"#), ExecEvent::Failed("out of credits".into()));
        assert_eq!(exec_event(r#"{"type":"error","message":"no model"}"#), ExecEvent::Failed("no model".into()));
        assert_eq!(exec_event(r#"{"type":"turn.started"}"#), ExecEvent::Other);
        assert_eq!(exec_event("not json"), ExecEvent::Other);
    }

    #[test]
    fn prompts_never_read_as_flags() {
        assert_eq!(prompt_arg("--help"), " --help");
        assert_eq!(prompt_arg("hello"), "hello");
    }

    #[test]
    fn server_status_says_what_it_waits_for() {
        assert_eq!(waiting_on(&json!({ "type": "active", "activeFlags": ["waitingOnApproval"] })), Some(WaitingOn::Approval));
        assert_eq!(waiting_on(&json!({ "type": "active", "activeFlags": ["waitingOnUserInput"] })), Some(WaitingOn::Input));
        assert_eq!(waiting_on(&json!({ "type": "active", "activeFlags": [] })), None);
        assert_eq!(waiting_on(&json!({ "type": "idle" })), None);
    }

    #[test]
    fn approval_cards_say_what_it_wants_to_do() {
        let key = keys::thread(AGENT_ID, ID);
        let run = json!({ "threadId": ID, "itemId": "i1", "command": "rm -rf build", "cwd": "/home/pat/app", "reason": "cleaning up" });
        let p = approval_payload("codex-i1", &key, "/elsewhere", "Run a command", "exec", &run);
        assert_eq!(p["presentation"]["commandText"], "rm -rf build");
        assert_eq!(p["presentation"]["warningText"], "cleaning up");
        assert_eq!(p["presentation"]["allowedDecisions"], json!(["allow-once", "allow-always", "deny"]));
        assert_eq!(p["request"]["cwd"], "/home/pat/app");
        assert_eq!(p["sessionKey"], json!(key));

        let files = json!({ "threadId": ID, "itemId": "i2", "reason": "needs to write outside the folder" });
        let p = approval_payload("codex-i2", &key, "/home/pat/app", "Change files", "apply_patch", &files);
        assert_eq!(p["presentation"]["commandText"], "needs to write outside the folder");
        assert!(p["presentation"].get("warningText").is_none());
        assert_eq!(p["request"]["cwd"], "/home/pat/app");
        assert_eq!(approval_payload("codex-i3", &key, "/w", "Change files", "apply_patch", &json!({}))["presentation"]["commandText"], "Change files");
    }

    #[test]
    fn shell_wrappers_come_off_commands() {
        assert_eq!(unwrap_shell("/bin/zsh -lc 'touch a.txt'"), "touch a.txt");
        assert_eq!(unwrap_shell("bash -c 'ls -la'"), "ls -la");
        assert_eq!(unwrap_shell("/bin/zsh -lc 'echo 'hi''"), "/bin/zsh -lc 'echo 'hi''");
        assert_eq!(unwrap_shell("python -c 'print(1)'"), "python -c 'print(1)'");
        assert_eq!(unwrap_shell("ls"), "ls");
    }

    #[test]
    fn past_sessions_leave_out_subagents_and_empty_ones() {
        let root = temp("past");
        let day = root.join("2026/11/01");
        std::fs::create_dir_all(&day).unwrap();
        let write = |n: u8, lines: &[Value]| {
            let id = format!("01a1a900-0000-7000-8000-0000000000{n:02}");
            std::fs::write(day.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl")), jsonl(lines)).unwrap();
            id
        };
        let named = write(1, &[meta("/home/pat/app", json!("cli"))]);
        let prompted = write(2, &[meta("/home/pat/api", json!("exec")), user("u1", "add a route\nand tests")]);
        write(3, &[meta("/home/pat/app", json!({ "subagent": "review" })), user("u2", "review this")]);
        write(4, &[meta("/home/pat/app", json!("cli"))]);
        let names: HashMap<String, String> = [(named.clone(), "Named one".to_string())].into();
        let mut past = past_sessions(&root, &names, None, 10);
        past.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(past.len(), 2);
        assert_eq!((past[0].session_id.as_str(), past[0].title.as_deref()), (named.as_str(), Some("Named one")));
        assert_eq!((past[1].session_id.as_str(), past[1].title.as_deref()), (prompted.as_str(), Some("add a route")));
        assert_eq!(past[1].cwd, "/home/pat/api");
        assert_eq!(past_sessions(&root, &names, Some("/home/pat/app"), 10).len(), 1);
        assert_eq!(past_sessions(&root, &names, None, 1).len(), 1);
    }

    #[test]
    fn thread_keys_lead_to_sessions_or_drafts() {
        let mut st = State::default();
        let key = keys::thread(AGENT_ID, ID);
        assert!(matches!(st.thread(&key), Some(Thread::Session(sid)) if sid == ID));
        assert!(matches!(st.thread("agent:codex:main"), Some(Thread::Draft)));
        assert!(st.thread("agent:claude:dashboard:x").is_none());
        st.started.insert("s1".into(), Started { key: "agent:codex:dashboard:mine".into(), ..Default::default() });
        assert!(matches!(st.thread("agent:codex:dashboard:mine"), Some(Thread::Session(sid)) if sid == "s1"));
        st.drafts.insert("agent:codex:dashboard:new".into(), Draft { cwd: String::new(), created_at: 0, launching: false, outbox: Vec::new() });
        assert!(matches!(st.thread("agent:codex:dashboard:new"), Some(Thread::Draft)));
    }

    #[test]
    fn what_you_kept_comes_back() {
        let dir = temp("kept");
        let path = dir.join("codex-threads-x.json");
        let mut started = HashMap::new();
        let flags = ThreadFlags { label: Some("Mine".into()), pinned: true, ..Default::default() };
        remember(&path, &mut started, ID, Started { key: "agent:codex:dashboard:a".into(), at: 1, keep: true, flags, ..Default::default() });
        remember(&path, &mut started, "old", Started { key: "agent:codex:dashboard:b".into(), at: 1, ..Default::default() });
        let loaded = load_started(&path);
        assert!(loaded[ID].keep);
        assert_eq!(loaded[ID].flags.label.as_deref(), Some("Mine"));
        // Not pinned, and older than `STARTED_KEEP_MS`.
        assert!(!loaded.contains_key("old"));

        let mut st = State { started: loaded, ..Default::default() };
        let s = st.session(ID);
        assert!(s.kept);
        assert_eq!(s.key.as_deref(), Some("agent:codex:dashboard:a"));
    }

    #[test]
    fn server_answers_settle_their_calls() {
        let (out, _rx) = mpsc::unbounded_channel();
        let srv = Server { out, calls: Mutex::new(HashMap::new()), next: AtomicU64::new(1) };
        let (tx, mut rx) = oneshot::channel();
        srv.calls.lock().unwrap().insert(7, tx);
        srv.settle(&json!({ "id": 7, "error": { "code": -32600, "message": "already has an active writer" } }));
        assert_eq!(rx.try_recv().unwrap(), Err("already has an active writer".to_string()));
        let (tx, mut rx) = oneshot::channel();
        srv.calls.lock().unwrap().insert(8, tx);
        srv.settle(&json!({ "id": 8, "result": { "ok": true } }));
        assert_eq!(rx.try_recv().unwrap(), Ok(json!({ "ok": true })));
        srv.settle(&json!({ "id": 9, "result": {} }));
    }
}
