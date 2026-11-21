//! The roster: which little guys exist on which gateway, and what they're up
//! to. Kept in Rust (fed by every gateway's events) so the menu-bar sprite and
//! notifications stay live while every window is hidden; webviews receive it
//! as `lg:roster`.

mod avatars;
mod bridge;
mod notifications;
mod preferences;
mod presentation;
mod state;
mod turns;
pub(crate) use avatars::is_face;
use avatars::{apply_faces, mark_faces, AvatarStore};
use preferences::Preferences;
pub use preferences::{Notifications, NotifyPrefs, RoutineNotifications};
use state::*;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::bridge::NoticeKind;
use crate::gateway::client::{ConnStatus, Phase, RpcError};
use crate::gateway::identity::now_ms;
use crate::outbox;
use crate::runtime::registry::Registry;
use crate::runtime::Runtime;
use crate::tray::{Mood, TrayAnimator};
use crate::windows::Navigate;

/// One gateway's slice of the roster.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRoster {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub status: ConnStatus,
    pub loaded: bool,
    pub default_agent_id: Option<String>,
    /// `AgentSummary` rows from `agents.list`.
    pub agents: Vec<Value>,
    /// `SessionRow`s across all agents from `sessions.list`.
    pub sessions: Vec<Value>,
    /// Session keys with a run in flight.
    pub running: Vec<String>,
    /// Pending exec approvals (`exec.approval.requested` payloads).
    pub approvals: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    pub gateways: Vec<GatewayRoster>,
    /// Guys the user hid (`gatewayId/agentId`): left out of lists, the tray
    /// mood and reply/routine notifications (approvals still notify).
    pub hidden: Vec<String>,
    /// Guys whose threads show their steps ("Ran 3 commands…"), as `gatewayId/agentId`
    /// (Settings → Agents). Off by default: a thread shows its messages.
    pub steps: Vec<String>,
    /// What notifies you, for everyone and per guy (Settings → Notifications).
    pub notifications: Notifications,
    /// Guys with a photo picked in littleguys (`gatewayId/agentId`). It's kept on
    /// this Mac, never sent to the bot, and is already in their `identity.avatarUrl`.
    pub faces: Vec<String>,
    /// Settings → Coder mode (code.rs).
    pub coder: crate::code::CoderPrefs,
    pub updated_at: u64,
}

pub struct Activity {
    app: AppHandle,
    preferences: Preferences,
    avatars: AvatarStore,
    clients: Registry<Runtime>,
    tray: TrayAnimator,
    state: Mutex<State>,
    publish_pending: AtomicBool,
    /// Statuses, events and bot-list changes for paired phones (the bridge).
    bridge: tokio::sync::broadcast::Sender<crate::bridge::HostEvent>,
}

const SESSION_LIST_LIMIT: u64 = 400;
/// How long a turn's end settles before what was queued goes (a runtime may still be
/// letting go of the turn, or a terminal prompt may start the next one).
const QUEUE_SETTLE: Duration = Duration::from_millis(400);
const QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(120);

