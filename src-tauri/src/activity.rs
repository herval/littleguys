//! The roster: which little guys exist on which gateway, and what they're up
//! to. Kept in Rust (fed by every gateway's events) so the menu-bar sprite and
//! notifications stay live while every window is hidden; webviews receive it
//! as `lg:roster`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::bridge::NoticeKind;
use crate::gateway::client::{ConnStatus, Phase};
use crate::runtime::Runtime;
use crate::gateway::identity::now_ms;
use crate::gateways::OPENCLAW_GATEWAY_ID;
use crate::tray::{Mood, TrayAnimator};

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
    /// Guys with a photo picked in littleguys (`gatewayId/agentId`). It's kept on
    /// this Mac, never sent to the bot, and is already in their `identity.avatarUrl`.
    pub faces: Vec<String>,
    pub updated_at: u64,
}

#[derive(Default)]
struct GatewayState {
    roster: GatewayRoster,
    running: HashSet<String>,
    /// Sessions working because a routine started them (not you): they don't
    /// make the menu-bar guy look busy.
    automated: HashSet<String>,
    approvals: HashMap<String, Value>,
    refresh_scheduled: bool,
    is_openclaw: bool,
    /// key → newest thing the guy said in a turn. OpenClaw's `sessions.list`
    /// preview reads the saved transcript, which some runtimes (claude-cli)
    /// only fill in when the turn ends (or never, when it's stopped), so a
    /// long turn would keep previewing the message that started it.
    live_previews: HashMap<String, LivePreview>,
    /// Running threads whose messages this connection streams (`sessions.messages.subscribe`).
    watched: HashSet<String>,
}

struct LivePreview {
    text: String,
    /// The gateway's preview during the turn; once it moves on, the transcript has caught up.
    saved: Option<String>,
}

impl GatewayState {
    /// True when the shown text changed.
    fn set_live_preview(&mut self, key: &str, text: String) -> bool {
        if let Some(live) = self.live_previews.get_mut(key) {
            return std::mem::replace(&mut live.text, text) != live.text;
        }
        let saved = self
            .roster
            .sessions
            .iter()
            .find(|r| r.get("key").and_then(Value::as_str) == Some(key))
            .and_then(saved_preview)
            .map(str::to_string);
        self.live_previews.insert(key.to_string(), LivePreview { text, saved });
        true
    }

    /// Folds a fresh `sessions.list` row in; returns its key when it's newly worth watching.
    fn track_row(&mut self, row: &Value) -> Option<String> {
        let key = row.get("key").and_then(Value::as_str)?;
        if !run_active(row) {
            if self.live_previews.get(key).is_some_and(|live| live.saved.as_deref() != saved_preview(row)) {
                self.live_previews.remove(key);
            }
            return None;
        }
        self.running.insert(key.to_string());
        if let Some(live) = self.live_previews.get_mut(key) {
            live.saved = saved_preview(row).map(str::to_string);
        }
        // Stream its replies so the preview can follow the turn.
        (self.is_openclaw && self.watched.insert(key.to_string())).then(|| key.to_string())
    }
}

#[derive(Default)]
struct State {
    /// In display order.
    gateways: Vec<GatewayState>,
    focused: Option<(String, String)>,
    /// The main window was auto-opened because the app needs the user.
    auto_opened: bool,
}

impl State {
    fn gateway(&mut self, id: &str) -> Option<&mut GatewayState> {
        self.gateways.iter_mut().find(|g| g.roster.id == id)
    }
}

pub struct Activity {
    app: AppHandle,
    hidden: Mutex<HashSet<String>>,
    /// `gatewayId/agentId` → the photo picked for them, as a `data:image/…` URL.
    faces: Mutex<HashMap<String, String>>,
    clients: Mutex<HashMap<String, Runtime>>,
    tray: TrayAnimator,
    state: Mutex<State>,
    publish_pending: AtomicBool,
    /// Statuses, events and bot-list changes for paired phones (the bridge).
    bridge: tokio::sync::broadcast::Sender<crate::bridge::HostEvent>,
}

