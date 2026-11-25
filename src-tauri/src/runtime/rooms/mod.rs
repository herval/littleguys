//! Rooms: chats with you and several guys in them (docs/GROUP-CHATS.md).
//!
//! No bot has a session several agents share, and the guys in a room can live on different
//! bots and machines. So littleguys runs the room. To the rest of the app it's one more
//! runtime (gateway id [`ROOMS_GATEWAY_ID`]) where every room is a guy with one chat:
//!
//! - `agents.list` → a row per room: `{id, name, identity:{name}, room:{lead, paused,
//!   members:[{uid, name, handle, attention: oncall|listening, seat?, folder?}]}}`
//! - `sessions.list` → its chat, `agent:<roomId>:main`
//! - `chat.history` → the room's transcript. A guy's message is an `assistant` row with
//!   `room: {from: uid, to: [uid]}`; yours a `user` row with `room: {to}`
//! - `chat.send {sessionKey, message, idempotencyKey, to?: [uid] | "everyone"}`: you wrote.
//!   Without `to` it goes to whoever the text addresses (`@handle`), else to the guy that
//!   spoke last, else to the lead. It never waits: writing while guys work starts a new round.
//! - `chat.abort` stops every turn under way in the room
//! - `rooms.create {name?, members:[uid], lead?}` → `{id, key}`; `rooms.update {id, name?, lead?,
//!   add?:[uid], remove?:[uid], attention?:{uid: mode}, folder?:{uid: path|null}}`; `rooms.delete {id}`;
//!   `rooms.continue {id}` (after a pause); `rooms.state {id}`
//!
//! Events: `session.message` for every line, `chat` `status`/`final`/`aborted` around a round
//! (from your message until the room is quiet), `room.state {sessionKey, working:[{uid, why}],
//! paused}` whenever who's working changes, `agents.changed` when rooms do.
//!
//! Each member takes part from a seat: an ordinary thread of that guy on its own bot
//! (`sessions.create`), where it has its tools, files and approvals. A turn is one message
//! sent there (talk.rs); the last thing the guy says in it is what the room posts. Who gets
//! a turn is floor.rs. The transcript and the seats are kept in littleguys' data folder
//! (`rooms/rooms.json`, `rooms/<id>.jsonl`): nothing is written into a bot's own files.
//!
//! Rooms run while littleguys runs. A turn under way when it quits finishes in its seat,
//! and the guy tells the room about it the next time it's given a turn.

pub mod floor;
pub mod talk;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use self::floor::{Action, Floor, Seat, Wake, Why};
use self::talk::{Said, Who};
use crate::gateway::client::{ConnStatus, Hooks, Phase, RpcError};
use crate::gateway::identity::{now_ms, write_private};
use crate::openclaw::config::{Credential, GatewayMode};
use crate::runtime::events::{RunEvent, RunPhase};
use crate::runtime::{archived_filter, keys, truncate, unsupported, Capabilities, RuntimeKind, ThreadFlags};

pub const ROOMS_GATEWAY_ID: &str = "rooms";
/// How many times guys may address each other after a message of yours before the room asks you.
const MAX_HOPS: u32 = 8;
/// How long a listener waits for a working guy before it looks anyway.
const LOOK_AFTER: Duration = Duration::from_secs(30);
/// A turn's end settles this long before the seat is asked whether it's really over.
const SETTLE: Duration = Duration::from_millis(400);
/// A seat's transcript can trail its turn's end: how often, and how many times, it's read for the reply.
const REPLY_RETRY: Duration = Duration::from_millis(600);
const REPLY_TRIES: usize = 5;