async fn forward(client: &Runtime, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, RpcError> {
    match timeout {
        Some(timeout) => client.call_with_timeout(method, params, timeout).await,
        None => client.call(method, params).await,
    }
}

impl Activity {
    pub fn new(app: AppHandle, tray: TrayAnimator) -> Arc<Self> {
        let data_dir = app.path().app_data_dir().ok();
        let preferences = Preferences::new(data_dir.as_deref());
        crate::code::set_prefs(preferences.snapshot().coder);
        let avatars = AvatarStore::new(data_dir.as_deref());
        Arc::new(Self {
            app,
            preferences,
            avatars,
            clients: Registry::default(),
            tray,
            state: Mutex::new(State::default()),
            publish_pending: AtomicBool::new(false),
            bridge: tokio::sync::broadcast::channel(4096).0,
        })
    }

    /// Everything a paired phone hears about (bridge/app_host.rs).
    pub fn subscribe_bridge(&self) -> tokio::sync::broadcast::Receiver<crate::bridge::HostEvent> {
        self.bridge.subscribe()
    }

    /// Show or hide a guy (`gatewayId/agentId`); persisted in the app's prefs.
    pub fn set_guy_hidden(&self, uid: &str, hidden: bool) -> anyhow::Result<()> {
        self.preferences.set_hidden(uid, hidden)?;
        self.publish();
        Ok(())
    }

    /// Show or hide a guy's steps in their threads; persisted in the app's prefs.
    pub fn set_guy_steps(&self, uid: &str, shown: bool) -> anyhow::Result<()> {
        self.preferences.set_steps(uid, shown)?;
        self.publish();
        Ok(())
    }

    pub fn set_notifications(&self, uid: Option<&str>, prefs: NotifyPrefs) -> anyhow::Result<()> {
        self.preferences.set_notifications(uid, prefs)?;
        self.publish();
        Ok(())
    }

    /// Settings → Coder mode: saved, in force at once, and sent to every window and phone.
    pub fn set_coder_prefs(&self, coder: crate::code::CoderPrefs) -> anyhow::Result<()> {
        self.preferences.set_coder(coder.clone())?;
        crate::code::set_prefs(coder);
        self.publish();
        Ok(())
    }

    /// Give a guy (`gatewayId/agentId`) a photo, or take it away with `None`. It's
    /// only stored in littleguys' data dir: bots never see it.
    pub fn set_guy_face(&self, uid: &str, face: Option<String>) -> anyhow::Result<()> {
        self.avatars.set(uid, face)?;
        self.publish();
        // Paired phones keep their own rosters: tell them to look at this bot's guys again.
        if let Some((gateway, agent_id)) = uid.split_once('/') {
            let _ = self.bridge.send(crate::bridge::HostEvent::Event {
                gateway: gateway.to_string(),
                name: "agents.changed".to_string(),
                payload: json!({ "agentId": agent_id }),
            });
        }
        Ok(())
    }

    /// A bot's `agents.list` rows as a paired phone gets them: with the photos
    /// picked in littleguys, each marked `littleguysFace` (a phone can take it away).
    pub fn faces_for_phone(&self, gateway: &str, agents: &mut [Value]) {
        let faces = self.avatars.snapshot();
        apply_faces(agents, gateway, &faces);
        mark_faces(agents, gateway, &faces);
    }

    /// The workspace folder of a guy whose gateway runs on this Mac (from `agents.list`).
    pub fn local_workspace(&self, gateway: &str, agent_id: &str) -> Option<std::path::PathBuf> {
        let state = self.state.lock().unwrap();
        let g = state.gateways.iter().find(|g| g.roster.id == gateway)?;
        if g.roster.status.mode != Some(crate::openclaw::config::GatewayMode::Local) {
            return None;
        }
        let agent = g.roster.agents.iter().find(|a| a.get("id").and_then(Value::as_str) == Some(agent_id))?;
        let path = std::path::PathBuf::from(agent.get("workspace")?.as_str()?);
        (path.is_absolute() && path.is_dir()).then_some(path)
    }

    fn is_hidden(&self, gateway: &str, agent_id: &str) -> bool {
        self.preferences.snapshot().hidden_guys.contains(&format!("{gateway}/{agent_id}"))
    }

    fn notify_prefs(&self, gateway: &str, agent_id: &str) -> NotifyPrefs {
        self.preferences.snapshot().notifications.for_guy(&format!("{gateway}/{agent_id}"))
    }

    // ---- gateway lifecycle (driven by the hub) ---------------------------------

    pub fn attach(&self, generation: u64, client: Runtime, name: String, enabled: bool) {
        let id = client.id().to_string();
        let status = client.status();
        let is_openclaw = client.as_openclaw().is_some();
        self.clients.insert(id.clone(), generation, client);
        let mut state = self.state.lock().unwrap();
        let replacement = GatewayState {
            generation,
            roster: GatewayRoster { id, name, enabled, status, ..Default::default() },
            is_openclaw,
            ..Default::default()
        };
        if let Some(old) = state.gateway(&replacement.roster.id) {
            *old = replacement;
        } else {
            state.gateways.push(replacement);
        }
        drop(state);
        let _ = self.bridge.send(crate::bridge::HostEvent::BotsChanged);
    }

    pub fn detach(&self, id: &str) {
        self.clients.remove(id);
        outbox::forget(id);
        self.state.lock().unwrap().gateways.retain(|g| g.roster.id != id);
        self.publish();
        let _ = self.bridge.send(crate::bridge::HostEvent::BotsChanged);
    }

    pub fn set_gateway_meta(&self, id: &str, name: Option<String>, enabled: Option<bool>) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(g) = state.gateway(id) {
                if let Some(name) = name {
                    g.roster.name = name;
                }
                if let Some(enabled) = enabled {
                    g.roster.enabled = enabled;
                    if !enabled {
                        g.roster.agents.clear();
                        g.roster.sessions.clear();
                        g.disconnect();
                        g.live_previews.clear();
                        g.watched.clear();
                        g.roster.loaded = false;
                    }
                }
            }
        }
        self.publish();
        let _ = self.bridge.send(crate::bridge::HostEvent::BotsChanged);
    }

    pub fn next_generation(&self) -> u64 {
        self.clients.next_generation()
    }
    pub fn clients(&self) -> Vec<Runtime> {
        self.clients.all()
    }
    pub fn client(&self, id: &str) -> Option<Runtime> {
        self.clients.get(id).map(|(_, client)| client)
    }

    pub fn roster(&self) -> Roster {
        self.snapshot()
    }

    pub fn set_focused_session(&self, gateway: Option<String>, key: Option<String>) {
        self.state.lock().unwrap().focused = gateway.zip(key);
    }

    fn gateway_count(&self) -> usize {
        self.state.lock().unwrap().gateways.iter().filter(|g| g.roster.enabled).count()
    }

    // ---- gateway hooks -----------------------------------------------------------

    pub fn on_status(&self, status: &ConnStatus, generation: u64) {
        let Some(open) = self.state.lock().unwrap().status(status, generation) else { return };
        self.broadcast_status(status);
        self.publish();
        if open {
            crate::windows::open_main(&self.app, None);
        }
    }

    pub fn on_connected(self: &Arc<Self>, gateway: &str, generation: u64) {
        let connection = {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway).filter(|g| g.generation == generation && g.roster.enabled) else { return };
            g.disconnect();
            g.connection()
        };
        let this = self.clone();
        let gateway = gateway.to_string();
        tauri::async_runtime::spawn(async move {
            let Some((_, client)) = this.clients.get(&gateway).filter(|(gen, _)| *gen == generation) else { return };
            if let Err(e) = client.call("sessions.subscribe", json!({})).await {
                log::warn!("sessions.subscribe failed: {e}");
            }
            if let Ok(result) = client.call("exec.approval.list", json!({})).await {
                let pending =
                    result.get("approvals").or_else(|| result.get("requests")).and_then(Value::as_array).cloned().unwrap_or_default();
                let mut state = this.state.lock().unwrap();
                if let Some(g) = state.current(&gateway, connection) {
                    g.approval_snapshot(connection, pending);
                }
            }
            if this.state.lock().unwrap().current(&gateway, connection).is_none() {
                return;
            }
            this.refresh(&gateway).await;
        });
    }

    pub fn on_event(self: &Arc<Self>, gateway: &str, generation: u64, name: &str, payload: &Value) {
        let (connection, effects) = {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway).filter(|g| g.generation == generation && g.roster.enabled) else { return };
            (g.connection(), g.event(name, payload))
        };
        self.broadcast_event(gateway, name, payload);
        for effect in effects {
            if self.state.lock().unwrap().current(gateway, connection).is_none() {
                return;
            }
            match effect {
                Effect::Publish => self.publish(),
                Effect::PublishSoon => self.publish_soon(),
                Effect::Refresh => self.schedule_refresh(gateway),
                Effect::Reply(key) => self.notify_reply_later(gateway.to_string(), key, connection),
                Effect::Approval(payload) => self.notify_approval(gateway, &payload),
                Effect::ApprovalResolved(id) => self.push_to_phones(NoticeKind::ApprovalResolved, "", "", gateway, None, Some(id)),
                Effect::Routine(payload) => self.notify_routine(gateway, &payload),
                // The turn's over: what you wrote meanwhile goes now (a stopped one keeps it).
                Effect::TurnOver(key) => {
                    if !outbox::is_empty(gateway, &key) {
                        self.send_queued_soon(gateway.to_string(), key);
                    }
                }
            }
        }
    }

    // ---- queued messages (outbox.rs) -----------------------------------------------

    /// Whether this thread's turn is under way, as far as its events tell.
    fn is_running(&self, gateway: &str, key: &str) -> bool {
        self.state.lock().unwrap().gateway(gateway).is_some_and(|g| g.running.contains(key))
    }

    /// A webview's `gw_call`. `chat.send` with `delivery: "queue"` waits in littleguys while
    /// the thread works (and goes at once when it doesn't); `"steer"` asks the guy to take it
    /// into the turn under way. `chat.queue.send {sessionKey, id, delivery?}` sends one now,
    /// `chat.queue.remove {sessionKey, id}` takes one back, and `chat.history` lists what's
    /// waiting (`queued`).
    pub async fn call(self: &Arc<Self>, client: &Runtime, method: &str, mut params: Value, timeout: Option<Duration>) -> Result<Value, RpcError> {
        let gateway = client.id().to_string();
        let Some(key) = params.get("sessionKey").and_then(Value::as_str).map(str::to_string) else {
            return forward(client, method, params, timeout).await;
        };
        match method {
            "chat.send" => {
                let delivery = params.as_object_mut().and_then(|p| p.remove("delivery"));
                let delivery = delivery.as_ref().and_then(Value::as_str);
                if delivery == Some("queue") && self.is_running(&gateway, &key) {
                    let text = params.get("message").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                    let attachments = params.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default();
                    if text.is_empty() && attachments.is_empty() {
                        return Err(RpcError { code: "INVALID_REQUEST".into(), message: "Write something first.".into(), details: None });
                    }
                    let held = outbox::hold(&gateway, &key, text, attachments);
                    self.broadcast_event(&gateway, "chat.queue", &outbox::event(&gateway, &key));
                    return Ok(json!({ "runId": held.id, "status": "held" }));
                }
                // OpenClaw's gateway refuses fields it doesn't know, and steers by default.
                if delivery == Some("steer") && client.as_openclaw().is_none() {
                    params["delivery"] = json!("steer");
                }
                forward(client, method, params, timeout).await
            }
            "chat.queue.send" => {
                // One you queued, now: into the turn under way, if the guy can (`delivery`).
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                let held = outbox::remove(&gateway, &key, id)
                    .ok_or_else(|| RpcError { code: "NOT_FOUND".into(), message: "It's already been sent.".into(), details: None })?;
                let mut send = json!({ "sessionKey": key, "message": held.text, "idempotencyKey": held.id });
                if !held.attachments.is_empty() {
                    send["attachments"] = Value::Array(held.attachments.clone());
                }
                if params.get("delivery").and_then(Value::as_str) == Some("steer") && client.as_openclaw().is_none() {
                    send["delivery"] = json!("steer");
                }
                self.broadcast_event(&gateway, "chat.queue", &outbox::sending(&gateway, &key, &held.id, &held.text));
                let sent = forward(client, "chat.send", send, timeout).await;
                match &sent {
                    Ok(receipt) => self.broadcast_event(&gateway, "chat.queue", &outbox::accepted(&gateway, &key, &held.id, receipt)),
                    Err(_) => {
                        let id = held.id.clone();
                        outbox::restore(&gateway, &key, vec![held]);
                        self.broadcast_event(&gateway, "chat.queue", &outbox::unsent(&gateway, &key, &id, None));
                    }
                }
                sent
            }
            "chat.queue.remove" => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                let removed = outbox::remove(&gateway, &key, id).is_some();
                self.broadcast_event(&gateway, "chat.queue", &outbox::event(&gateway, &key));
                Ok(json!({ "ok": true, "removed": removed }))
            }
            "chat.history" => {
                let mut res = forward(client, method, params, timeout).await?;
                if let Some(obj) = res.as_object_mut() {
                    obj.insert("queued".into(), outbox::shown(&outbox::list(&gateway, &key)));
                }
                Ok(res)
            }
            _ => forward(client, method, params, timeout).await,
        }
    }

    /// Hands a thread's queued messages over, as one, once its turn has settled. If another
    /// turn started meanwhile (say a prompt typed in its terminal), they wait for that one.
    fn send_queued_soon(self: &Arc<Self>, gateway: String, key: String) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(QUEUE_SETTLE).await;
            if this.is_running(&gateway, &key) {
                return;
            }
            let Some(client) = this.client(&gateway) else { return };
            let items = outbox::take(&gateway, &key);
            if items.is_empty() {
                return;
            }
            let (text, attachments) = outbox::combine(&items);
            let id = uuid::Uuid::new_v4().to_string();
            let mut params = json!({ "sessionKey": key, "message": text, "idempotencyKey": id });
            if !attachments.is_empty() {
                params["attachments"] = Value::Array(attachments);
            }
            // Shown as sending first: the guy may put it in the thread before chat.send answers.
            this.broadcast_event(&gateway, "chat.queue", &outbox::sending(&gateway, &key, &id, &text));
            match client.call_with_timeout("chat.send", params, QUEUE_SEND_TIMEOUT).await {
                Ok(receipt) => this.broadcast_event(&gateway, "chat.queue", &outbox::accepted(&gateway, &key, &id, &receipt)),
                Err(e) => {
                    log::warn!("[{gateway}] couldn't send the queued messages for {key}: {}", e.message);
                    outbox::restore(&gateway, &key, items);
                    let error = format!("Couldn't send what was queued: {}", e.message);
                    this.broadcast_event(&gateway, "chat.queue", &outbox::unsent(&gateway, &key, &id, Some(&error)));
                }
            }
        });
    }

    // ---- roster refresh ------------------------------------------------------------

    fn schedule_refresh(self: &Arc<Self>, gateway: &str) {
        let connection = {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway) else { return };
            if g.refresh_scheduled {
                return;
            }
            g.refresh_scheduled = true;
            g.connection()
        };
        let this = self.clone();
        let gateway = gateway.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            {
                let mut state = this.state.lock().unwrap();
                let Some(g) = state.current(&gateway, connection) else { return };
                g.refresh_scheduled = false;
            }
            this.refresh(&gateway).await;
        });
    }

    pub async fn refresh_all(self: &Arc<Self>) {
        let ids: Vec<String> = self.clients.all().iter().map(|c| c.id().to_string()).collect();
        futures_util::future::join_all(ids.iter().map(|id| self.refresh(id))).await;
    }

    pub async fn refresh(self: &Arc<Self>, gateway: &str) {
        let Some((generation, client)) = self.clients.get(gateway) else { return };
        if client.status().phase != Phase::Connected {
            return;
        }
        let (connection, ticket) = {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway).filter(|g| g.generation == generation && g.roster.enabled) else { return };
            (g.connection(), g.begin_refresh())
        };
        let (agents, sessions) = tokio::join!(
            client.call("agents.list", json!({})),
            client.call(
                "sessions.list",
                json!({
                    "limit": SESSION_LIST_LIMIT,
                    "includeDerivedTitles": true,
                    "includeLastMessage": true,
                    "configuredAgentsOnly": true,
                }),
            )
        );
        let watch = {
            let mut state = self.state.lock().unwrap();
            state.current(gateway, connection).and_then(|g| g.snapshot(ticket, agents.as_ref().ok(), sessions.as_ref().ok()))
        };
        let Some(watch) = watch else {
            // A newer request will publish its own snapshot; retry only for intervening events.
            let retry = self.state.lock().unwrap().current(gateway, connection).is_some_and(|g| g.needs_refresh(ticket));
            if retry {
                self.schedule_refresh(gateway);
            }
            return;
        };
        if let Err(e) = &agents {
            log::warn!("agents.list failed: {e}");
        }
        if let Err(e) = &sessions {
            log::warn!("sessions.list failed: {e}");
        }
        self.publish();
        for key in watch {
            if self.state.lock().unwrap().current(gateway, connection).is_none() {
                return;
            }
            if let Err(e) = client.call("sessions.messages.subscribe", json!({ "key": key })).await {
                log::warn!("sessions.messages.subscribe {key} failed: {e}");
                if let Some(g) = self.state.lock().unwrap().current(gateway, connection) {
                    g.watched.remove(&key);
                }
            }
        }
    }
}