const SESSION_LIST_LIMIT: u64 = 400;

impl Activity {
    pub fn new(app: AppHandle, tray: TrayAnimator) -> Arc<Self> {
        let hidden = read_prefs(&app).hidden_guys.into_iter().collect();
        let faces = read_faces(&app);
        Arc::new(Self {
            app,
            hidden: Mutex::new(hidden),
            faces: Mutex::new(faces),
            clients: Mutex::new(HashMap::new()),
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
        let list: Vec<String> = {
            let mut set = self.hidden.lock().unwrap();
            if hidden {
                set.insert(uid.to_string());
            } else {
                set.remove(uid);
            }
            let mut list: Vec<String> = set.iter().cloned().collect();
            list.sort();
            list
        };
        let mut prefs = read_prefs(&self.app);
        prefs.hidden_guys = list;
        write_prefs(&self.app, &prefs)?;
        self.publish();
        Ok(())
    }

    /// Give a guy (`gatewayId/agentId`) a photo, or take it away with `None`. It's
    /// only stored in littleguys' data dir: bots never see it.
    pub fn set_guy_face(&self, uid: &str, face: Option<String>) -> anyhow::Result<()> {
        if let Some(face) = &face {
            anyhow::ensure!(is_face(face), "That photo can't be used.");
        }
        let faces = {
            let mut faces = self.faces.lock().unwrap();
            match face {
                Some(face) => faces.insert(uid.to_string(), face),
                None => faces.remove(uid),
            };
            faces.clone()
        };
        write_faces(&self.app, &faces)?;
        self.publish();
        Ok(())
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
        self.hidden.lock().unwrap().contains(&format!("{gateway}/{agent_id}"))
    }

    // ---- gateway lifecycle (driven by the hub) ---------------------------------

    pub fn attach(&self, client: Runtime, name: String, enabled: bool) {
        let id = client.id().to_string();
        let status = client.status();
        let is_openclaw = client.as_openclaw().is_some();
        self.clients.lock().unwrap().insert(id.clone(), client);
        let mut state = self.state.lock().unwrap();
        if state.gateway(&id).is_none() {
            state.gateways.push(GatewayState {
                roster: GatewayRoster { id, name, enabled, status, ..Default::default() },
                is_openclaw,
                ..Default::default()
            });
        }
        drop(state);
        let _ = self.bridge.send(crate::bridge::HostEvent::BotsChanged);
    }

    pub fn detach(&self, id: &str) {
        self.clients.lock().unwrap().remove(id);
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
                        g.running.clear();
                        g.approvals.clear();
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

    fn client(&self, id: &str) -> Option<Runtime> {
        self.clients.lock().unwrap().get(id).cloned()
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

    pub fn on_status(&self, status: &ConnStatus) {
        log::info!(
            "[{}] gateway {:?} {} {}",
            status.gateway_name,
            status.phase,
            status.url.as_deref().unwrap_or(""),
            status.message.as_deref().unwrap_or("")
        );
        let _ = self.app.emit("gw:status", status);
        if let Ok(raw) = serde_json::to_value(status) {
            let _ = self.bridge.send(crate::bridge::HostEvent::Status(raw));
        }
        let open = {
            let mut state = self.state.lock().unwrap();
            let others_connected = state
                .gateways
                .iter()
                .any(|g| g.roster.id != status.gateway_id && g.roster.enabled && g.roster.status.phase == Phase::Connected);
            if let Some(g) = state.gateway(&status.gateway_id) {
                g.roster.status = status.clone();
                if status.phase != Phase::Connected {
                    g.roster.loaded = false;
                }
            }
            // First time something needs the user (approve pairing, paste a
            // token, set up OpenClaw), bring the window up instead of waiting
            // for a click. "Not set up" only matters when nothing else works.
            let actionable = match status.phase {
                Phase::Pairing | Phase::AuthFailed => true,
                Phase::NoConfig => status.gateway_id == OPENCLAW_GATEWAY_ID && !others_connected,
                _ => false,
            };
            let open = actionable && !state.auto_opened;
            state.auto_opened |= actionable;
            open
        };
        self.publish();
        if open {
            crate::windows::open_main(&self.app, None);
        }
    }

    pub fn on_connected(self: &Arc<Self>, gateway: &str) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(g) = state.gateway(gateway) {
                g.running.clear();
                g.approvals.clear();
                // Subscriptions don't survive a reconnect.
                g.watched.clear();
            }
        }
        let this = self.clone();
        let gateway = gateway.to_string();
        tauri::async_runtime::spawn(async move {
            let Some(client) = this.client(&gateway) else { return };
            if let Err(e) = client.call("sessions.subscribe", json!({})).await {
                log::warn!("sessions.subscribe failed: {e}");
            }
            if let Ok(result) = client.call("exec.approval.list", json!({})).await {
                let pending = result
                    .get("approvals")
                    .or_else(|| result.get("requests"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut state = this.state.lock().unwrap();
                if let Some(g) = state.gateway(&gateway) {
                    for approval in pending {
                        if let Some(id) = approval_id(&approval) {
                            g.approvals.insert(id, approval);
                        }
                    }
                }
            }
            this.refresh(&gateway).await;
        });
    }

    pub fn on_event(self: &Arc<Self>, gateway: &str, name: &str, payload: &Value) {
        let _ = self.app.emit("gw:event", json!({ "gateway": gateway, "event": name, "payload": payload }));
        let _ = self.bridge.send(crate::bridge::HostEvent::Event {
            gateway: gateway.to_string(),
            name: name.to_string(),
            payload: payload.clone(),
        });
        match name {
            "sessions.changed" => {
                if let Some(key) = payload.get("sessionKey").and_then(Value::as_str) {
                    let mut notify = false;
                    {
                        let mut state = self.state.lock().unwrap();
                        let focused = state.focused.as_ref().is_some_and(|(g, k)| g == gateway && k == key);
                        if let Some(g) = state.gateway(gateway) {
                            match payload.get("phase").and_then(Value::as_str) {
                                Some("start") => {
                                    g.running.insert(key.to_string());
                                    // A new turn: the saved transcript (your message) is newest again.
                                    g.live_previews.remove(key);
                                }
                                Some("end") | Some("error") => {
                                    notify = g.running.remove(key) && !focused;
                                    g.automated.remove(key);
                                }
                                _ => {}
                            }
                        }
                    }
                    if notify {
                        self.notify_reply_later(gateway.to_string(), key.to_string());
                    }
                }
                self.schedule_refresh(gateway);
            }
            "chat" => {
                let state_name = payload.get("state").and_then(Value::as_str).unwrap_or_default();
                let (mut ran, mut said) = (false, false);
                if let Some(key) = payload.get("sessionKey").and_then(Value::as_str) {
                    let mut state = self.state.lock().unwrap();
                    if let Some(g) = state.gateway(gateway) {
                        ran = match state_name {
                            "delta" | "status" => g.running.insert(key.to_string()),
                            "final" | "aborted" | "error" => g.running.remove(key),
                            _ => false,
                        };
                        if state_name == "delta" && g.is_openclaw {
                            let text = payload.get("message").map(message_text).unwrap_or_default();
                            if let Some(latest) = newest_paragraph(&text) {
                                said = g.set_live_preview(key, truncate(latest, 200));
                            }
                        }
                    }
                }
                if ran {
                    self.publish();
                } else if said {
                    self.publish_soon();
                }
            }
            "session.message" => {
                let message = payload.get("message").unwrap_or(&Value::Null);
                let text = message_text(message);
                if let (Some(key), Some("assistant"), false) = (
                    payload.get("sessionKey").and_then(Value::as_str),
                    message.get("role").and_then(Value::as_str),
                    text.trim().is_empty(),
                ) {
                    let said = {
                        let mut state = self.state.lock().unwrap();
                        state.gateway(gateway).filter(|g| g.is_openclaw).is_some_and(|g| g.set_live_preview(key, truncate(&text, 200)))
                    };
                    if said {
                        self.publish_soon();
                    }
                }
            }
            "exec.approval.requested" => {
                if let Some(id) = approval_id(payload) {
                    if let Some(g) = self.state.lock().unwrap().gateway(gateway) {
                        g.approvals.insert(id, payload.clone());
                    }
                    self.notify_approval(gateway, payload);
                }
                self.publish();
            }
            "exec.approval.resolved" => {
                if let Some(id) = approval_id(payload) {
                    if let Some(g) = self.state.lock().unwrap().gateway(gateway) {
                        g.approvals.remove(&id);
                    }
                    // Phones take down the approval's notification.
                    self.push_to_phones(NoticeKind::ApprovalResolved, "", "", gateway, None, Some(id));
                }
                self.publish();
            }
            "cron" => {
                let action = payload.get("action").and_then(Value::as_str);
                if let Some(key) = routine_target(payload) {
                    let mut state = self.state.lock().unwrap();
                    if let Some(g) = state.gateway(gateway) {
                        match action {
                            Some("started") => {
                                g.automated.insert(key);
                            }
                            Some("finished") => {
                                g.automated.remove(&key);
                            }
                            _ => {}
                        }
                    }
                }
                if action == Some("finished") {
                    self.notify_routine(gateway, payload);
                }
                self.schedule_refresh(gateway);
            }
            "config.changed" | "agents.changed" => self.schedule_refresh(gateway),
            _ => {}
        }
    }

    // ---- roster refresh ------------------------------------------------------------

    fn schedule_refresh(self: &Arc<Self>, gateway: &str) {
        {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway) else { return };
            if g.refresh_scheduled {
                return;
            }
            g.refresh_scheduled = true;
        }
        let this = self.clone();
        let gateway = gateway.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if let Some(g) = this.state.lock().unwrap().gateway(&gateway) {
                g.refresh_scheduled = false;
            }
            this.refresh(&gateway).await;
        });
    }

    pub async fn refresh_all(&self) {
        let ids: Vec<String> = self.clients.lock().unwrap().keys().cloned().collect();
        futures_util::future::join_all(ids.iter().map(|id| self.refresh(id))).await;
    }

    pub async fn refresh(&self, gateway: &str) {
        let Some(client) = self.client(gateway) else { return };
        if client.status().phase != Phase::Connected {
            return;
        }
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
        let mut watch = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway) else { return };
            if let Ok(agents) = &agents {
                g.roster.default_agent_id = agents.get("defaultId").and_then(Value::as_str).map(str::to_string);
                g.roster.agents = agents.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
            }
            if let Ok(sessions) = &sessions {
                let rows = sessions.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
                watch.extend(rows.iter().filter_map(|row| g.track_row(row)));
                g.roster.sessions = rows;
            }
            g.roster.loaded = agents.is_ok() && sessions.is_ok();
            if let (Ok(a), Ok(s)) = (&agents, &sessions) {
                log::info!(
                    "[{}] roster: {} guys, {} threads",
                    g.roster.name,
                    a.get("agents").and_then(Value::as_array).map_or(0, Vec::len),
                    s.get("sessions").and_then(Value::as_array).map_or(0, Vec::len)
                );
            }
        }
        if let Err(e) = &agents {
            log::warn!("agents.list failed: {e}");
        }
        if let Err(e) = &sessions {
            log::warn!("sessions.list failed: {e}");
        }
        self.publish();
        for key in watch {
            if let Err(e) = client.call("sessions.messages.subscribe", json!({ "key": key })).await {
                log::warn!("sessions.messages.subscribe {key} failed: {e}");
                if let Some(g) = self.state.lock().unwrap().gateway(gateway) {
                    g.watched.remove(&key);
                }
            }
        }
    }

    fn snapshot(&self) -> Roster {
        let mut state = self.state.lock().unwrap();
        for g in state.gateways.iter_mut() {
            g.roster.running = g.running.iter().cloned().collect();
            g.roster.approvals = g.approvals.values().cloned().collect();
        }
        let mut hidden: Vec<String> = self.hidden.lock().unwrap().iter().cloned().collect();
        hidden.sort();
        let faces = self.faces.lock().unwrap();
        let gateways = state
            .gateways
            .iter()
            .map(|g| {
                let mut roster = g.roster.clone();
                apply_live_previews(&mut roster.sessions, &g.live_previews);
                apply_faces(&mut roster.agents, &roster.id, &faces);
                roster
            })
            .collect();
        let mut with_faces: Vec<String> = faces.keys().cloned().collect();
        with_faces.sort();
        Roster { gateways, hidden, faces: with_faces, updated_at: now_ms() }
    }

    /// Streamed text arrives in bursts across every running thread; one roster
    /// update per beat is plenty for a preview line.
    fn publish_soon(self: &Arc<Self>) {
        if self.publish_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            this.publish_pending.store(false, Ordering::Release);
            this.publish();
        });
    }

    fn publish(&self) {
        let roster = self.snapshot();
        let _ = self.app.emit("lg:roster", &roster);
        let automated: HashMap<String, HashSet<String>> =
            self.state.lock().unwrap().gateways.iter().map(|g| (g.roster.id.clone(), g.automated.clone())).collect();
        self.update_mood(&roster, &automated);
    }

    fn update_mood(&self, roster: &Roster, automated: &HashMap<String, HashSet<String>>) {
        let enabled: Vec<&GatewayRoster> = roster.gateways.iter().filter(|g| g.enabled).collect();
        let connected: Vec<&&GatewayRoster> = enabled.iter().filter(|g| g.status.phase == Phase::Connected).collect();
        let shown = |g: &GatewayRoster, key: &str| {
            agent_id_from_key(key).is_none_or(|agent| !roster.hidden.contains(&format!("{}/{agent}", g.id)))
        };
        let mood = if connected.iter().any(|g| !g.approvals.is_empty())
            || enabled.iter().any(|g| matches!(g.status.phase, Phase::Pairing | Phase::AuthFailed))
        {
            Mood::NeedsYou
        } else if connected.iter().any(|g| {
            // Routine runs happen in the background; only work you're waiting on animates.
            let by_routine = automated.get(&g.id);
            g.running.iter().any(|k| shown(g, k) && !k.contains(":cron:") && !by_routine.is_some_and(|a| a.contains(k)))
        }) {
            Mood::Working
        } else if connected.iter().any(|g| {
            g.sessions
                .iter()
                .any(|r| is_notable_unread(r) && r.get("key").and_then(Value::as_str).is_some_and(|k| shown(g, k)))
        }) {
            Mood::News
        } else if !connected.is_empty() {
            Mood::Idle
        } else if enabled
            .iter()
            .any(|g| matches!(g.status.phase, Phase::Starting | Phase::Connecting | Phase::NoConfig))
        {
            Mood::Setup
        } else {
            Mood::Offline
        };
        self.tray.set_mood(mood);
    }

    // ---- notifications ---------------------------------------------------------------

    fn main_window_focused(&self) -> bool {
        self.app
            .get_webview_window("main")
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false)
    }

    /// "🦊 Fox", or "🦊 Fox · attic" once there's more than one gateway.
    fn agent_label(&self, gateway: &str, agent_id: &str) -> String {
        let multi = self.gateway_count() > 1;
        let mut state = self.state.lock().unwrap();
        let Some(g) = state.gateway(gateway) else { return agent_id.to_string() };
        let name = g
            .roster
            .agents
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(agent_id))
            .map(agent_display_name)
            .unwrap_or_else(|| agent_id.to_string());
        if multi {
            format!("{name} · {}", g.roster.name)
        } else {
            name
        }
    }

    /// A run ended in a thread the user isn't looking at: once the roster
    /// refresh lands, notify with the thread's last message.
    fn notify_reply_later(self: &Arc<Self>, gateway: String, key: String) {
        if !notifiable_session(&key) || agent_id_from_key(&key).is_some_and(|a| self.is_hidden(&gateway, &a)) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(900)).await;
            let focused = this.state.lock().unwrap().focused.clone();
            if this.main_window_focused() && focused == Some((gateway.clone(), key.clone())) {
                return;
            }
            let preview = {
                let mut state = this.state.lock().unwrap();
                state.gateway(&gateway).and_then(|g| match g.live_previews.get(&key) {
                    Some(live) => Some(truncate(&live.text, 180)),
                    None => g
                        .roster
                        .sessions
                        .iter()
                        .find(|r| r.get("key").and_then(Value::as_str) == Some(key.as_str()))
                        .and_then(saved_preview)
                        .map(|s| truncate(s, 180)),
                })
            };
            let agent_id = agent_id_from_key(&key).unwrap_or_default();
            let preview = preview.unwrap_or_else(|| "Finished working.".into());
            let title = this.agent_label(&gateway, &agent_id);
            this.show_notification(&title, &preview);
            this.push_to_phones(NoticeKind::Reply, &title, &preview, &gateway, Some(key), None);
        });
    }

    fn notify_routine(&self, gateway: &str, payload: &Value) {
        let job = payload.get("job").cloned().unwrap_or(Value::Null);
        let name = job.get("name").and_then(Value::as_str).unwrap_or("Routine");
        let agent_id = job.get("agentId").and_then(Value::as_str).unwrap_or_default();
        if !agent_id.is_empty() && self.is_hidden(gateway, agent_id) {
            return;
        }
        // The run had nothing to report (Hermes' [SILENT]).
        if payload.get("silent").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let status = payload.get("status").and_then(Value::as_str).unwrap_or("ok");
        let summary = payload.get("summary").and_then(Value::as_str).map(|s| truncate(s, 180));
        let title = if agent_id.is_empty() {
            name.to_string()
        } else {
            format!("{} · {name}", self.agent_label(gateway, agent_id))
        };
        let body = match (status, summary) {
            ("ok", Some(summary)) => summary,
            ("ok", None) => "Routine finished.".into(),
            (_, Some(summary)) => format!("Routine failed: {summary}"),
            (other, None) => format!("Routine {other}."),
        };
        self.show_notification(&title, &body);
        let key = payload.get("sessionKey").and_then(Value::as_str).map(str::to_string).or_else(|| routine_target(payload));
        self.push_to_phones(NoticeKind::Routine, &title, &body, gateway, key, None);
    }

    fn notify_approval(&self, gateway: &str, payload: &Value) {
        let command = payload
            .pointer("/presentation/commandText")
            .or_else(|| payload.pointer("/request/command"))
            .or_else(|| payload.get("command"))
            .and_then(Value::as_str)
            .map(|c| truncate(c, 120))
            .unwrap_or_else(|| "a command".into());
        let agent_id = payload
            .pointer("/request/agentId")
            .or_else(|| payload.get("agentId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let who = if agent_id.is_empty() { "A little guy".to_string() } else { self.agent_label(gateway, agent_id) };
        let (title, body) = (format!("{who} needs your OK"), format!("Wants to run: {command}"));
        self.show_notification(&title, &body);
        let key = payload
            .get("sessionKey")
            .or_else(|| payload.pointer("/request/sessionKey"))
            .and_then(Value::as_str)
            .map(str::to_string);
        self.push_to_phones(NoticeKind::Approval, &title, &body, gateway, key, approval_id(payload));
    }

    fn show_notification(&self, title: &str, body: &str) {
        if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
            log::warn!("notification failed: {e}");
        }
    }

    /// The same notification for paired phones, as a push (bridge/push.rs).
    fn push_to_phones(&self, kind: NoticeKind, title: &str, body: &str, gateway: &str, session_key: Option<String>, approval_id: Option<String>) {
        let _ = self.bridge.send(crate::bridge::HostEvent::Notice(crate::bridge::Notice {
            kind,
            title: title.to_string(),
            body: body.to_string(),
            gateway: gateway.to_string(),
            session_key,
            approval_id,
        }));
    }
}