/// The other bots, as a room needs them (Activity; a fake in tests).
pub trait Bots: Send + Sync + 'static {
    fn call(&self, gateway: &str, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
    /// A guy's name, if its bot is connected and has it.
    fn guy(&self, gateway: &str, agent: &str) -> Option<String>;
    fn online(&self, gateway: &str) -> bool;
    /// A message can join a turn under way there (`chat.send` with `delivery: "steer"`).
    fn steers(&self, gateway: &str) -> bool;
    fn is_running(&self, gateway: &str, key: &str) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attention {
    /// Gets a turn only when addressed.
    #[default]
    Oncall,
    /// Also gets one look when the room goes quiet.
    Listening,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    /// `gatewayId/agentId`.
    uid: String,
    /// Its name when last seen, for while its bot is away.
    name: String,
    #[serde(default)]
    attention: Attention,
    /// Its thread for this room, on its own bot.
    #[serde(default)]
    seat: Option<String>,
    /// The folder its seat works in, for guys whose threads each work in one (Claude Code,
    /// Codex, ACP). None: wherever that guy starts a thread by default.
    #[serde(default)]
    folder: Option<String>,
    /// How many of the room's lines it has been shown.
    #[serde(default)]
    seen: usize,
    /// It has the room's rules for the members as they are now.
    #[serde(default)]
    briefed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Room {
    id: String,
    name: String,
    lead: String,
    members: Vec<Member>,
    created_at: u64,
    updated_at: u64,
    #[serde(default)]
    flags: ThreadFlags,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    id: String,
    at: u64,
    /// None: you.
    #[serde(default)]
    from: Option<String>,
    text: String,
    /// The members it addresses.
    #[serde(default)]
    to: Vec<String>,
    /// Something the room says itself ("Patch is offline").
    #[serde(default)]
    note: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_send_id: Option<String>,
}

#[derive(Default)]
struct Live {
    floor: Floor,
    /// Bumped when you stop the room: turns from before no longer count.
    epoch: u64,
    /// The round under way, as `chat` events name it.
    run: Option<String>,
}

struct SeatEvent {
    phase: RunPhase,
    run: Option<String>,
    text: Option<String>,
    error: Option<String>,
    /// Claude Code wouldn't start in a folder it hasn't been told to trust.
    untrusted: Option<String>,
}

type Waiters = HashMap<(String, String), Vec<(u64, mpsc::UnboundedSender<SeatEvent>)>>;

#[derive(Default)]
struct St {
    rooms: Vec<Room>,
    lines: HashMap<String, Vec<Line>>,
    live: HashMap<String, Live>,
    /// Who waits for a seat's turn to end, by (gateway, session key).
    waiters: Waiters,
}

struct Inner {
    hooks: Hooks,
    dir: PathBuf,
    bots: OnceLock<Arc<dyn Bots>>,
    status: Mutex<ConnStatus>,
    st: Mutex<St>,
    started: AtomicBool,
    next_waiter: AtomicU64,
    look_after: Duration,
}

#[derive(Clone)]
pub struct RoomsClient {
    inner: Arc<Inner>,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new("INVALID_REQUEST", message)
}

fn no_room() -> RpcError {
    RpcError::new("NOT_FOUND", "That room is gone.")
}

fn split_uid(uid: &str) -> Option<(&str, &str)> {
    uid.split_once('/').filter(|(g, a)| !g.is_empty() && !a.is_empty())
}

fn room_key(id: &str) -> String {
    keys::main(id)
}

fn room_of_key(params: &Value) -> Result<String, RpcError> {
    let key = params.get("sessionKey").or_else(|| params.get("key")).and_then(Value::as_str).unwrap_or_default();
    keys::split(key).map(|(id, _)| id.to_string()).ok_or_else(no_room)
}

fn seats(room: &Room) -> Vec<Seat<'_>> {
    room.members.iter().map(|m| Seat { uid: &m.uid, listening: m.attention == Attention::Listening }).collect()
}

fn whos(room: &Room) -> Vec<Who> {
    let names: Vec<String> = room.members.iter().map(|m| m.name.clone()).collect();
    let handles = talk::handles(&names);
    room.members.iter().zip(handles).map(|(m, handle)| Who { uid: m.uid.clone(), name: m.name.clone(), handle }).collect()
}

fn name_of<'a>(room: &'a Room, uid: &str) -> &'a str {
    room.members.iter().find(|m| m.uid == uid).map(|m| m.name.as_str()).unwrap_or("someone")
}

/// A line as `chat.history` and `session.message` carry it.
fn message(line: &Line) -> Value {
    if line.note {
        return json!({ "role": "system", "content": line.text, "timestamp": line.at, "__openclaw": { "id": line.id } });
    }
    let mut m = json!({
        "role": if line.from.is_some() { "assistant" } else { "user" },
        "content": [{ "type": "text", "text": line.text }],
        "timestamp": line.at,
        "room": { "from": line.from, "to": line.to },
        "__openclaw": { "id": line.id },
    });
    if let Some(id) = &line.client_send_id {
        m["__openclaw"]["clientSendId"] = json!(id);
    }
    m
}