/// "Claude Code needs your OK in shop": with every session its own thread, the folder says which.
fn approval_title(who: &str, payload: &Value) -> String {
    match payload.pointer("/presentation/folder").and_then(Value::as_str).filter(|f| !f.is_empty()) {
        Some(folder) => format!("{who} needs your OK in {folder}"),
        None => format!("{who} needs your OK"),
    }
}

fn approval_id(payload: &Value) -> Option<String> {
    payload
        .get("id")
        .or_else(|| payload.get("approvalId"))
        .or_else(|| payload.pointer("/request/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn agent_id_from_key(key: &str) -> Option<String> {
    let mut parts = key.split(':');
    (parts.next() == Some("agent")).then(|| parts.next().map(str::to_string)).flatten()
}

/// Threads worth a notification: the main DM, littleguys/dashboard threads.
/// Channel threads (Slack, Telegram…) already notify through their apps.
fn notifiable_session(key: &str) -> bool {
    key.ends_with(":main") || key.contains(":dashboard:")
}

/// Whether a finished routine run (a `cron` "finished" event) is worth a notification:
/// never when it had nothing to report (Hermes' [SILENT]), and with "failures" only
/// when it failed.
fn routine_run_notifies(payload: &Value, mode: RoutineNotifications) -> bool {
    if payload.get("silent").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match mode {
        RoutineNotifications::All => true,
        RoutineNotifications::Failing => payload.get("status").and_then(Value::as_str) == Some("error"),
        RoutineNotifications::Off => false,
    }
}

/// The conversation a routine run happens in, when it isn't its own
/// (`sessionTarget` "session:<key>" or "main"); isolated runs get `:cron:` keys.
fn routine_target(payload: &Value) -> Option<String> {
    let job = payload.get("job")?;
    let target = job.get("sessionTarget").and_then(Value::as_str)?;
    if let Some(key) = target.strip_prefix("session:") {
        return Some(key.to_string());
    }
    let agent = job.get("agentId").or_else(|| job.pointer("/owner/agentId")).and_then(Value::as_str)?;
    (target == "main").then(|| format!("agent:{agent}:main"))
}

fn is_notable_unread(row: &Value) -> bool {
    row.get("unread").and_then(Value::as_bool) == Some(true) && row.get("key").and_then(Value::as_str).is_some_and(notifiable_session)
}

pub fn agent_display_name(agent: &Value) -> String {
    let name = agent
        .pointer("/identity/name")
        .or_else(|| agent.get("name"))
        .or_else(|| agent.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("little guy");
    match agent.pointer("/identity/emoji").and_then(Value::as_str) {
        Some(emoji) if !emoji.is_empty() => format!("{emoji} {name}"),
        _ => name.to_string(),
    }
}

/// A chat message's text (`content` as a string or text blocks).
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// A turn's streamed text piles up every reply in it; the last paragraph is what was just said.
fn newest_paragraph(text: &str) -> Option<&str> {
    text.rsplit("\n\n").map(str::trim).find(|p| !p.is_empty())
}

/// Rows preview what the guy said last in a turn the transcript hasn't caught up with.
fn apply_live_previews(rows: &mut [Value], live: &HashMap<String, LivePreview>) {
    if live.is_empty() {
        return;
    }
    for row in rows {
        if let Some(text) = row.get("key").and_then(Value::as_str).and_then(|k| live.get(k)).map(|l| l.text.clone()) {
            row["lastMessagePreview"] = json!(text);
        }
    }
}

fn saved_preview(row: &Value) -> Option<&str> {
    row.get("lastMessagePreview").and_then(Value::as_str)
}

/// OpenClaw's own rule: a settled `status` wins, then `hasActiveRun`. A
/// stopped claude-cli turn can leave `status: "running"` behind for good.
fn run_active(row: &Value) -> bool {
    let status = row.get("status").and_then(Value::as_str);
    if status.is_some_and(|s| s != "queued" && s != "running") {
        return false;
    }
    row.get("hasActiveRun").and_then(Value::as_bool).unwrap_or(status.is_some())
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::avatars::MAX_FACE_CHARS;
    use super::preferences::Prefs;
    use super::*;

    #[test]
    fn approval_notifications_say_which_folder() {
        let shop = json!({ "presentation": { "commandText": "cargo test", "folder": "shop" } });
        assert_eq!(approval_title("Claude Code", &shop), "Claude Code needs your OK in shop");
        for elsewhere in [json!({ "presentation": { "folder": null } }), json!({ "presentation": { "folder": "" } }), json!({})] {
            assert_eq!(approval_title("Fox", &elsewhere), "Fox needs your OK");
        }
    }

    #[test]
    fn finds_the_conversation_a_routine_runs_in() {
        let job = |target: &str| json!({ "action": "started", "job": { "agentId": "moneynuggs", "sessionTarget": target } });
        assert_eq!(routine_target(&job("session:agent:moneynuggs:dashboard:abc")).as_deref(), Some("agent:moneynuggs:dashboard:abc"));
        assert_eq!(routine_target(&job("main")).as_deref(), Some("agent:moneynuggs:main"));
        assert_eq!(routine_target(&job("isolated")), None);
        assert_eq!(routine_target(&json!({ "action": "finished", "jobId": "x" })), None);
    }

    #[test]
    fn parses_agent_from_session_key() {
        assert_eq!(agent_id_from_key("agent:main:main").as_deref(), Some("main"));
        assert_eq!(agent_id_from_key("agent:fox:dashboard:123").as_deref(), Some("fox"));
        assert_eq!(agent_id_from_key("global"), None);
    }

    #[test]
    fn previews_the_newest_reply_of_a_running_turn() {
        let delta = json!({ "role": "assistant", "content": [{ "type": "text", "text": "On it. Checking the map first.\n\nFound the trail; packing snacks now.\n\n" }] });
        assert_eq!(newest_paragraph(&message_text(&delta)), Some("Found the trail; packing snacks now."));
        assert_eq!(newest_paragraph(" \n\n "), None);
        let tool_turn =
            json!({ "role": "assistant", "content": [{ "type": "toolCall", "name": "Bash" }, { "type": "text", "text": "Tests pass." }] });
        assert_eq!(message_text(&tool_turn), "Tests pass.");
        assert_eq!(message_text(&json!({ "role": "user", "content": "go for it" })), "go for it");
    }

    #[test]
    fn live_preview_lasts_until_the_transcript_catches_up() {
        let key = "agent:fox:dashboard:a1";
        let row = |preview: &str, status: &str, active: bool| json!({ "key": key, "lastMessagePreview": preview, "status": status, "hasActiveRun": active });
        let shown = |g: &GatewayState| {
            let mut rows = g.roster.sessions.clone();
            apply_live_previews(&mut rows, &g.live_previews);
            rows[0]["lastMessagePreview"].as_str().unwrap().to_string()
        };
        let mut g = GatewayState { is_openclaw: true, ..Default::default() };

        // A long claude-cli turn: the gateway keeps previewing your message.
        g.roster.sessions = vec![row("go for it", "running", true)];
        assert_eq!(g.track_row(&g.roster.sessions[0].clone()).as_deref(), Some(key));
        assert_eq!(g.track_row(&g.roster.sessions[0].clone()), None, "already watched");
        assert!(g.set_live_preview(key, "Found the trail; packing snacks now.".into()));
        assert!(!g.set_live_preview(key, "Found the trail; packing snacks now.".into()), "nothing new to publish");
        assert_eq!(shown(&g), "Found the trail; packing snacks now.");

        // Turn over, transcript not saved yet (or never, if it was stopped).
        g.running.clear();
        g.turns.disconnect();
        g.roster.sessions = vec![row("go for it", "running", false)];
        g.track_row(&g.roster.sessions[0].clone());
        assert!(g.running.is_empty(), "a stopped turn's leftover status isn't a run");
        assert_eq!(shown(&g), "Found the trail; packing snacks now.");

        // The transcript caught up: the gateway's preview wins again.
        g.roster.sessions = vec![row("Trail's mapped, snacks packed.", "done", false)];
        g.track_row(&g.roster.sessions[0].clone());
        assert!(g.live_previews.is_empty());
        assert_eq!(shown(&g), "Trail's mapped, snacks packed.");
    }

    #[test]
    fn reads_run_state_like_openclaw() {
        assert!(run_active(&json!({ "status": "running", "hasActiveRun": true })));
        assert!(run_active(&json!({ "status": "queued" })));
        assert!(run_active(&json!({ "hasActiveRun": true })));
        assert!(!run_active(&json!({ "status": "running", "hasActiveRun": false })));
        assert!(!run_active(&json!({ "status": "done", "hasActiveRun": true })));
        assert!(!run_active(&json!({})));
    }

    #[test]
    fn routines_notify_all_failures_or_never() {
        use RoutineNotifications::{All, Failing, Off};
        let run = |status: &str| json!({ "action": "finished", "status": status, "summary": "pending=2 new=0", "job": { "name": "parcel-watch" } });
        assert!(routine_run_notifies(&run("ok"), All));
        assert!(routine_run_notifies(&run("error"), All));
        assert!(!routine_run_notifies(&run("ok"), Failing));
        assert!(!routine_run_notifies(&run("skipped"), Failing));
        assert!(routine_run_notifies(&run("error"), Failing));
        assert!(!routine_run_notifies(&run("error"), Off));
        // A run with nothing to say never notifies.
        assert!(!routine_run_notifies(&json!({ "status": "ok", "silent": true }), All));
    }

    #[test]
    fn everyone_sets_every_guy_and_guys_can_differ() {
        let quiet = NotifyPrefs { replies: false, routines: RoutineNotifications::Failing };
        let mut n = Notifications::default();
        assert_eq!(n.for_guy("attic/fox"), NotifyPrefs::default());
        n.set(Some("attic/fox"), quiet);
        assert_eq!(n.for_guy("attic/fox"), quiet);
        assert_eq!(n.for_guy("attic/owl"), NotifyPrefs::default());
        // Back to what everyone gets: no longer set apart.
        n.set(Some("attic/fox"), NotifyPrefs::default());
        assert!(n.guys.is_empty());
        n.set(Some("attic/fox"), quiet);
        n.set(None, NotifyPrefs { replies: true, routines: RoutineNotifications::Off });
        assert!(n.guys.is_empty());
        assert_eq!(n.for_guy("attic/fox").routines, RoutineNotifications::Off);
    }

    #[test]
    fn notification_prefs_default_to_everything() {
        let prefs: Prefs = serde_json::from_str(r#"{ "hiddenGuys": ["attic/fox"] }"#).unwrap();
        assert_eq!(prefs.notifications, Notifications::default());
        let prefs: Prefs = serde_json::from_str(
            r#"{ "notifications": { "everyone": { "replies": true, "routines": "all" }, "guys": { "attic/owl": { "replies": false, "routines": "failing" } } } }"#,
        )
        .unwrap();
        assert_eq!(prefs.notifications.for_guy("attic/owl"), NotifyPrefs { replies: false, routines: RoutineNotifications::Failing });
    }

    #[test]
    fn only_main_and_dashboard_threads_notify() {
        assert!(notifiable_session("agent:main:main"));
        assert!(notifiable_session("agent:main:dashboard:abc"));
        assert!(!notifiable_session("agent:main:slack:channel:C1"));
        assert!(!notifiable_session("agent:main:cron:job:run:1"));
    }

    #[test]
    fn phones_learn_which_faces_are_picked_photos() {
        let photo = "data:image/png;base64,iVBORw0KGgo=".to_string();
        let faces = HashMap::from([("attic/fox".to_string(), photo.clone())]);
        let mut agents = vec![
            json!({ "id": "fox", "identity": { "emoji": "🦊" } }),
            json!({ "id": "owl", "identity": { "avatarUrl": "https://example.com/owl.png" } }),
        ];
        apply_faces(&mut agents, "attic", &faces);
        mark_faces(&mut agents, "attic", &faces);
        assert_eq!(agents[0]["identity"]["avatarUrl"], photo.as_str());
        assert_eq!(agents[0]["littleguysFace"], true);
        // The bot's own face isn't one littleguys can take away.
        assert!(agents[1].get("littleguysFace").is_none());
    }

    #[test]
    fn a_picked_photo_is_the_face_on_its_own_gateway_only() {
        let photo = "data:image/jpeg;base64,/9j/4AAQ".to_string();
        let faces = HashMap::from([("attic/fox".to_string(), photo.clone())]);
        let mut agents = vec![
            json!({ "id": "fox", "identity": { "name": "Fox", "emoji": "🦊", "avatarUrl": "data:image/svg+xml;base64,PHN2Zz4=" } }),
            json!({ "id": "owl", "identity": { "name": "Owl", "emoji": "🦉" } }),
        ];
        apply_faces(&mut agents, "attic", &faces);
        assert_eq!(agents[0]["identity"]["avatarUrl"], photo.as_str());
        assert_eq!(agents[0]["identity"]["emoji"], "🦊");
        assert!(agents[1]["identity"].get("avatarUrl").is_none());
        // Same agent id on another gateway is another guy.
        let mut elsewhere = vec![json!({ "id": "fox" })];
        apply_faces(&mut elsewhere, "garage", &faces);
        assert!(elsewhere[0].get("identity").is_none());
        // A guy the bot gives no identity still gets the photo.
        let mut bare = vec![json!({ "id": "fox" })];
        apply_faces(&mut bare, "attic", &faces);
        assert_eq!(bare[0]["identity"]["avatarUrl"], photo.as_str());
    }

    #[test]
    fn only_small_images_are_faces() {
        assert!(is_face("data:image/png;base64,iVBORw0KGgo="));
        assert!(is_face("data:image/webp;base64,UklGRg=="));
        assert!(!is_face("data:image/svg+xml;base64,PHN2Zz4="));
        assert!(!is_face("https://example.com/fox.png"));
        assert!(!is_face(&format!("data:image/png;base64,{}", "A".repeat(MAX_FACE_CHARS))));
    }
}
