//! The message index behind `sessions.search` for runtimes whose transcripts are files on
//! this machine (Claude Code, Codex, ACP): `<data_dir>/search-<gatewayId>.db`, SQLite FTS5.
//!
//! - It keeps no message text. The full-text table is contentless, and each message row says
//!   which transcript line it came from; a hit's snippet is read back from that line. (Except
//!   for compressed transcripts, which would have to be unpacked up to that line: their
//!   messages' text is kept, which is little, since it's their tool output that's big.)
//! - Transcripts only grow, so each one is read from where the last update stopped. One that
//!   shrank or was rewritten (the bytes just before that point changed) is read again from
//!   the start; one that's gone takes its messages with it; a compressed one is read whole,
//!   once.
//! - The first build reads everything, in the background, in a process of its own: reading
//!   GBs of transcripts leaves the allocator holding tens of MB, which the app would keep.
//!   After that, a search first catches up on what's new, here.
//! - Every search word has to match, as a word or the start of one ("book" finds "booked",
//!   not "ebook"). The best match per thread, by relevance (bm25) and how recent it is.
//! - SQLite holds at most a few MB of it in memory (`CACHE_KB` per connection).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;
use crate::runtime::truncate;

/// Bump to rebuild every index (it's only a cache of the transcripts).
const SCHEMA: i64 = 1;
/// SQLite's page cache, per connection, in KB.
const CACHE_KB: i64 = 2000;
/// How much of one message is indexed.
const TEXT_MAX: usize = 64 << 10;
/// Messages written per transaction while building.
const BATCH: usize = 2000;
/// Longer lines are skipped unread: they're tool output or images, never just words.
const LINE_MAX: usize = 4 << 20;
/// What's read from a transcript at a time.
const READ_BUFFER: usize = 64 << 10;
/// The bytes kept from just before where reading stopped, to tell a rewritten transcript.
const MARK_BYTES: u64 = 64;
/// How long a search waits for the catch-up before answering with what's indexed.
const CATCH_UP_WAIT: Duration = Duration::from_millis(400);
/// A search catches up at most this often.
const CATCH_UP_EVERY: Duration = Duration::from_secs(2);
/// The best matches looked at before keeping one per thread.
const CANDIDATES: i64 = 500;
/// The most hits one search returns.
pub const MAX_HITS: usize = 25;
/// Words in a snippet.
const SNIPPET_TOKENS: i64 = 28;
/// Recent messages rank higher: a month-old one counts this much less, at most.
const RECENCY_DAYS: f64 = 30.0;
/// The first build waits this long after launch, out of the way of everything else.
pub const BUILD_DELAY: Duration = Duration::from_secs(8);
/// What the app is started with to build an index in a process of its own:
/// `<flag> <index path> <runtime kind> <its argument>`.
const CHILD_FLAG: &str = "--littleguys-search-index";
/// Set by [`child_main`]: this executable answers [`CHILD_FLAG`]. Any other (an example
/// probe, a test binary) ignores the flag and runs whole again in the copy, starting copies
/// of its own: rooms_probe did, two per copy, until the Mac froze. Those build in place.
static ANSWERS_CHILD_FLAG: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    fn from_i64(n: i64) -> Self {
        if n == 0 {
            Role::User
        } else {
            Role::Assistant
        }
    }
}

/// One message worth searching: what you or the guy wrote (not tool calls or thinking).
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    /// Its id in `chat.history` (`__openclaw.id`); empty when it has none.
    pub id: String,
    pub role: Role,
    pub at: Option<u64>,
    pub text: String,
}

/// What one transcript line says.
#[derive(Debug, Default)]
pub struct Line {
    pub messages: Vec<Msg>,
    /// The thread's title, when the line gives one (it wins over the first message).
    pub title: Option<String>,
    /// The folder it works in.
    pub cwd: Option<String>,
    /// The whole transcript isn't a thread (a helper's): none of it is searched.
    pub skip: bool,
}

/// A runtime's transcripts: which files there are, and how to read a line of one.
pub trait Transcripts: Send + Sync + 'static {
    /// Every transcript, with the thread it belongs to (its id, as the adapter knows it).
    fn list(&self) -> Vec<(PathBuf, String)>;
    fn read(&self, line: &Value) -> Line;
    /// Whether a line, unparsed, may say anything [`read`](Self::read) wants: the rest
    /// (most of a transcript's bytes are tool output) are skipped without parsing them.
    fn wanted(&self, _raw: &[u8]) -> bool {
        true
    }
}

/// Whether `raw` has any of `needles` in it.
pub fn has_any(raw: &[u8], needles: &[&str]) -> bool {
    needles.iter().any(|n| memchr::memmem::find(raw, n.as_bytes()).is_some())
}