#[derive(Debug, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Prefs {
    #[serde(default)]
    hidden_guys: Vec<String>,
}

fn prefs_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("prefs.json"))
}

fn read_prefs(app: &AppHandle) -> Prefs {
    prefs_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_prefs(app: &AppHandle, prefs: &Prefs) -> anyhow::Result<()> {
    let path = prefs_path(app).ok_or_else(|| anyhow::anyhow!("no app data dir"))?;
    crate::gateway::identity::write_private(&path, &serde_json::to_vec_pretty(prefs)?)
}

/// Photos are small images the app made itself (see src/lib/avatarImage.ts).
const MAX_FACE_CHARS: usize = 100_000;

pub(crate) fn is_face(url: &str) -> bool {
    url.len() <= MAX_FACE_CHARS
        && ["data:image/png;base64,", "data:image/jpeg;base64,", "data:image/webp;base64,"].iter().any(|p| url.starts_with(p))
}

fn faces_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("faces.json"))
}

fn read_faces(app: &AppHandle) -> HashMap<String, String> {
    let faces: HashMap<String, String> = faces_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    faces.into_iter().filter(|(_, face)| is_face(face)).collect()
}

fn write_faces(app: &AppHandle, faces: &HashMap<String, String>) -> anyhow::Result<()> {
    let path = faces_path(app).ok_or_else(|| anyhow::anyhow!("no app data dir"))?;
    crate::gateway::identity::write_private(&path, &serde_json::to_vec(faces)?)
}

