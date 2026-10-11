//! The roster: which little guys exist on which gateway, and what they're up
//! to. Kept in Rust (fed by every gateway's events) so the menu-bar sprite and
//! notifications stay live while every window is hidden; webviews receive it
//! as `lg:roster`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
#[cfg(not(target_os = "macos"))]
use tauri_plugin_notification::NotificationExt;

use crate::gateway::client::{ConnStatus, Phase};
use crate::runtime::Runtime;
use crate::gateway::identity::now_ms;
use crate::gateways::OPENCLAW_GATEWAY_ID;
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
    clients: Mutex<HashMap<String, Runtime>>,
    tray: TrayAnimator,
    state: Mutex<State>,
}

const SESSION_LIST_LIMIT: u64 = 400;

impl Activity {
    pub fn new(app: AppHandle, tray: TrayAnimator) -> Arc<Self> {
        let hidden = read_prefs(&app).hidden_guys.into_iter().collect();
        Arc::new(Self {
            app,
            hidden: Mutex::new(hidden),
            clients: Mutex::new(HashMap::new()),
            tray,
            state: Mutex::new(State::default()),
        })
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

    fn is_hidden(&self, gateway: &str, agent_id: &str) -> bool {
        self.hidden.lock().unwrap().contains(&format!("{gateway}/{agent_id}"))
    }

    // ---- gateway lifecycle (driven by the hub) ---------------------------------

    pub fn attach(&self, client: Runtime, name: String, enabled: bool) {
        let id = client.id().to_string();
        let status = client.status();
        self.clients.lock().unwrap().insert(id.clone(), client);
        let mut state = self.state.lock().unwrap();
        if state.gateway(&id).is_none() {
            state.gateways.push(GatewayState {
                roster: GatewayRoster { id, name, enabled, status, ..Default::default() },
                ..Default::default()
            });
        }
    }

    pub fn detach(&self, id: &str) {
        self.clients.lock().unwrap().remove(id);
        self.state.lock().unwrap().gateways.retain(|g| g.roster.id != id);
        self.publish();
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
                        g.roster.loaded = false;
                    }
                }
            }
        }
        self.publish();
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
                if let Some(key) = payload.get("sessionKey").and_then(Value::as_str) {
                    let mut state = self.state.lock().unwrap();
                    if let Some(g) = state.gateway(gateway) {
                        match state_name {
                            "delta" | "status" => {
                                g.running.insert(key.to_string());
                            }
                            "final" | "aborted" | "error" => {
                                g.running.remove(key);
                            }
                            _ => {}
                        }
                    }
                }
                self.publish();
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
        {
            let mut state = self.state.lock().unwrap();
            let Some(g) = state.gateway(gateway) else { return };
            if let Ok(agents) = &agents {
                g.roster.default_agent_id = agents.get("defaultId").and_then(Value::as_str).map(str::to_string);
                g.roster.agents = agents.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
            }
            if let Ok(sessions) = &sessions {
                let rows = sessions.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
                for row in &rows {
                    let active = row.get("hasActiveRun").and_then(Value::as_bool) == Some(true)
                        || row.get("status").and_then(Value::as_str) == Some("running");
                    if let (true, Some(key)) = (active, row.get("key").and_then(Value::as_str)) {
                        g.running.insert(key.to_string());
                    }
                }
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
    }

    fn snapshot(&self) -> Roster {
        let mut state = self.state.lock().unwrap();
        for g in state.gateways.iter_mut() {
            g.roster.running = g.running.iter().cloned().collect();
            g.roster.approvals = g.approvals.values().cloned().collect();
        }
        let mut hidden: Vec<String> = self.hidden.lock().unwrap().iter().cloned().collect();
        hidden.sort();
        Roster { gateways: state.gateways.iter().map(|g| g.roster.clone()).collect(), hidden, updated_at: now_ms() }
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
            let row = {
                let mut state = this.state.lock().unwrap();
                state.gateway(&gateway).and_then(|g| {
                    g.roster.sessions.iter().find(|r| r.get("key").and_then(Value::as_str) == Some(key.as_str())).cloned()
                })
            };
            let agent_id = agent_id_from_key(&key).unwrap_or_default();
            let preview = row
                .as_ref()
                .and_then(|r| r.get("lastMessagePreview"))
                .and_then(Value::as_str)
                .map(|s| truncate(s, 180))
                .unwrap_or_else(|| "Finished working.".into());
            let label = this.agent_label(&gateway, &agent_id);
            let nav = Navigate { gateway_id: Some(gateway), agent_id: Some(agent_id), session_key: Some(key), view: None };
            this.show_notification(&label, &preview, nav);
        });
    }

    fn notify_routine(&self, gateway: &str, payload: &Value) {
        let job = payload.get("job").cloned().unwrap_or(Value::Null);
        let name = job.get("name").and_then(Value::as_str).unwrap_or("Routine");
        let agent_id = job.get("agentId").and_then(Value::as_str).unwrap_or_default();
        if !agent_id.is_empty() && self.is_hidden(gateway, agent_id) {
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
        let nav = Navigate {
            gateway_id: Some(gateway.to_string()),
            agent_id: Some(agent_id.to_string()).filter(|a| !a.is_empty()),
            session_key: payload.get("sessionKey").and_then(Value::as_str).map(str::to_string),
            view: Some("routines".into()),
        };
        self.show_notification(&title, &body, nav);
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
        let nav = Navigate {
            gateway_id: Some(gateway.to_string()),
            agent_id: Some(agent_id.to_string()).filter(|a| !a.is_empty()),
            session_key: payload.pointer("/request/sessionKey").or_else(|| payload.get("sessionKey")).and_then(Value::as_str).map(str::to_string),
            view: None,
        };
        self.show_notification(&format!("{who} needs your OK"), &format!("Wants to run: {command}"), nav);
    }

    /// Shows a notification; clicking it opens `nav` (macOS only — the notification
    /// plugin ignores clicks on desktop, so there it goes through mac-notification-sys).
    fn show_notification(&self, title: &str, body: &str, nav: Navigate) {
        #[cfg(target_os = "macos")]
        {
            static APP_SET: std::sync::Once = std::sync::Once::new();
            let identifier = self.app.config().identifier.clone();
            APP_SET.call_once(|| {
                // Same as the plugin: unbundled dev builds borrow Terminal's identity.
                let _ = mac_notification_sys::set_application(if tauri::is_dev() { "com.apple.Terminal" } else { &identifier });
            });
            let (app, title, body) = (self.app.clone(), title.to_string(), body.to_string());
            // Blocks until the notification is clicked, closed or cleared from Notification Center.
            std::thread::spawn(move || match mac_notification_sys::Notification::new().title(&title).message(&body).wait_for_click(true).send() {
                Ok(mac_notification_sys::NotificationResponse::Click) => {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || crate::windows::open_main(&handle, Some(nav)));
                }
                Ok(_) => {}
                Err(e) => log::warn!("notification failed: {e}"),
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = nav;
            if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
                log::warn!("notification failed: {e}");
            }
        }
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
    fn only_main_and_dashboard_threads_notify() {
        assert!(notifiable_session("agent:main:main"));
        assert!(notifiable_session("agent:main:dashboard:abc"));
        assert!(!notifiable_session("agent:main:slack:channel:C1"));
        assert!(!notifiable_session("agent:main:cron:job:run:1"));
    }
}