fn text_of(message: &Value) -> Option<String> {
    let text = match message.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

impl RoomsClient {
    pub fn new(data_dir: PathBuf, hooks: Hooks) -> Self {
        Self::with(data_dir, hooks, LOOK_AFTER)
    }

    fn with(data_dir: PathBuf, hooks: Hooks, look_after: Duration) -> Self {
        let dir = data_dir.join("rooms");
        let rooms: Vec<Room> = std::fs::read(dir.join("rooms.json")).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
        let lines = rooms.iter().map(|r| (r.id.clone(), read_lines(&dir, &r.id))).collect();
        let status = ConnStatus {
            gateway_id: ROOMS_GATEWAY_ID.into(),
            gateway_name: "Rooms".into(),
            kind: RuntimeKind::Rooms,
            capabilities: Capabilities::rooms(),
            phase: Phase::Starting,
            mode: Some(GatewayMode::Local),
            ..Default::default()
        };
        Self {
            inner: Arc::new(Inner {
                hooks,
                dir,
                bots: OnceLock::new(),
                status: Mutex::new(status),
                st: Mutex::new(St { rooms, lines, ..Default::default() }),
                started: AtomicBool::new(false),
                next_waiter: AtomicU64::new(1),
                look_after,
            }),
        }
    }

    /// The bots its members live on. Set once, before [`RoomsClient::start`].
    pub fn set_bots(&self, bots: Arc<dyn Bots>) {
        let _ = self.inner.bots.set(bots);
    }

    pub fn id(&self) -> &str {
        ROOMS_GATEWAY_ID
    }

    pub fn status(&self) -> ConnStatus {
        self.inner.status.lock().unwrap().clone()
    }

    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let status = {
            let mut status = self.inner.status.lock().unwrap();
            status.phase = Phase::Connected;
            status.clone()
        };
        (self.inner.hooks.status)(&status);
        (self.inner.hooks.connected)();
    }

    pub fn stop(&self) {}
    pub fn reconnect_now(&self) {}
    pub fn set_name(&self, _name: String) {}
    pub fn set_credential_override(&self, _credential: Option<Credential>) {}

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, Duration::ZERO).await
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, _timeout: Duration) -> Result<Value, RpcError> {
        match method {
            "sessions.subscribe" | "sessions.unsubscribe" | "sessions.messages.subscribe" | "sessions.messages.unsubscribe" => {
                Ok(json!({ "subscribed": true }))
            }
            "exec.approval.list" => Ok(json!({ "approvals": [] })),
            "agents.list" => Ok(self.agents()),
            "sessions.list" => Ok(self.sessions(&params)),
            "sessions.patch" => self.patch(&params),
            "chat.history" => self.history(&params),
            "chat.send" => self.send(&params),
            "chat.abort" => self.abort(&room_of_key(&params)?),
            "rooms.create" => self.create(&params),
            "rooms.update" => self.update(&params),
            "rooms.delete" => self.delete(&params),
            "rooms.continue" => self.resume(&params),
            "rooms.state" => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                let st = self.inner.st.lock().unwrap();
                if !st.rooms.iter().any(|r| r.id == id) {
                    return Err(no_room());
                }
                Ok(state_of(&st, id))
            }
            _ => Err(unsupported(method)),
        }
    }

    /// The room a guy's thread is its seat in: `(room id, room name)`.
    pub fn seat_room(&self, gateway: &str, key: &str) -> Option<(String, String)> {
        let st = self.inner.st.lock().unwrap();
        st.rooms.iter().find_map(|r| {
            r.members
                .iter()
                .any(|m| m.seat.as_deref() == Some(key) && split_uid(&m.uid).is_some_and(|(g, _)| g == gateway))
                .then(|| (r.id.clone(), r.name.clone()))
        })
    }

    /// An event from another bot: a seat's turn ending is what a room waits on.
    pub fn on_bot_event(&self, gateway: &str, name: &str, payload: &Value) {
        let Some(event) = RunEvent::decode(name, payload) else { return };
        let st = self.inner.st.lock().unwrap();
        let Some(waiting) = st.waiters.get(&(gateway.to_string(), event.session_key.to_string())) else { return };
        for (_, tx) in waiting {
            let _ = tx.send(SeatEvent {
                phase: event.phase,
                run: event.run_id.map(str::to_string),
                text: payload.get("message").and_then(text_of),
                error: payload.get("errorMessage").and_then(Value::as_str).map(str::to_string),
                untrusted: payload.get("untrustedFolder").and_then(Value::as_str).map(str::to_string),
            });
        }
    }

    fn bots(&self) -> Option<Arc<dyn Bots>> {
        self.inner.bots.get().cloned()
    }

    fn emit(&self, name: &str, payload: Value) {
        (self.inner.hooks.event)(name, &payload, &Value::Null);
    }

    fn save(&self, st: &St) {
        match serde_json::to_vec_pretty(&st.rooms) {
            Ok(raw) => {
                if let Err(e) = write_private(&self.inner.dir.join("rooms.json"), &raw) {
                    log::warn!("couldn't save rooms: {e}");
                }
            }
            Err(e) => log::warn!("couldn't save rooms: {e}"),
        }
    }

    /// Adds a line to a room's transcript, in memory and on disk.
    fn append(&self, st: &mut St, room: &str, line: Line) {
        let path = self.inner.dir.join(format!("{room}.jsonl"));
        let written = std::fs::create_dir_all(&self.inner.dir)
            .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path))
            .and_then(|mut f| writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default()));
        if let Err(e) = written {
            log::warn!("couldn't save a room's message: {e}");
        }
        st.lines.entry(room.to_string()).or_default().push(line);
    }

    // ---- what the app reads -----------------------------------------------------------

    fn agents(&self) -> Value {
        let bots = self.bots();
        let mut st = self.inner.st.lock().unwrap();
        // A guy renamed since it joined goes by its new name.
        let mut renamed = false;
        if let Some(bots) = &bots {
            for m in st.rooms.iter_mut().flat_map(|r| r.members.iter_mut()) {
                if let Some(name) = split_uid(&m.uid).and_then(|(g, a)| bots.guy(g, a)).filter(|n| *n != m.name) {
                    m.name = name;
                    renamed = true;
                }
            }
        }
        if renamed {
            self.save(&st);
        }
        let agents: Vec<Value> = st
            .rooms
            .iter()
            .map(|r| {
                let handles = whos(r);
                let members: Vec<Value> = r
                    .members
                    .iter()
                    .zip(handles)
                    .map(|(m, who)| json!({ "uid": m.uid, "name": m.name, "handle": who.handle, "attention": m.attention, "seat": m.seat, "folder": m.folder }))
                    .collect();
                json!({
                    "id": r.id,
                    "name": r.name,
                    "identity": { "name": r.name },
                    "room": { "lead": r.lead, "members": members, "paused": st.live.get(&r.id).is_some_and(|l| l.floor.is_paused()) },
                })
            })
            .collect();
        json!({ "defaultId": Value::Null, "agents": agents })
    }

    fn sessions(&self, params: &Value) -> Value {
        if archived_filter(params) == Some(true) {
            return json!({ "sessions": [] });
        }
        let st = self.inner.st.lock().unwrap();
        let rows: Vec<Value> = st
            .rooms
            .iter()
            .map(|r| {
                let last = st.lines.get(&r.id).and_then(|l| l.iter().rev().find(|l| !l.note));
                let preview = last.map(|l| match &l.from {
                    Some(uid) => format!("{}: {}", name_of(r, uid), truncate(&l.text, 200)),
                    None => truncate(&l.text, 200),
                });
                let live = st.live.get(&r.id);
                let mut row = json!({
                    "key": room_key(&r.id),
                    "agentId": r.id,
                    "sessionId": r.id,
                    "updatedAt": last.map(|l| l.at).unwrap_or(r.updated_at),
                    "lastMessagePreview": preview,
                    "hasActiveRun": live.is_some_and(|l| l.floor.is_open()),
                    "runId": live.and_then(|l| l.run.clone()),
                    "archived": false,
                });
                r.flags.add_to(&mut row);
                row["label"] = Value::Null;
                row
            })
            .collect();
        json!({ "sessions": rows })
    }

    fn patch(&self, params: &Value) -> Result<Value, RpcError> {
        let id = room_of_key(params)?;
        let mut st = self.inner.st.lock().unwrap();
        let room = st.rooms.iter_mut().find(|r| r.id == id).ok_or_else(no_room)?;
        // A room's name is `rooms.update`'s, and it can't be archived: only read and unread.
        let changed = room.flags.patch(&json!({ "unread": params.get("unread") }));
        if changed {
            self.save(&st);
        }
        Ok(json!({ "ok": true, "key": room_key(&id) }))
    }

    fn history(&self, params: &Value) -> Result<Value, RpcError> {
        let id = room_of_key(params)?;
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 1000) as usize;
        let st = self.inner.st.lock().unwrap();
        if !st.rooms.iter().any(|r| r.id == id) {
            return Err(no_room());
        }
        let lines = st.lines.get(&id).map(Vec::as_slice).unwrap_or_default();
        let messages: Vec<Value> = lines.iter().skip(lines.len().saturating_sub(limit)).map(message).collect();
        Ok(json!({
            "sessionKey": room_key(&id),
            "sessionId": id,
            "messages": messages,
            "hasMore": lines.len() > limit,
            "inFlightRun": st.live.get(&id).filter(|l| l.floor.is_open()).and_then(|l| l.run.clone()).map(|run| json!({ "runId": run })),
        }))
    }

    // ---- rooms and their members --------------------------------------------------------

    fn member(&self, uid: &str) -> Result<Member, RpcError> {
        let (gateway, agent) = split_uid(uid).filter(|(g, _)| *g != ROOMS_GATEWAY_ID).ok_or_else(|| invalid("That isn't a guy."))?;
        let name = self.bots().and_then(|b| b.guy(gateway, agent)).ok_or_else(|| invalid("That guy isn't around right now."))?;
        Ok(Member { uid: uid.to_string(), name, attention: Attention::default(), seat: None, folder: None, seen: 0, briefed: false })
    }

    fn create(&self, params: &Value) -> Result<Value, RpcError> {
        let mut members: Vec<Member> = Vec::new();
        for uid in params.get("members").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !members.iter().any(|m| m.uid == uid) {
                members.push(self.member(uid)?);
            }
        }
        if members.len() < 2 {
            return Err(invalid("A room needs at least two guys."));
        }
        let lead = params.get("lead").and_then(Value::as_str).filter(|l| members.iter().any(|m| m.uid == *l)).unwrap_or(&members[0].uid).to_string();
        let name = params.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
        let name = name.unwrap_or_else(|| members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "));
        let id = format!("room-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        let now = now_ms();
        {
            let mut st = self.inner.st.lock().unwrap();
            st.rooms.push(Room { id: id.clone(), name, lead, members, created_at: now, updated_at: now, flags: ThreadFlags::default() });
            self.save(&st);
        }
        self.emit("agents.changed", json!({ "agentId": id }));
        Ok(json!({ "ok": true, "id": id, "key": room_key(&id) }))
    }

    fn update(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let uids = |field: &str| -> Vec<String> {
            params.get(field).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
        };
        let mut joining = Vec::new();
        for uid in uids("add") {
            joining.push(self.member(&uid)?);
        }
        let leaving = uids("remove");
        let mut left_seats = Vec::new();
        let (actions, epoch) = {
            let mut st = self.inner.st.lock().unwrap();
            let St { rooms, live, lines, .. } = &mut *st;
            let room = rooms.iter_mut().find(|r| r.id == id).ok_or_else(no_room)?;
            if room.members.iter().filter(|m| !leaving.contains(&m.uid)).count() + joining.iter().filter(|j| !room.members.iter().any(|m| m.uid == j.uid)).count() < 2 {
                return Err(invalid("A room needs at least two guys."));
            }
            if let Some(name) = params.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
                room.name = name.to_string();
            }
            let before: Vec<String> = room.members.iter().map(|m| m.uid.clone()).collect();
            for m in room.members.iter().filter(|m| leaving.contains(&m.uid)) {
                if let (Some((gateway, _)), Some(seat)) = (split_uid(&m.uid), &m.seat) {
                    left_seats.push((m.uid.clone(), gateway.to_string(), seat.clone()));
                }
            }
            room.members.retain(|m| !leaving.contains(&m.uid));
            let seen = lines.get(&id).map(Vec::len).unwrap_or(0);
            for mut m in joining {
                if !room.members.iter().any(|x| x.uid == m.uid) {
                    // It joins the conversation from here on, with the last of it for context.
                    m.seen = seen.saturating_sub(talk::HISTORY_LIMIT);
                    room.members.push(m);
                }
            }
            for (uid, mode) in params.get("attention").and_then(Value::as_object).into_iter().flatten() {
                if let (Some(m), Ok(mode)) = (room.members.iter_mut().find(|m| m.uid == *uid), serde_json::from_value::<Attention>(mode.clone())) {
                    m.attention = mode;
                }
            }
            for (uid, folder) in params.get("folder").and_then(Value::as_object).into_iter().flatten() {
                let folder = folder.as_str().map(str::trim).filter(|f| !f.is_empty()).map(str::to_string);
                if let Some(m) = room.members.iter_mut().find(|m| m.uid == *uid).filter(|m| m.folder != folder) {
                    // A thread's folder is fixed once it starts: it takes a new seat there, with the last of the room for context.
                    m.folder = folder;
                    m.seat = None;
                    m.briefed = false;
                    m.seen = seen.saturating_sub(talk::HISTORY_LIMIT);
                }
            }
            if let Some(lead) = params.get("lead").and_then(Value::as_str).filter(|l| room.members.iter().any(|m| m.uid == *l)) {
                room.lead = lead.to_string();
            }
            if !room.members.iter().any(|m| m.uid == room.lead) {
                room.lead = room.members[0].uid.clone();
            }
            // Who's here changed: everyone hears the rules again, with the new list.
            if before != room.members.iter().map(|m| m.uid.clone()).collect::<Vec<_>>() || params.get("lead").is_some() {
                room.members.iter_mut().for_each(|m| m.briefed = false);
            }
            room.updated_at = now_ms();
            let live = live.entry(id.clone()).or_default();
            let seats = seats(room);
            let mut actions = Vec::new();
            for (uid, _, _) in &left_seats {
                actions.extend(live.floor.removed(uid, &seats));
            }
            let epoch = live.epoch;
            self.save(&st);
            (actions, epoch)
        };
        // A guy that left stops what it was doing for the room.
        if let Some(bots) = self.bots() {
            for (_, gateway, seat) in left_seats {
                let call = bots.call(&gateway, "chat.abort", json!({ "sessionKey": seat }));
                tauri::async_runtime::spawn(async move {
                    let _ = call.await;
                });
            }
        }
        self.perform(&id, epoch, actions);
        self.emit("agents.changed", json!({ "agentId": id }));
        Ok(json!({ "ok": true }))
    }

    fn delete(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.abort(&id)?;
        {
            let mut st = self.inner.st.lock().unwrap();
            st.rooms.retain(|r| r.id != id);
            st.lines.remove(&id);
            st.live.remove(&id);
            self.save(&st);
        }
        let _ = std::fs::remove_file(self.inner.dir.join(format!("{id}.jsonl")));
        self.emit("agents.changed", json!({ "agentId": id }));
        Ok(json!({ "ok": true }))
    }

    // ---- the conversation ---------------------------------------------------------------

    /// You wrote in a room.
    fn send(&self, params: &Value) -> Result<Value, RpcError> {
        let id = room_of_key(params)?;
        let text = params.get("message").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
        if params.get("attachments").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            return Err(invalid("Rooms can't take attachments yet."));
        }
        if text.is_empty() {
            return Err(invalid("Write something first."));
        }
        let send_id = params.get("idempotencyKey").and_then(Value::as_str).map(str::to_string);
        let (line, run, opened, actions, owed, epoch) = {
            let mut st = self.inner.st.lock().unwrap();
            let St { rooms, lines, live, .. } = &mut *st;
            let room = rooms.iter_mut().find(|r| r.id == id).ok_or_else(no_room)?;
            let whos = whos(room);
            let mut to: Vec<String> = match params.get("to") {
                Some(Value::String(all)) if all == "everyone" => room.members.iter().map(|m| m.uid.clone()).collect(),
                Some(Value::Array(uids)) => uids.iter().filter_map(Value::as_str).filter(|u| room.members.iter().any(|m| m.uid == *u)).map(str::to_string).collect(),
                _ => Vec::new(),
            };
            for uid in talk::mentions(&text, &whos, None) {
                if !to.contains(&uid) {
                    to.push(uid);
                }
            }
            let mut why = Why::Addressed;
            if to.is_empty() {
                // To whoever spoke last (you're answering them), else to the lead.
                let last = lines.get(&id).and_then(|l| l.iter().rev().filter(|l| !l.note).find_map(|l| l.from.clone())).filter(|u| room.members.iter().any(|m| m.uid == *u));
                if last.is_none() {
                    why = Why::Lead;
                }
                to.push(last.unwrap_or_else(|| room.lead.clone()));
            }
            let line = Line { id: uuid::Uuid::new_v4().to_string(), at: now_ms(), from: None, text, to: to.clone(), note: false, client_send_id: send_id.clone() };
            room.flags.unread = false;
            room.flags.last_read_at = Some(line.at);
            room.updated_at = line.at;
            let live = live.entry(id.clone()).or_default();
            let opened = !live.floor.is_open();
            if opened {
                live.run = Some(uuid::Uuid::new_v4().to_string());
            }
            let actions = live.floor.human(&to, why, &seats(room));
            let out = (line.clone(), live.run.clone().unwrap_or_default(), opened, actions, live.floor.owed_while_running(), live.epoch);
            self.append(&mut st, &id, line);
            self.save(&st);
            out
        };
        let key = room_key(&id);
        self.emit("session.message", json!({ "sessionKey": key, "runId": run, "clientSendId": send_id, "message": message(&line) }));
        if opened {
            self.emit("chat", json!({ "sessionKey": key, "runId": run, "state": "status", "phase": "room" }));
        }
        self.perform(&id, epoch, actions);
        // A guy that's mid-turn takes it now if it can; else it's owed the turn.
        for uid in owed {
            let this = self.clone();
            let id = id.clone();
            tauri::async_runtime::spawn(async move { this.nudge(&id, &uid, epoch).await });
        }
        Ok(json!({ "runId": run, "status": "started", "clientSendId": send_id }))
    }

    /// "Let them continue", after a pause.
    fn resume(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let (actions, run, epoch) = {
            let mut st = self.inner.st.lock().unwrap();
            let St { rooms, live, .. } = &mut *st;
            let room = rooms.iter().find(|r| r.id == id).ok_or_else(no_room)?;
            let live = live.entry(id.clone()).or_default();
            let actions = live.floor.resume(&seats(room));
            if live.floor.is_open() && live.run.is_none() {
                live.run = Some(uuid::Uuid::new_v4().to_string());
            }
            (actions, live.run.clone(), live.epoch)
        };
        if let Some(run) = run.filter(|_| !actions.is_empty()) {
            self.emit("chat", json!({ "sessionKey": room_key(&id), "runId": run, "state": "status", "phase": "room" }));
        }
        self.perform(&id, epoch, actions);
        Ok(json!({ "ok": true }))
    }

    /// You stopped the room: every turn under way in it is stopped too.
    fn abort(&self, id: &str) -> Result<Value, RpcError> {
        let (run, stop) = {
            let mut st = self.inner.st.lock().unwrap();
            let St { rooms, live, waiters, .. } = &mut *st;
            let room = rooms.iter().find(|r| r.id == id).ok_or_else(no_room)?;
            let live = live.entry(id.to_string()).or_default();
            let mut stop = Vec::new();
            for wake in live.floor.running() {
                let member = room.members.iter().find(|m| m.uid == wake.uid);
                if let Some((gateway, seat)) = member.and_then(|m| Some((split_uid(&m.uid)?.0.to_string(), m.seat.clone()?))) {
                    waiters.remove(&(gateway.clone(), seat.clone()));
                    stop.push((gateway, seat));
                }
            }
            live.floor.abort();
            live.epoch += 1;
            (live.run.take(), stop)
        };
        if let Some(bots) = self.bots() {
            for (gateway, seat) in stop {
                let call = bots.call(&gateway, "chat.abort", json!({ "sessionKey": seat }));
                tauri::async_runtime::spawn(async move {
                    let _ = call.await;
                });
            }
        }
        if let Some(run) = run {
            self.emit("chat", json!({ "sessionKey": room_key(id), "runId": run, "state": "aborted" }));
        }
        self.emit_state(id);
        Ok(json!({ "ok": true, "aborted": true }))
    }

    /// Does what the floor decided: starts turns, and closes the round when the room is quiet.
    fn perform(&self, id: &str, epoch: u64, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Run(wake) => {
                    let this = self.clone();
                    let id = id.to_string();
                    tauri::async_runtime::spawn(async move { this.turn(id, wake, epoch).await });
                }
                Action::Quiet | Action::Pause => {
                    let run = self.inner.st.lock().unwrap().live.get_mut(id).and_then(|l| l.run.take());
                    if let Some(run) = run {
                        self.emit("chat", json!({ "sessionKey": room_key(id), "runId": run, "state": "final" }));
                    }
                    if action == Action::Pause {
                        // The roster carries `paused`, for windows that open later.
                        self.emit("agents.changed", json!({ "agentId": id }));
                    }
                }
            }
        }
        self.emit_state(id);
    }

    fn emit_state(&self, id: &str) {
        let state = state_of(&self.inner.st.lock().unwrap(), id);
        self.emit("room.state", state);
    }

    /// One guy's turn, start to end, and what the floor makes of its answer.
    async fn turn(self, id: String, wake: Wake, epoch: u64) {
        let outcome = self.seat_turn(&id, &wake, epoch).await;
        let mut posted = None;
        let (actions, run) = {
            let mut st = self.inner.st.lock().unwrap();
            let St { rooms, live, .. } = &mut *st;
            let (Some(room), Some(live)) = (rooms.iter_mut().find(|r| r.id == id), live.get_mut(&id).filter(|l| l.epoch == epoch)) else { return };
            let mut line = Line { id: uuid::Uuid::new_v4().to_string(), at: now_ms(), from: None, text: String::new(), to: Vec::new(), note: false, client_send_id: None };
            let mentions = match outcome {
                Ok(Some(text)) => {
                    line.to = talk::mentions(&text, &whos(room), Some(&wake.uid));
                    line.from = Some(wake.uid.clone());
                    line.text = text;
                    room.flags.unread = true;
                    room.updated_at = line.at;
                    posted = Some(line.clone());
                    Some(line.to.clone())
                }
                Ok(None) => None,
                // The room or the member went away meanwhile: nothing to say about it.
                Err(note) if note.is_empty() => None,
                Err(note) => {
                    line.note = true;
                    line.text = note;
                    posted = Some(line.clone());
                    None
                }
            };
            let actions = live.floor.replied(&wake.uid, mentions.as_deref(), &seats(room), MAX_HOPS);
            let run = live.run.clone();
            if let Some(line) = posted.clone() {
                self.append(&mut st, &id, line);
                self.save(&st);
            }
            (actions, run)
        };
        if let Some(line) = posted {
            self.emit("session.message", json!({ "sessionKey": room_key(&id), "runId": run, "message": message(&line) }));
        }
        self.perform(&id, epoch, actions);
    }

    /// Gives a guy its turn in its seat and waits for what it says. `Ok(None)`: it stayed
    /// silent. `Err`: it couldn't take the turn, with what the room says about that.
    async fn seat_turn(&self, id: &str, wake: &Wake, epoch: u64) -> Result<Option<String>, String> {
        let bots = self.bots().ok_or("Rooms aren't ready yet.")?;
        let (gateway, agent) = split_uid(&wake.uid).ok_or("That guy is gone.")?;
        let (name, room_name, seat, folder) = {
            let st = self.inner.st.lock().unwrap();
            let room = st.rooms.iter().find(|r| r.id == id).ok_or("")?;
            let m = room.members.iter().find(|m| m.uid == wake.uid).ok_or("")?;
            (m.name.clone(), room.name.clone(), m.seat.clone(), m.folder.clone())
        };
        if !bots.online(gateway) {
            return Err(format!("{name} is offline, so it missed this."));
        }
        let seat = match seat {
            Some(seat) => seat,
            None => {
                let label = format!("{room_name} (room)");
                let made = match bots.call(gateway, "sessions.create", json!({ "agentId": agent, "label": label, "idempotencyKey": uuid::Uuid::new_v4().to_string() })).await {
                    Ok(made) => Ok(made),
                    // A bot that wants thread names unique, or doesn't take one.
                    Err(_) => bots.call(gateway, "sessions.create", json!({ "agentId": agent, "idempotencyKey": uuid::Uuid::new_v4().to_string() })).await,
                };
                let key = made.ok().and_then(|m| m.get("key").and_then(Value::as_str).map(str::to_string)).ok_or(format!("{name} couldn't join: its bot wouldn't start a thread."))?;
                {
                    let mut st = self.inner.st.lock().unwrap();
                    if let Some(m) = st.rooms.iter_mut().find(|r| r.id == id).and_then(|r| r.members.iter_mut().find(|m| m.uid == wake.uid)) {
                        m.seat = Some(key.clone());
                    }
                    self.save(&st);
                }
                // The roster marks the thread as this room's, so it stays out of the guy's own list.
                self.emit("agents.changed", json!({ "agentId": id }));
                key
            }
        };
        let marker = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let (text, upto) = {
            let st = self.inner.st.lock().unwrap();
            let room = st.rooms.iter().find(|r| r.id == id).ok_or("")?;
            let m = room.members.iter().find(|m| m.uid == wake.uid).ok_or("")?;
            let lines = st.lines.get(id).map(Vec::as_slice).unwrap_or_default();
            let (said, skipped) = unseen(room, lines, m);
            let rules = (!m.briefed).then(|| talk::rules(&room.name, &whos(room), &room.lead, &m.uid));
            // Whoever addressed it last, for "Scout addressed you".
            let by = lines.iter().rev().find(|l| l.to.contains(&m.uid)).and_then(|l| l.from.as_deref()).map(|uid| name_of(room, uid));
            (talk::envelope(&room.name, &marker, rules.as_deref(), &said, skipped, wake.why, by), lines.len())
        };
        let (waiter, mut ended) = self.wait_for(gateway, &seat);
        let mut send = json!({ "sessionKey": seat, "message": text, "idempotencyKey": uuid::Uuid::new_v4().to_string() });
        if let Some(folder) = folder {
            // Where its thread starts (bots that take one ignore it once the thread has).
            send["cwd"] = json!(folder);
        }
        let sent = bots.call(gateway, "chat.send", send).await;
        let sent = match sent {
            Ok(sent) => sent,
            Err(e) => {
                self.stop_waiting(gateway, &seat, waiter);
                return Err(format!("{name} couldn't take its turn: {}", e.message));
            }
        };
        {
            let mut st = self.inner.st.lock().unwrap();
            if let Some(m) = st.rooms.iter_mut().find(|r| r.id == id).and_then(|r| r.members.iter_mut().find(|m| m.uid == wake.uid)) {
                m.seen = m.seen.max(upto);
                m.briefed = true;
            }
            self.save(&st);
        }
        if wake.why != Why::Look {
            self.look_if_long(id, &wake.uid, epoch);
        }
        let run = sent.get("runId").and_then(Value::as_str).map(str::to_string);
        let end = loop {
            // Dropped: you stopped the room.
            let Some(event) = ended.recv().await else { return Ok(None) };
            if !matches!(event.phase, RunPhase::End | RunPhase::Final | RunPhase::Error | RunPhase::Aborted) {
                continue;
            }
            if run.is_some() && event.run == run {
                break event;
            }
            // Not every bot names its turn the way its receipt did: ask whether it's over.
            tokio::time::sleep(SETTLE).await;
            if !bots.is_running(gateway, &seat) {
                break event;
            }
        };
        self.stop_waiting(gateway, &seat, waiter);
        match end.phase {
            RunPhase::Aborted => return Ok(None),
            RunPhase::Error => {
                if let Some(folder) = end.untrusted {
                    return Err(format!("{name} can't start in {folder} until you trust that folder. Open {name}'s side to trust it, then ask again."));
                }
                return Err(match end.error {
                    Some(error) => format!("{name} hit an error: {}", truncate(&error, 200)),
                    None => format!("{name} hit an error."),
                })
            }
            _ => {}
        }
        let mut reply = end.text;
        for attempt in 0..REPLY_TRIES {
            if reply.is_some() {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(REPLY_RETRY).await;
            }
            let history = bots.call(gateway, "chat.history", json!({ "sessionKey": seat, "limit": 60 })).await;
            let messages = history.ok().and_then(|h| h.get("messages").and_then(Value::as_array).cloned()).unwrap_or_default();
            reply = talk::reply_in(&messages, &marker);
        }
        let reply = talk::clean(&reply.unwrap_or_default());
        Ok((!talk::is_silent(&reply)).then_some(reply))
    }

    /// Sends a working guy what you just wrote, into its turn, where its bot can take it.
    async fn nudge(&self, id: &str, uid: &str, epoch: u64) {
        let Some(bots) = self.bots() else { return };
        let Some((gateway, _)) = split_uid(uid) else { return };
        if !bots.steers(gateway) {
            return;
        }
        let Some((seat, text, upto)) = ({
            let st = self.inner.st.lock().unwrap();
            let room = st.rooms.iter().find(|r| r.id == id);
            room.and_then(|room| {
                let m = room.members.iter().find(|m| m.uid == uid)?;
                let lines = st.lines.get(id).map(Vec::as_slice).unwrap_or_default();
                let (said, _) = unseen(room, lines, m);
                let marker = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
                (!said.is_empty()).then(|| (m.seat.clone(), talk::nudge(&room.name, &marker, &said), lines.len()))
            })
        }) else {
            return;
        };
        let Some(seat) = seat else { return };
        let sent = bots.call(gateway, "chat.send", json!({ "sessionKey": seat, "message": text, "idempotencyKey": uuid::Uuid::new_v4().to_string(), "delivery": "steer" })).await;
        if sent.is_err() {
            return;
        }
        let mut st = self.inner.st.lock().unwrap();
        let St { rooms, live, .. } = &mut *st;
        if let Some(live) = live.get_mut(id).filter(|l| l.epoch == epoch) {
            live.floor.steered(uid);
        }
        if let Some(m) = rooms.iter_mut().find(|r| r.id == id).and_then(|r| r.members.iter_mut().find(|m| m.uid == uid)) {
            m.seen = m.seen.max(upto);
        }
        self.save(&st);
    }

    /// A turn that runs long doesn't keep the listeners waiting.
    fn look_if_long(&self, id: &str, uid: &str, epoch: u64) {
        let (this, id, uid, after) = (self.clone(), id.to_string(), uid.to_string(), self.inner.look_after);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(after).await;
            let actions = {
                let mut st = this.inner.st.lock().unwrap();
                let St { rooms, live, .. } = &mut *st;
                let (Some(room), Some(live)) = (rooms.iter().find(|r| r.id == id), live.get_mut(&id).filter(|l| l.epoch == epoch)) else { return };
                if !live.floor.running().iter().any(|w| w.uid == uid && w.why != Why::Look) {
                    return;
                }
                live.floor.waited(&seats(room))
            };
            if !actions.is_empty() {
                this.perform(&id, epoch, actions);
            }
        });
    }

    fn wait_for(&self, gateway: &str, seat: &str) -> (u64, mpsc::UnboundedReceiver<SeatEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.inner.next_waiter.fetch_add(1, Ordering::Relaxed);
        self.inner.st.lock().unwrap().waiters.entry((gateway.to_string(), seat.to_string())).or_default().push((id, tx));
        (id, rx)
    }

    fn stop_waiting(&self, gateway: &str, seat: &str, waiter: u64) {
        let mut st = self.inner.st.lock().unwrap();
        let key = (gateway.to_string(), seat.to_string());
        if let Some(list) = st.waiters.get_mut(&key) {
            list.retain(|(id, _)| *id != waiter);
            if list.is_empty() {
                st.waiters.remove(&key);
            }
        }
    }
}