/// The messages worth searching among contract ChatMessages.
pub fn words_of(messages: &[Value]) -> Vec<Msg> {
    messages
        .iter()
        .filter_map(|m| {
            let role = match m.get("role").and_then(Value::as_str)? {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            let text = match m.get("content")? {
                Value::String(s) => s.trim().to_string(),
                Value::Array(blocks) => {
                    let texts: Vec<&str> = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect();
                    texts.join("\n").trim().to_string()
                }
                _ => return None,
            };
            if text.is_empty() {
                return None;
            }
            let id = m.pointer("/__openclaw/id").and_then(Value::as_str).unwrap_or_default().to_string();
            Some(Msg { id, role, at: m.get("timestamp").and_then(Value::as_u64), text })
        })
        .collect()
}

pub fn compressed(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

/// A message that matched, the best one in its thread.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub thread: String,
    pub message_id: String,
    pub role: Role,
    pub at: u64,
    /// Plain text around the match, with " … " where it was cut.
    pub snippet: String,
    /// 0–1, higher is better.
    pub score: f64,
    /// What the transcript calls the thread (or its first message), for threads the adapter
    /// doesn't know any more.
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Its last message.
    pub updated: u64,
}

impl Hit {
    /// The `sessions.search` result row: OpenClaw's, plus what a thread outside the list
    /// needs to be opened (`title`, `workspaceDir`, `updatedAt`).
    pub fn to_json(&self, key: &str, session_id: Option<&str>, title: Option<String>, cwd: Option<String>) -> Value {
        json!({
            "sessionKey": key,
            "sessionId": session_id.unwrap_or(&self.thread),
            "messageId": self.message_id,
            "role": self.role.as_str(),
            "timestamp": self.at,
            "snippet": self.snippet,
            "score": self.score,
            "title": title.or_else(|| self.title.clone()),
            "workspaceDir": cwd.or_else(|| self.cwd.clone()),
            "updatedAt": self.updated,
        })
    }
}

/// `sessions.search {query, limit?}` → (query, limit). (`sessionKeys` and `agentId` don't
/// matter here: every thread is searched.)
pub fn search_params(params: &Value) -> Result<(String, usize), RpcError> {
    let query = params.get("query").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if query.is_empty() {
        return Err(RpcError::new("INVALID_REQUEST", "query is required"));
    }
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, MAX_HITS as u64) as usize;
    Ok((query.to_string(), limit))
}

/// When the app was started to build an index ([`Index::build_apart`]): builds it, and
/// returns the exit code. None otherwise, and from then on indexes build in a copy of this
/// executable.
pub fn child_main() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(CHILD_FLAG)) {
        ANSWERS_CHILD_FLAG.store(true, Ordering::SeqCst);
        return None;
    }
    let (path, kind, arg) = (PathBuf::from(args.next()?), args.next()?, args.next().unwrap_or_default());
    let Some(transcripts) = make_transcripts(&kind.to_string_lossy(), PathBuf::from(arg)) else { return Some(2) };
    let index = Index::boxed(path, transcripts, None);
    Some(match index.update() {
        Ok(Update::Done) => 0,
        Ok(_) => 3,
        Err(e) => {
            eprintln!("search index {}: {e}", index.path.display());
            1
        }
    })
}

/// A runtime's transcripts, from its kind and argument (see [`Index::built_apart`]).
fn make_transcripts(kind: &str, arg: PathBuf) -> Option<Box<dyn Transcripts>> {
    match kind {
        "claudecode" => Some(super::claudecode::search_transcripts()),
        "codex" => Some(super::codex::search_transcripts(arg)),
        "acp" => Some(super::acp::search_transcripts(arg)),
        _ => None,
    }
}

/// Removes a gateway's index (it's removed from littleguys).
pub fn forget(data_dir: &Path, gateway_id: &str) {
    let path = index_path(data_dir, gateway_id);
    remove_db(&path);
}

pub fn index_path(data_dir: &Path, gateway_id: &str) -> PathBuf {
    data_dir.join(format!("search-{gateway_id}.db"))
}

fn remove_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
}

/// How an update went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Every transcript is read up to its end.
    Done,
    /// Another update is at it.
    Busy,
    Stopped,
}

pub struct Index {
    path: PathBuf,
    transcripts: Box<dyn Transcripts>,
    /// How a process of its own makes the same transcripts ([`child_main`]): the runtime's
    /// kind and its argument.
    recipe: Option<(&'static str, String)>,
    /// That process, while it builds.
    child: Mutex<Option<std::process::Child>>,
    /// Builds and catches up, one at a time.
    writer: Mutex<Option<Connection>>,
    /// Answers searches while the writer writes.
    reader: Mutex<Option<Connection>>,
    /// When the last update finished.
    last: Mutex<Option<Instant>>,
    /// Everything was read at least once.
    built: AtomicBool,
    stopped: AtomicBool,
}

impl Index {
    pub fn new(path: PathBuf, transcripts: impl Transcripts) -> Arc<Self> {
        Arc::new(Self::boxed(path, Box::new(transcripts), None))
    }

    /// One whose first build runs in a process of its own, which makes its transcripts with
    /// `make_transcripts(kind, arg)`.
    pub fn built_apart(path: PathBuf, transcripts: impl Transcripts, kind: &'static str, arg: String) -> Arc<Self> {
        Arc::new(Self::boxed(path, Box::new(transcripts), Some((kind, arg))))
    }