/// A photo picked in littleguys wins over the face the bot gives a guy.
fn apply_faces(agents: &mut [Value], gateway: &str, faces: &HashMap<String, String>) {
    if faces.is_empty() {
        return;
    }
    for agent in agents.iter_mut().filter(|a| a.is_object()) {
        let Some(face) = agent.get("id").and_then(Value::as_str).and_then(|id| faces.get(&format!("{gateway}/{id}"))) else {
            continue;
        };
        if !agent.get("identity").is_some_and(Value::is_object) {
            agent["identity"] = json!({});
        }
        agent["identity"]["avatarUrl"] = json!(face);
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

/// The conversation a routine run happens in, when it isn't its own
/// (`sessionTarget` "session:<key>" or "main"); isolated runs get `:cron:` keys.
fn routine_target(payload: &Value) -> Option<String> {
    let job = payload.get("job")?;
    let target = job.get("sessionTarget").and_then(Value::as_str)?;
    if let Some(key) = target.strip_prefix("session:") {
        return Some(key.to_string());
    }
    let agent = job
        .get("agentId")
        .or_else(|| job.pointer("/owner/agentId"))
        .and_then(Value::as_str)?;
    (target == "main").then(|| format!("agent:{agent}:main"))
}

fn is_notable_unread(row: &Value) -> bool {
    row.get("unread").and_then(Value::as_bool) == Some(true)
        && row.get("key").and_then(Value::as_str).is_some_and(notifiable_session)
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
    use super::*;

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
        let tool_turn = json!({ "role": "assistant", "content": [{ "type": "toolCall", "name": "Bash" }, { "type": "text", "text": "Tests pass." }] });
        assert_eq!(message_text(&tool_turn), "Tests pass.");
        assert_eq!(message_text(&json!({ "role": "user", "content": "go for it" })), "go for it");
    }

    #[test]
    fn live_preview_lasts_until_the_transcript_catches_up() {
        let key = "agent:fox:dashboard:a1";
        let row = |preview: &str, status: &str, active: bool| {
            json!({ "key": key, "lastMessagePreview": preview, "status": status, "hasActiveRun": active })
        };
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
    fn only_main_and_dashboard_threads_notify() {
        assert!(notifiable_session("agent:main:main"));
        assert!(notifiable_session("agent:main:dashboard:abc"));
        assert!(!notifiable_session("agent:main:slack:channel:C1"));
        assert!(!notifiable_session("agent:main:cron:job:run:1"));
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