/// What a member hasn't been shown yet (its own lines and the room's notes aside): the
/// newest [`talk::HISTORY_LIMIT`], and how many older ones that leaves out.
fn unseen<'a>(room: &'a Room, lines: &'a [Line], m: &Member) -> (Vec<Said<'a>>, usize) {
    let new: Vec<&Line> = lines.iter().skip(m.seen).filter(|l| !l.note && l.from.as_deref() != Some(m.uid.as_str())).collect();
    let skipped = new.len().saturating_sub(talk::HISTORY_LIMIT);
    let said = new[skipped..].iter().map(|l| Said { from: l.from.as_deref().map(|uid| name_of(room, uid)), text: &l.text }).collect();
    (said, skipped)
}

fn state_of(st: &St, id: &str) -> Value {
    let live = st.live.get(id);
    let working: Vec<Value> = live.map(|l| l.floor.running()).unwrap_or_default().iter().map(|w| json!({ "uid": w.uid, "why": w.why.as_str() })).collect();
    json!({ "sessionKey": room_key(id), "id": id, "working": working, "paused": live.is_some_and(|l| l.floor.is_paused()) })
}

fn read_lines(dir: &std::path::Path, id: &str) -> Vec<Line> {
    let Ok(raw) = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))) else { return Vec::new() };
    raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests;