    fn boxed(path: PathBuf, transcripts: Box<dyn Transcripts>, recipe: Option<(&'static str, String)>) -> Self {
        Self {
            path,
            transcripts,
            recipe,
            child: Mutex::new(None),
            writer: Mutex::new(None),
            reader: Mutex::new(None),
            last: Mutex::new(None),
            built: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    /// Builds (or catches up) in the background after `delay` (again, after `stop`).
    pub fn build_later(self: &Arc<Self>, delay: Duration) {
        self.stopped.store(false, Ordering::SeqCst);
        let index = self.clone();
        let spawned = std::thread::Builder::new().name("search-index".into()).spawn(move || {
            std::thread::sleep(delay);
            if !index.stopped.load(Ordering::SeqCst) {
                let started = Instant::now();
                let result = match index.build_apart() {
                    Some(Ok(done)) => Ok(done),
                    _ if index.stopped.load(Ordering::SeqCst) => Ok(Update::Stopped),
                    Some(Err(e)) => {
                        log::warn!("search index {}: {e}; building it here", index.path.display());
                        index.update()
                    }
                    None => index.update(),
                };
                match result {
                    Ok(Update::Done) => log::info!("search index {} up to date in {:?}", index.path.display(), started.elapsed()),
                    Ok(_) => {}
                    Err(e) => log::warn!("search index {}: {e}", index.path.display()),
                }
            }
        });
        if let Err(e) = spawned {
            log::warn!("couldn't start the search index: {e}");
        }
    }

    /// Stops a build in progress (the bot is removed or turned off).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    /// Builds in a process of its own and waits for it. Meanwhile searches answer with what's
    /// there instead of catching up themselves. None when it can't be started.
    fn build_apart(&self) -> Option<Result<Update, String>> {
        let (kind, arg) = self.recipe.as_ref()?;
        if !ANSWERS_CHILD_FLAG.load(Ordering::SeqCst) {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let Ok(_writer) = self.writer.try_lock() else { return Some(Ok(Update::Busy)) };
        let spawned = crate::platform::std_command(exe)
            .arg(CHILD_FLAG)
            .arg(&self.path)
            .arg(kind)
            .arg(arg)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn();
        match spawned {
            Ok(child) => *self.child.lock().unwrap() = Some(child),
            Err(e) => return Some(Err(format!("couldn't start a process to build it: {e}"))),
        }
        loop {
            let status = match self.child.lock().unwrap().as_mut() {
                Some(child) => child.try_wait(),
                None => return Some(Ok(Update::Stopped)),
            };
            match status {
                Ok(Some(status)) => {
                    *self.child.lock().unwrap() = None;
                    *self.last.lock().unwrap() = Some(Instant::now());
                    return Some(if status.success() {
                        self.built.store(true, Ordering::SeqCst);
                        Ok(Update::Done)
                    } else {
                        Err(format!("the process building it ended with {status}"))
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(250)),
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }

    /// Messages matching `query`, the best one per thread, best first, leaving out `hidden`
    /// threads. `indexing`: not every transcript is in yet.
    pub async fn find(self: &Arc<Self>, query: String, limit: usize, hidden: HashSet<String>) -> Result<(Vec<Hit>, bool), RpcError> {
        let fresh = self.last.lock().unwrap().is_some_and(|t| t.elapsed() < CATCH_UP_EVERY);
        let caught_up = if fresh {
            self.built.load(Ordering::SeqCst)
        } else {
            let index = self.clone();
            let update = tokio::task::spawn_blocking(move || index.update());
            matches!(tokio::time::timeout(CATCH_UP_WAIT, update).await, Ok(Ok(Ok(Update::Done))))
        };
        let index = self.clone();
        let hits = tokio::task::spawn_blocking(move || index.search(&query, limit, &hidden))
            .await
            .map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
            .map_err(|e| RpcError::new("UNAVAILABLE", format!("Couldn't search messages: {e}")))?;
        Ok((hits, !caught_up))
    }

    /// Reads what's new in every transcript.
    pub fn update(&self) -> Result<Update, String> {
        let Ok(mut writer) = self.writer.try_lock() else { return Ok(Update::Busy) };
        if writer.is_none() {
            *writer = Some(open(&self.path).map_err(|e| e.to_string())?);
        }
        let conn = writer.as_mut().expect("just opened");
        let result = self.update_with(conn);
        match &result {
            Ok(Update::Done) => self.built.store(true, Ordering::SeqCst),
            // Start from a fresh connection next time.
            Err(_) => *writer = None,
            _ => {}
        }
        *self.last.lock().unwrap() = Some(Instant::now());
        result.map_err(|e| e.to_string())
    }

    fn update_with(&self, conn: &mut Connection) -> rusqlite::Result<Update> {
        let known = known_files(conn)?;
        let mut listed: Vec<(PathBuf, String, u64, u64)> = self
            .transcripts
            .list()
            .into_iter()
            .filter_map(|(path, thread)| {
                let meta = std::fs::metadata(&path).ok()?;
                Some((path, thread, meta.len(), modified_ms(&meta)))
            })
            .collect();
        // The latest first, so they're searchable soonest.
        listed.sort_by_key(|(_, _, _, modified)| std::cmp::Reverse(*modified));
        // One line buffer for every transcript (allocating one per transcript leaves the
        // allocator holding tens of MB after a build).
        let mut buf = Vec::new();
        let there: HashSet<&Path> = listed.iter().map(|(p, ..)| p.as_path()).collect();
        let gone: Vec<i64> = known.iter().filter(|(path, _)| !there.contains(Path::new(path.as_str()))).map(|(_, f)| f.id).collect();
        if !gone.is_empty() {
            let tx = conn.transaction()?;
            for id in gone {
                clear_file(&tx, id)?;
                tx.execute("DELETE FROM files WHERE id = ?1", [id])?;
            }
            tx.commit()?;
        }
        for (path, thread, size, modified) in &listed {
            if self.stopped.load(Ordering::SeqCst) {
                return Ok(Update::Stopped);
            }
            let row = path.to_str().and_then(|p| known.get(p));
            if row.is_some_and(|r| r.size == *size && r.modified == *modified && r.thread == *thread) {
                continue;
            }
            match self.read_file(conn, row, path, thread, *size, *modified, &mut buf) {
                Ok(true) => {}
                Ok(false) => return Ok(Update::Stopped),
                // Gone or unreadable mid-read: it's looked at again next time.
                Err(ReadError::Io(e)) => log::debug!("search index: {}: {e}", path.display()),
                Err(ReadError::Db(e)) => return Err(e),
            }
        }
        Ok(Update::Done)
    }

    /// Reads one transcript from where the last update stopped (or from the start). Returns
    /// false when stopped partway.
    #[allow(clippy::too_many_arguments)]
    fn read_file(&self, conn: &Connection, row: Option<&FileRow>, path: &Path, thread: &str, size: u64, modified: u64, buf: &mut Vec<u8>) -> Result<bool, ReadError> {
        let Some(path_str) = path.to_str() else { return Ok(true) };
        let packed = compressed(path);
        let mut file = File::open(path)?;
        let from = match row {
            Some(r) if !packed && r.thread == thread && size >= r.done && mark_of(&mut file, r.done)? == r.mark => r.done,
            _ => 0,
        };
        let mut reader: Box<dyn BufRead> = if packed {
            let decoder = ruzstd::decoding::StreamingDecoder::new(file).map_err(std::io::Error::other)?;
            Box::new(BufReader::with_capacity(READ_BUFFER, decoder))
        } else {
            file.seek(SeekFrom::Start(from))?;
            Box::new(BufReader::with_capacity(READ_BUFFER, file))
        };
        // From here on, a database error drops the connection (and the transaction with it).
        conn.execute_batch("BEGIN")?;
        let mut state = row.map(FileRow::state).unwrap_or_default();
        let id = match row {
            Some(r) => {
                if from == 0 {
                    clear_file(conn, r.id)?;
                    state = FileState::default();
                }
                r.id
            }
            None => {
                conn.execute("INSERT INTO files (path, thread) VALUES (?1, ?2)", params![path_str, thread])?;
                conn.last_insert_rowid()
            }
        };
        // Where reading got to, and the bytes before it (none for a compressed one, which is
        // always read whole).
        let mark = |done: u64| if packed { Vec::new() } else { File::open(path).and_then(|mut f| mark_of(&mut f, done)).unwrap_or_default() };
        let mut offset = from;
        let mut done = from;
        let mut written = 0;
        let result = (|| -> Result<bool, ReadError> {
            loop {
                let (n, whole, fits) = next_line(&mut reader, buf)?;
                if n == 0 {
                    break;
                }
                // Still being written: it's read next time.
                if !whole && !packed {
                    break;
                }
                let at = offset;
                offset += n as u64;
                done = offset;
                if state.skip || !fits || n <= 1 || !self.transcripts.wanted(buf) {
                    continue;
                }
                let Ok(value) = serde_json::from_slice::<Value>(buf) else { continue };
                let line = self.transcripts.read(&value);
                if line.skip {
                    state.skip = true;
                    clear_file(conn, id)?;
                    continue;
                }
                written += state.add(conn, id, at, line, modified, packed)?;
                if written >= BATCH {
                    written = 0;
                    // Not all read (size 0): the next update reads on from `done`.
                    state.save(conn, id, 0, modified, done, &mark(done))?;
                    conn.execute_batch("COMMIT; BEGIN")?;
                    if self.stopped.load(Ordering::SeqCst) {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })();
        let finished = match result {
            Ok(finished) => finished,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        if finished {
            state.save(conn, id, size, modified, if packed { size } else { done }, &mark(done))?;
        }
        conn.execute_batch("COMMIT")?;
        Ok(finished)
    }

    /// The best match per thread for `query`, best first.
    pub fn search(&self, query: &str, limit: usize, hidden: &HashSet<String>) -> rusqlite::Result<Vec<Hit>> {
        let Some(fts) = fts_query(query) else { return Ok(Vec::new()) };
        let mut reader = self.reader.lock().unwrap();
        if reader.is_none() {
            *reader = Some(open(&self.path)?);
        }
        let conn = reader.as_ref().expect("just opened");
        let result = search_with(conn, &fts, limit, hidden, &*self.transcripts);
        if result.is_err() {
            *reader = None;
        }
        result
    }
}

enum ReadError {
    Io(std::io::Error),
    Db(rusqlite::Error),
}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        ReadError::Io(e)
    }
}

impl From<rusqlite::Error> for ReadError {
    fn from(e: rusqlite::Error) -> Self {
        ReadError::Db(e)
    }
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// Reads through the next newline: how many bytes, whether the newline was there (else the
/// end came first), and whether the line fits in `LINE_MAX` (only then is it in `buf`).
fn next_line(reader: &mut dyn BufRead, buf: &mut Vec<u8>) -> std::io::Result<(usize, bool, bool)> {
    buf.clear();
    let (mut total, mut fits) = (0, true);
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok((total, false, fits));
        }
        let (used, whole) = match memchr::memchr(b'\n', chunk) {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if fits && buf.len() + used <= LINE_MAX {
            buf.extend_from_slice(&chunk[..used]);
        } else if fits {
            fits = false;
            buf.clear();
        }
        reader.consume(used);
        total += used;
        if whole {
            return Ok((total, true, fits));
        }
    }
}

/// The `MARK_BYTES` before `at`.
fn mark_of(file: &mut File, at: u64) -> std::io::Result<Vec<u8>> {
    let from = at.saturating_sub(MARK_BYTES);
    file.seek(SeekFrom::Start(from))?;
    let mut mark = Vec::with_capacity((at - from) as usize);
    file.take(at - from).read_to_end(&mut mark)?;
    Ok(mark)
}

/// Opens (creating, or rebuilding when it's from another schema or broken) the index.
fn open(path: &Path) -> rusqlite::Result<Connection> {
    match connect(path) {
        Ok(conn) => Ok(conn),
        Err(e) => {
            log::warn!("search index {} unusable ({e}); starting over", path.display());
            remove_db(path);
            connect(path)
        }
    }
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -{CACHE_KB}; PRAGMA temp_store = MEMORY;"
    ))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS body;
             DROP TABLE IF EXISTS msgs;
             DROP TABLE IF EXISTS files;
             DROP TABLE IF EXISTS kept;
             CREATE TABLE files (
               id INTEGER PRIMARY KEY,
               path TEXT NOT NULL UNIQUE,
               thread TEXT NOT NULL,
               size INTEGER NOT NULL DEFAULT 0,
               modified INTEGER NOT NULL DEFAULT 0,
               done INTEGER NOT NULL DEFAULT 0,
               mark BLOB NOT NULL DEFAULT x'',
               title TEXT,
               titled INTEGER NOT NULL DEFAULT 0,
               cwd TEXT,
               updated INTEGER NOT NULL DEFAULT 0,
               skip INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE msgs (
               id INTEGER PRIMARY KEY,
               file INTEGER NOT NULL,
               at INTEGER NOT NULL,
               role INTEGER NOT NULL,
               offset INTEGER NOT NULL,
               mid TEXT NOT NULL
             );
             CREATE INDEX msgs_file ON msgs (file);
             CREATE TABLE IF NOT EXISTS kept (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
             CREATE VIRTUAL TABLE body USING fts5 (
               text, content = '', contentless_delete = 1,
               tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3'
             );
             PRAGMA user_version = {SCHEMA};
             COMMIT;"
        ))?;
    }
    // A one-message table, for snippets (the index keeps no text to make them from).
    conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS temp.snip USING fts5 (text, tokenize = 'unicode61 remove_diacritics 2');")?;
    Ok(conn)
}

struct FileRow {
    id: i64,
    thread: String,
    size: u64,
    modified: u64,
    done: u64,
    mark: Vec<u8>,
    title: Option<String>,
    titled: bool,
    cwd: Option<String>,
    updated: u64,
    skip: bool,
}

impl FileRow {
    fn state(&self) -> FileState {
        FileState { title: self.title.clone(), titled: self.titled, cwd: self.cwd.clone(), updated: self.updated, skip: self.skip }
    }
}

fn known_files(conn: &Connection) -> rusqlite::Result<HashMap<String, FileRow>> {
    let mut stmt = conn.prepare("SELECT id, path, thread, size, modified, done, mark, title, titled, cwd, updated, skip FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            FileRow {
                id: r.get(0)?,
                thread: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                modified: r.get::<_, i64>(4)? as u64,
                done: r.get::<_, i64>(5)? as u64,
                mark: r.get(6)?,
                title: r.get(7)?,
                titled: r.get(8)?,
                cwd: r.get(9)?,
                updated: r.get::<_, i64>(10)? as u64,
                skip: r.get(11)?,
            },
        ))
    })?;
    rows.collect()
}

/// Takes a transcript's messages out of the index.
fn clear_file(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM body WHERE rowid IN (SELECT id FROM msgs WHERE file = ?1)", [id])?;
    conn.execute("DELETE FROM kept WHERE id IN (SELECT id FROM msgs WHERE file = ?1)", [id])?;
    conn.execute("DELETE FROM msgs WHERE file = ?1", [id])?;
    Ok(())
}

/// What's known about a transcript's thread, as it's read.
#[derive(Default)]
struct FileState {
    title: Option<String>,
    /// `title` is the transcript's own, not its first message.
    titled: bool,
    cwd: Option<String>,
    updated: u64,
    skip: bool,
}

impl FileState {
    /// Indexes a line's messages; returns how many.
    /// `keep`: keep the text too (a compressed transcript's).
    fn add(&mut self, conn: &Connection, file: i64, offset: u64, line: Line, modified: u64, keep: bool) -> rusqlite::Result<usize> {
        if let Some(title) = line.title.filter(|t| !t.trim().is_empty()) {
            self.title = Some(title);
            self.titled = true;
        }
        if let Some(cwd) = line.cwd.filter(|c| !c.is_empty()) {
            self.cwd = Some(cwd);
        }
        let mut insert_msg = conn.prepare_cached("INSERT INTO msgs (file, at, role, offset, mid) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut insert_body = conn.prepare_cached("INSERT INTO body (rowid, text) VALUES (?1, ?2)")?;
        let mut insert_kept = conn.prepare_cached("INSERT INTO kept (id, text) VALUES (?1, ?2)")?;
        let count = line.messages.len();
        for (i, m) in line.messages.into_iter().enumerate() {
            if self.title.is_none() && m.role == Role::User {
                let first = m.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                self.title = Some(truncate(first, 80));
            }
            let at = m.at.unwrap_or(if self.updated > 0 { self.updated } else { modified });
            self.updated = self.updated.max(at);
            let mid = if m.id.is_empty() { format!("#{i}") } else { m.id };
            insert_msg.execute(params![file, at as i64, (m.role == Role::Assistant) as i64, offset as i64, mid])?;
            let id = conn.last_insert_rowid();
            let text = cut_at(&m.text, TEXT_MAX);
            insert_body.execute(params![id, text])?;
            if keep {
                insert_kept.execute(params![id, text])?;
            }
        }
        Ok(count)
    }

    #[allow(clippy::too_many_arguments)]
    fn save(&self, conn: &Connection, file: i64, size: u64, modified: u64, done: u64, mark: &[u8]) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE files SET size = ?2, modified = ?3, done = ?4, mark = ?5, title = ?6, titled = ?7, cwd = ?8, updated = ?9, skip = ?10 WHERE id = ?1",
            params![file, size as i64, modified as i64, done as i64, mark, self.title, self.titled, self.cwd, self.updated as i64, self.skip],
        )?;
        Ok(())
    }
}

/// `text`'s first `max` bytes, at most (ending at a character).
fn cut_at(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The search as FTS5 takes it: every word, quoted, as the start of a word, and as a whole
/// word too, so "book" ranks "book" over "booked". (One letter only as a whole word, which is
/// quicker.)
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| {
            let quoted = format!("\"{}\"", w.replace('"', "\"\""));
            if w.chars().filter(|c| c.is_alphanumeric()).count() > 1 {
                format!("({quoted} OR {quoted}*)")
            } else {
                quoted
            }
        })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

struct Candidate {
    id: i64,
    file: i64,
    at: u64,
    role: Role,
    offset: u64,
    mid: String,
    rank: f64,
}

struct FileInfo {
    thread: String,
    path: PathBuf,
    title: Option<String>,
    cwd: Option<String>,
    updated: u64,
    skip: bool,
}

fn search_with(conn: &Connection, fts: &str, limit: usize, hidden: &HashSet<String>, transcripts: &dyn Transcripts) -> rusqlite::Result<Vec<Hit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.file, m.at, m.role, m.offset, m.mid, b.rank
         FROM (SELECT rowid, rank FROM body WHERE body MATCH ?1 ORDER BY rank LIMIT ?2) b
         JOIN msgs m ON m.id = b.rowid",
    )?;
    let candidates: Vec<Candidate> = stmt
        .query_map(params![fts, CANDIDATES], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                file: r.get(1)?,
                at: r.get::<_, i64>(2)? as u64,
                role: Role::from_i64(r.get(3)?),
                offset: r.get::<_, i64>(4)? as u64,
                mid: r.get(5)?,
                rank: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // bm25 ranks are negative: the lowest is the best.
    let best = candidates.iter().map(|c| c.rank).fold(0.0_f64, f64::min);
    let now = now_ms();
    let mut files: HashMap<i64, Option<FileInfo>> = HashMap::new();
    let mut per_thread: HashMap<String, (f64, Candidate)> = HashMap::new();
    let mut info = conn.prepare_cached("SELECT thread, path, title, cwd, updated, skip FROM files WHERE id = ?1")?;
    for c in candidates {
        if !files.contains_key(&c.file) {
            let found = info
                .query_row([c.file], |r| {
                    Ok(FileInfo {
                        thread: r.get(0)?,
                        path: PathBuf::from(r.get::<_, String>(1)?),
                        title: r.get(2)?,
                        cwd: r.get(3)?,
                        updated: r.get::<_, i64>(4)? as u64,
                        skip: r.get(5)?,
                    })
                })
                .optional()?;
            files.insert(c.file, found);
        }
        let Some(Some(f)) = files.get(&c.file) else { continue };
        if f.skip || hidden.contains(&f.thread) {
            continue;
        }
        let score = score(c.rank, best, c.at, now);
        match per_thread.get(&f.thread) {
            Some((s, _)) if *s >= score => {}
            _ => {
                per_thread.insert(f.thread.clone(), (score, c));
            }
        }
    }
    let mut ranked: Vec<(f64, Candidate)> = per_thread.into_values().collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.at.cmp(&a.1.at)));
    let mut hits = Vec::new();
    for (score, c) in ranked {
        if hits.len() >= limit {
            break;
        }
        let Some(Some(f)) = files.get(&c.file) else { continue };
        let text = if compressed(&f.path) {
            conn.query_row("SELECT text FROM kept WHERE id = ?1", [c.id], |r| r.get(0)).optional()?
        } else {
            message_text(&f.path, c.offset, &c.mid, transcripts)
        };
        // A transcript rewritten since it was indexed: its hit waits for the next update.
        let Some(text) = text else { continue };
        let snippet = snippet(conn, &text, fts)?.unwrap_or_else(|| truncate(&text, 200));
        hits.push(Hit {
            thread: f.thread.clone(),
            message_id: c.mid.clone(),
            role: c.role,
            at: c.at,
            snippet,
            score,
            title: f.title.clone(),
            cwd: f.cwd.clone(),
            updated: f.updated,
        });
    }
    Ok(hits)
}

/// Relevance (0–1, against the best match) weighed with how recent the message is.
fn score(rank: f64, best: f64, at: u64, now: u64) -> f64 {
    let relevance = if best < 0.0 { (rank / best).clamp(0.0, 1.0) } else { 1.0 };
    let days = now.saturating_sub(at) as f64 / 86_400_000.0;
    let recency = 1.0 / (1.0 + days / RECENCY_DAYS);
    relevance * (0.6 + 0.4 * recency)
}

/// The text of message `mid` in the line at `offset` (of a transcript that isn't compressed).
fn message_text(path: &Path, offset: u64, mid: &str, transcripts: &dyn Transcripts) -> Option<String> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    reader.read_until(b'\n', &mut buf).ok()?;
    let line = serde_json::from_slice::<Value>(&buf).ok()?;
    let messages = transcripts.read(&line).messages;
    messages.into_iter().enumerate().find(|(i, m)| if m.id.is_empty() { format!("#{i}") == mid } else { m.id == mid }).map(|(_, m)| m.text)
}

/// The words of `text` around what matches `fts`, as SQLite picks them.
fn snippet(conn: &Connection, text: &str, fts: &str) -> rusqlite::Result<Option<String>> {
    conn.execute("DELETE FROM temp.snip", [])?;
    conn.execute("INSERT INTO temp.snip (rowid, text) VALUES (1, ?1)", [text])?;
    let found: Option<String> = conn
        .query_row(
            &format!("SELECT snippet(snip, 0, '', '', ' … ', {SNIPPET_TOKENS}) FROM temp.snip WHERE snip MATCH ?1"),
            [fts],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Test transcripts: `<dir>/*.jsonl`, the thread being the file name, each line
    /// `{id, role, text, ts}` (or `{title}`, `{cwd}`, `{skip}`).
    struct Fake(PathBuf);

    impl Transcripts for Fake {
        fn list(&self) -> Vec<(PathBuf, String)> {
            let mut out: Vec<(PathBuf, String)> = std::fs::read_dir(&self.0)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().contains(".jsonl"))
                .map(|p| {
                    let name = p.file_name().unwrap().to_string_lossy();
                    let thread = name.split('.').next().unwrap().to_string();
                    (p, thread)
                })
                .collect();
            out.sort();
            out
        }

        fn read(&self, line: &Value) -> Line {
            let messages = line
                .get("text")
                .and_then(Value::as_str)
                .map(|text| {
                    vec![Msg {
                        id: line.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        role: if line["role"] == "user" { Role::User } else { Role::Assistant },
                        at: line.get("ts").and_then(Value::as_u64),
                        text: text.to_string(),
                    }]
                })
                .unwrap_or_default();
            Line {
                messages,
                title: line.get("title").and_then(Value::as_str).map(str::to_string),
                cwd: line.get("cwd").and_then(Value::as_str).map(str::to_string),
                skip: line.get("skip").is_some(),
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lg-search-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("t")).unwrap();
        dir
    }

    fn line(id: &str, role: &str, text: &str) -> String {
        format!("{}\n", json!({ "id": id, "role": role, "text": text, "ts": now_ms() }))
    }

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn index(dir: &Path) -> Arc<Index> {
        Index::new(dir.join("search.db"), Fake(dir.join("t")))
    }

    fn found(index: &Index, query: &str) -> Vec<(String, String)> {
        index.search(query, 25, &HashSet::new()).unwrap().into_iter().map(|h| (h.thread, h.message_id)).collect()
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
    fn finds_words_by_their_start_with_every_word_required() {
        let dir = temp_dir("words");
        let a = dir.join("t/a.jsonl");
        append(&a, &(line("a1", "user", "Book the flight to São Paulo") + &line("a2", "assistant", "Booked: an ebook about birds")));
        append(&dir.join("t/b.jsonl"), &line("b1", "user", "the ebook reader is broken"));
        let index = index(&dir);
        assert_eq!(index.update().unwrap(), Update::Done);
        // One hit per thread, its best message.
        assert_eq!(found(&index, "book"), [("a".to_string(), "a1".to_string())]);
        assert_eq!(found(&index, "sao paulo"), [("a".to_string(), "a1".to_string())]);
        assert_eq!(found(&index, "ebook broken"), [("b".to_string(), "b1".to_string())]);
        assert!(found(&index, "flight birds broken").is_empty());
        assert!(found(&index, "  -- ").is_empty());
        assert_eq!(found(&index, "\"birds"), [("a".to_string(), "a2".to_string())]);
        assert_eq!(found(&index, "booke"), [("a".to_string(), "a2".to_string())]);
        let hit = &index.search("paulo", 25, &HashSet::new()).unwrap()[0];
        assert_eq!(hit.role, Role::User);
        assert_eq!(hit.title.as_deref(), Some("Book the flight to São Paulo"));
        assert!(hit.snippet.contains("São Paulo"), "{}", hit.snippet);
        assert!(index.search("paulo", 25, &HashSet::from(["a".to_string()])).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_messages_are_cut_around_the_match() {
        let dir = temp_dir("snippet");
        let words: Vec<String> = (0..200).map(|i| format!("word{i}")).collect();
        let text = format!("{} needle {}", words[..100].join(" "), words[100..].join(" "));
        append(&dir.join("t/a.jsonl"), &line("a1", "assistant", &text));
        let index = index(&dir);
        index.update().unwrap();
        let hit = &index.search("needle", 25, &HashSet::new()).unwrap()[0];
        assert!(hit.snippet.starts_with("… ") && hit.snippet.ends_with(" …"), "{}", hit.snippet);
        assert!(hit.snippet.contains("needle"));
        assert!(hit.snippet.split_whitespace().count() < 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_only_what_was_appended_and_waits_for_a_whole_line() {
        let dir = temp_dir("append");
        let a = dir.join("t/a.jsonl");
        append(&a, &line("a1", "user", "first apple"));
        let index = index(&dir);
        index.update().unwrap();
        append(&a, &line("a2", "assistant", "second banana"));
        append(&a, "{\"id\":\"a3\",\"role\":\"user\",\"text\":\"cherry"); // still being written
        index.update().unwrap();
        assert_eq!(found(&index, "banana"), [("a".to_string(), "a2".to_string())]);
        assert!(found(&index, "cherry").is_empty());
        append(&a, " pie\"}\n");
        index.update().unwrap();
        assert_eq!(found(&index, "cherry"), [("a".to_string(), "a3".to_string())]);
        // Nothing read twice.
        let conn = open(&dir.join("search.db")).unwrap();
        let count: i64 = conn.query_row("SELECT count(*) FROM msgs", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_too_long_to_be_words_are_skipped_unread() {
        let dir = temp_dir("long");
        let a = dir.join("t/a.jsonl");
        let huge = "x".repeat(LINE_MAX);
        append(&a, &(line("a1", "assistant", &format!("{huge} needle")) + &line("a2", "user", "needle in a short one")));
        let index = index(&dir);
        index.update().unwrap();
        assert_eq!(found(&index, "needle"), [("a".to_string(), "a2".to_string())]);
        let mut reader = BufReader::with_capacity(16, "abc\ndefghijklmnopqrstuvwxyz\ntail".as_bytes());
        let mut buf = Vec::new();
        assert_eq!(next_line(&mut reader, &mut buf).unwrap(), (4, true, true));
        assert_eq!(buf, b"abc\n");
        assert_eq!(next_line(&mut reader, &mut buf).unwrap(), (24, true, true));
        assert_eq!(next_line(&mut reader, &mut buf).unwrap(), (4, false, true));
        assert_eq!(next_line(&mut reader, &mut buf).unwrap(), (0, false, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewritten_and_deleted_transcripts_are_read_again_or_dropped() {
        let dir = temp_dir("rewrite");
        let a = dir.join("t/a.jsonl");
        let b = dir.join("t/b.jsonl");
        append(&a, &(line("a1", "user", "old lemon") + &line("a2", "user", "old lime")));
        append(&b, &line("b1", "user", "grape"));
        let index = index(&dir);
        index.update().unwrap();
        assert_eq!(found(&index, "lemon").len(), 1);
        // Shorter: read again from the start.
        std::fs::write(&a, line("a9", "user", "new melon")).unwrap();
        index.update().unwrap();
        assert!(found(&index, "lemon").is_empty());
        assert_eq!(found(&index, "melon"), [("a".to_string(), "a9".to_string())]);
        // Longer, but what was read changed (a message replaced in place).
        std::fs::write(&a, line("a9", "user", "new mango, ripe") + &line("a10", "user", "kiwi")).unwrap();
        index.update().unwrap();
        assert!(found(&index, "melon").is_empty());
        assert_eq!(found(&index, "mango kiwi").len(), 0);
        assert_eq!(found(&index, "mango").len(), 1);
        assert_eq!(found(&index, "kiwi").len(), 1);
        std::fs::remove_file(&b).unwrap();
        index.update().unwrap();
        assert!(found(&index, "grape").is_empty());
        let conn = open(&dir.join("search.db")).unwrap();
        let files: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(files, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_transcripts_are_read_whole_and_their_text_kept() {
        let dir = temp_dir("zst");
        let data = line("z1", "user", "zebra crossing") + &line("z2", "assistant", "quokka smile");
        assert!(data.len() < 256);
        std::fs::write(dir.join("t/z.jsonl.zst"), zst(data.as_bytes())).unwrap();
        let index = index(&dir);
        index.update().unwrap();
        let hits = index.search("quokka", 25, &HashSet::new()).unwrap();
        assert_eq!((hits[0].thread.as_str(), hits[0].message_id.as_str(), hits[0].snippet.as_str()), ("z", "z2", "quokka smile"));
        // Unchanged: not read again.
        index.update().unwrap();
        let conn = open(&dir.join("search.db")).unwrap();
        let count: i64 = conn.query_row("SELECT count(*) FROM msgs", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
        std::fs::remove_file(dir.join("t/z.jsonl.zst")).unwrap();
        index.update().unwrap();
        let kept: i64 = conn.query_row("SELECT count(*) FROM kept", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helpers_titles_and_folders() {
        let dir = temp_dir("meta");
        let a = dir.join("t/a.jsonl");
        append(&a, &(format!("{}\n", json!({ "cwd": "/w/app" })) + &line("a1", "user", "  \nfix the login\nplease") + &format!("{}\n", json!({ "title": "Login fix" }))));
        append(&dir.join("t/h.jsonl"), &(format!("{}\n", json!({ "skip": true })) + &line("h1", "user", "login helper")));
        let index = index(&dir);
        index.update().unwrap();
        let hits = index.search("login", 25, &HashSet::new()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].title.as_deref(), hits[0].cwd.as_deref()), (Some("Login fix"), Some("/w/app")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_index_starts_over() {
        let dir = temp_dir("broken");
        append(&dir.join("t/a.jsonl"), &line("a1", "user", "walrus"));
        std::fs::write(dir.join("search.db"), b"not a database at all, not even close to one").unwrap();
        let index = index(&dir);
        assert_eq!(index.update().unwrap(), Update::Done);
        assert_eq!(found(&index, "walrus").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_never_ran_child_main_builds_in_place() {
        let dir = temp_dir("apart");
        let index = Index::built_apart(dir.join("search.db"), Fake(dir.join("t")), "claudecode", String::new());
        assert!(index.build_apart().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn find_catches_up_first() {
        let dir = temp_dir("find");
        let a = dir.join("t/a.jsonl");
        append(&a, &line("a1", "user", "otter"));
        let index = index(&dir);
        let (hits, indexing) = index.find("otter".into(), 10, HashSet::new()).await.unwrap();
        assert_eq!((hits.len(), indexing), (1, false));
        assert_eq!(hits[0].to_json("agent:x:dashboard:a", None, None, None)["sessionId"], "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn words_of_keeps_what_was_said() {
        let messages = [
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }, { "type": "image" }], "timestamp": 5, "__openclaw": { "id": "u" } }),
            json!({ "role": "assistant", "content": [{ "type": "thinking", "thinking": "hmm" }, { "type": "toolCall", "name": "x" }] }),
            json!({ "role": "toolResult", "content": "out" }),
            json!({ "role": "assistant", "content": "plain" }),
        ];
        let got = words_of(&messages);
        assert_eq!(got, [
            Msg { id: "u".into(), role: Role::User, at: Some(5), text: "hi".into() },
            Msg { id: String::new(), role: Role::Assistant, at: None, text: "plain".into() },
        ]);
    }

    #[test]
    fn fts_queries_are_quoted_prefixes() {
        assert_eq!(fts_query("book sao").as_deref(), Some("(\"book\" OR \"book\"*) AND (\"sao\" OR \"sao\"*)"));
        assert_eq!(fts_query("a \"x y").as_deref(), Some("\"a\" AND \"\"\"x\" AND \"y\""));
        assert_eq!(fts_query("- ..."), None);
    }

    #[test]
    fn recent_beats_old_when_as_relevant() {
        let now = 100 * 86_400_000;
        assert!(score(-5.0, -5.0, now, now) > score(-5.0, -5.0, 0, now));
        assert!(score(-5.0, -5.0, 0, now) > score(-1.0, -5.0, now, now));
    }
}
