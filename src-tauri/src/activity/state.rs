use super::{approval_id, message_text, newest_paragraph, routine_target, run_active, saved_preview, truncate, turns, GatewayRoster};
use crate::gateway::client::{ConnStatus, Phase};
use crate::gateways::OPENCLAW_GATEWAY_ID;
use crate::runtime::events::RunEvent;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct GatewayState {
    pub roster: GatewayRoster,
    pub generation: u64,
    pub epoch: u64,
    pub revision: u64,
    pub refresh_request: u64,
    pub resolved_approvals: std::collections::VecDeque<String>,
    pub settled: HashSet<String>,
    pub running: HashSet<String>,
    pub turns: turns::Turns,
    /// Sessions working because a routine started them (not you): they don't
    /// make the menu-bar guy look busy.
    pub automated: HashSet<String>,
    pub approvals: HashMap<String, Value>,
    pub refresh_scheduled: bool,
    pub is_openclaw: bool,
    /// key → newest thing the guy said in a turn. OpenClaw's `sessions.list`
    /// preview reads the saved transcript, which some runtimes (claude-cli)
    /// only fill in when the turn ends (or never, when it's stopped), so a
    /// long turn would keep previewing the message that started it.
    pub live_previews: HashMap<String, LivePreview>,
    /// Running threads whose messages this connection streams (`sessions.messages.subscribe`).
    pub watched: HashSet<String>,
}

pub(super) struct LivePreview {
    pub text: String,
    /// The gateway's preview during the turn; once it moves on, the transcript has caught up.
    pub saved: Option<String>,
}

impl GatewayState {
    /// True when the shown text changed.
    pub fn set_live_preview(&mut self, key: &str, text: String) -> bool {
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
    pub fn track_row(&mut self, row: &Value) -> Option<String> {
        let key = row.get("key").and_then(Value::as_str)?;
        if !run_active(row) {
            self.settled.remove(key);
            if self.live_previews.get(key).is_some_and(|live| live.saved.as_deref() != saved_preview(row)) {
                self.live_previews.remove(key);
            }
            return None;
        }
        if self.settled.contains(key) || !self.turns.start(key, row.get("runId").and_then(Value::as_str)) {
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
pub(super) struct State {
    /// In display order.
    pub gateways: Vec<GatewayState>,
    pub focused: Option<(String, String)>,
    /// The main window was auto-opened because the app needs the user.
    pub auto_opened: bool,
}

impl State {
    pub fn gateway(&mut self, id: &str) -> Option<&mut GatewayState> {
        self.gateways.iter_mut().find(|g| g.roster.id == id)
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Effect {
    Publish,
    PublishSoon,
    Refresh,
    Reply(String),
    Approval(Value),
    ApprovalResolved(String),
    Routine(Value),
    /// A turn ended (not stopped): what littleguys held for the thread can go (outbox.rs).
    TurnOver(String),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Connection {
    pub generation: u64,
    pub epoch: u64,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RefreshTicket {
    connection: Connection,
    request: u64,
    revision: u64,
}

impl GatewayState {
    pub fn connection(&self) -> Connection {
        Connection { generation: self.generation, epoch: self.epoch }
    }
    pub fn disconnect(&mut self) {
        self.epoch += 1;
        self.revision += 1;
        self.running.clear();
        self.turns.disconnect();
        self.automated.clear();
        self.approvals.clear();
        self.resolved_approvals.clear();
        self.watched.clear();
        self.settled.clear();
        self.refresh_scheduled = false;
        self.roster.loaded = false;
    }
    pub fn begin_refresh(&mut self) -> RefreshTicket {
        self.refresh_request += 1;
        RefreshTicket { connection: self.connection(), request: self.refresh_request, revision: self.revision }
    }
    pub fn accepts(&self, ticket: RefreshTicket) -> bool {
        ticket.connection == self.connection() && ticket.request == self.refresh_request && ticket.revision == self.revision
    }
    /// A superseded request already has a successor. Only newer events need a retry.
    pub fn needs_refresh(&self, ticket: RefreshTicket) -> bool {
        ticket.connection == self.connection() && ticket.request == self.refresh_request && ticket.revision != self.revision
    }
    pub fn snapshot(&mut self, ticket: RefreshTicket, agents: Option<&Value>, sessions: Option<&Value>) -> Option<Vec<String>> {
        if !self.accepts(ticket) {
            return None;
        }
        let mut watch = Vec::new();
        if let Some(agents) = agents {
            self.roster.default_agent_id = agents.get("defaultId").and_then(Value::as_str).map(str::to_string);
            self.roster.agents = agents.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
        }
        if let Some(sessions) = sessions {
            let rows = sessions.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
            self.running.clear();
            watch.extend(rows.iter().filter_map(|row| self.track_row(row)));
            self.roster.sessions = rows;
        }
        self.roster.loaded = agents.is_some() && sessions.is_some();
        Some(watch)
    }
    pub fn approval_snapshot(&mut self, connection: Connection, approvals: Vec<Value>) {
        if self.connection() != connection {
            return;
        }
        for approval in approvals {
            if let Some(id) = approval_id(&approval).filter(|id| !self.resolved_approvals.contains(id)) {
                self.approvals.entry(id).or_insert(approval);
            }
        }
    }
    pub fn event(&mut self, name: &str, payload: &Value) -> Vec<Effect> {
        let mut effects = Vec::new();
        let key = payload.get("sessionKey").and_then(Value::as_str);
        match name {
            "sessions.changed" | "chat" => {
                if let Some(event) = RunEvent::decode(name, payload) {
                    let (key, run, phase) = (event.session_key, event.run_id, event.phase.as_str());
                    match phase {
                        "start" | "status" | "delta" => {
                            let accepted = self.turns.start(key, run);
                            if accepted {
                                self.settled.remove(key);
                            }
                            if accepted && self.running.insert(key.to_string()) {
                                self.revision += 1;
                                if phase == "start" {
                                    self.live_previews.remove(key);
                                }
                                effects.push(Effect::Publish);
                            }
                            if accepted && phase == "delta" && self.is_openclaw {
                                if let Some(latest) = newest_paragraph(&payload.get("message").map(message_text).unwrap_or_default()) {
                                    if self.set_live_preview(key, truncate(latest, 200)) {
                                        effects.push(Effect::PublishSoon);
                                    }
                                }
                            }
                        }
                        "end" | "final" | "error" | "aborted" => {
                            // A completion invalidates in-flight snapshots even if its start was missed.
                            self.revision += 1;
                            if phase == "final" || phase == "error" {
                                effects.push(Effect::TurnOver(key.to_string()));
                            }
                            if self.turns.finish(key, run) {
                                self.running.remove(key);
                                self.settled.insert(key.to_string());
                                if phase != "aborted" && !self.automated.contains(key) {
                                    effects.push(Effect::Reply(key.to_string()));
                                }
                                self.automated.remove(key);
                                effects.push(Effect::Publish);
                            }
                            effects.push(Effect::Refresh);
                        }
                        _ => {}
                    }
                }
                if name == "sessions.changed" {
                    effects.push(Effect::Refresh);
                }
            }
            "session.message" => {
                let message = payload.get("message").unwrap_or(&Value::Null);
                let text = message_text(message);
                if let Some(key) = key.filter(|_| {
                    self.is_openclaw && message.get("role").and_then(Value::as_str) == Some("assistant") && !text.trim().is_empty()
                }) {
                    if self.set_live_preview(key, truncate(&text, 200)) {
                        effects.push(Effect::PublishSoon);
                    }
                }
            }
            "exec.approval.requested" => {
                if let Some(id) = approval_id(payload).filter(|id| !self.resolved_approvals.contains(id)) {
                    self.revision += 1;
                    if self.approvals.insert(id, payload.clone()).is_none() {
                        effects.push(Effect::Approval(payload.clone()));
                    }
                    effects.push(Effect::Publish);
                }
            }
            "exec.approval.resolved" => {
                if let Some(id) = approval_id(payload) {
                    self.revision += 1;
                    self.approvals.remove(&id);
                    if !self.resolved_approvals.contains(&id) {
                        self.resolved_approvals.push_back(id.clone());
                        if self.resolved_approvals.len() > 2048 {
                            self.resolved_approvals.pop_front();
                        }
                        effects.push(Effect::ApprovalResolved(id));
                    }
                    effects.push(Effect::Publish);
                }
            }
            "cron" => {
                let action = payload.get("action").and_then(Value::as_str);
                if let Some(key) = routine_target(payload) {
                    match action {
                        Some("started") => {
                            self.automated.insert(key);
                        }
                        Some("finished") => {
                            self.automated.remove(&key);
                        }
                        _ => {}
                    }
                }
                if action == Some("finished") {
                    effects.push(Effect::Routine(payload.clone()));
                }
                effects.push(Effect::Refresh);
            }
            "config.changed" | "agents.changed" => {
                self.revision += 1;
                effects.push(Effect::Refresh);
            }
            _ => {}
        }
        effects
    }
}
impl State {
    pub fn current(&mut self, id: &str, connection: Connection) -> Option<&mut GatewayState> {
        self.gateway(id).filter(|g| g.connection() == connection && g.roster.enabled)
    }
    pub fn status(&mut self, status: &ConnStatus, generation: u64) -> Option<bool> {
        let others_connected =
            self.gateways.iter().any(|g| {
                g.roster.id != status.gateway_id && g.roster.enabled && g.roster.status.phase == Phase::Connected && !super::rooms::is_rooms(&g.roster)
            });
        let g = self.gateway(&status.gateway_id).filter(|g| g.generation == generation)?;
        if status.phase != Phase::Connected && g.roster.status.phase == Phase::Connected {
            g.disconnect();
        }
        g.roster.status = status.clone();
        if status.phase != Phase::Connected {
            g.roster.loaded = false;
        }
        let actionable = match status.phase {
            Phase::Pairing | Phase::AuthFailed => true,
            Phase::NoConfig => status.gateway_id == OPENCLAW_GATEWAY_ID && !others_connected,
            _ => false,
        };
        let open = actionable && !self.auto_opened;
        self.auto_opened |= actionable;
        Some(open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn gateway() -> GatewayState {
        GatewayState { generation: 1, roster: GatewayRoster { id: "g".into(), enabled: true, ..Default::default() }, ..Default::default() }
    }
    fn lifecycle(g: &mut GatewayState, name: &str, phase: &str, run: Option<&str>) -> Vec<Effect> {
        g.event(name, &json!({"sessionKey":"agent:a:main","runId":run, if name == "chat" { "state" } else { "phase" }:phase}))
    }
    #[test]
    fn final_and_session_end_notify_once_in_either_order() {
        for reverse in [false, true] {
            let mut g = gateway();
            lifecycle(&mut g, "sessions.changed", "start", Some("r"));
            lifecycle(&mut g, "chat", "status", Some("r"));
            let mut endings = vec![("chat", "final"), ("sessions.changed", "end")];
            if reverse {
                endings.reverse();
            }
            let effects: Vec<_> = endings.iter().flat_map(|(n, p)| lifecycle(&mut g, n, p, Some("r"))).collect();
            assert_eq!(effects.iter().filter(|e| matches!(e, Effect::Reply(_))).count(), 1);
            assert!(g.running.is_empty());
            assert!(lifecycle(&mut g, "chat", "status", Some("r")).is_empty());
        }
    }
    #[test]
    fn stale_end_or_status_cannot_remove_a_new_turn() {
        let mut g = gateway();
        lifecycle(&mut g, "chat", "status", Some("old"));
        lifecycle(&mut g, "chat", "status", Some("new"));
        lifecycle(&mut g, "chat", "status", Some("old"));
        assert!(!lifecycle(&mut g, "sessions.changed", "end", Some("old")).iter().any(|e| matches!(e, Effect::Reply(_))));
        assert!(g.running.contains("agent:a:main"));
        assert!(lifecycle(&mut g, "chat", "final", Some("new")).iter().any(|e| matches!(e, Effect::Reply(_))));
    }
    #[test]
    fn abort_and_automated_work_do_not_send_reply_notifications() {
        for automated in [false, true] {
            let mut g = gateway();
            if automated {
                g.event("cron", &json!({"action":"started","job":{"agentId":"a","sessionTarget":"main"}}));
            }
            lifecycle(&mut g, "chat", "status", Some("r"));
            let effects = lifecycle(&mut g, "chat", if automated { "final" } else { "aborted" }, Some("r"));
            assert!(!effects.iter().any(|e| matches!(e, Effect::Reply(_))));
            assert!(g.running.is_empty());
        }
    }
    #[test]
    fn completion_does_not_depend_on_a_snapshot_running_flag() {
        let mut g = gateway();
        lifecycle(&mut g, "chat", "status", Some("r"));
        let ticket = g.begin_refresh();
        g.snapshot(ticket, None, Some(&json!({"sessions":[]}))).unwrap();
        assert!(g.running.is_empty());
        assert!(lifecycle(&mut g, "chat", "final", Some("r")).iter().any(|e| matches!(e, Effect::Reply(_))));
    }
    #[test]
    fn delayed_snapshots_cannot_overwrite_events_newer_requests_or_connections() {
        let mut g = gateway();
        let a = g.begin_refresh();
        let b = g.begin_refresh();
        assert!(!g.accepts(a));
        assert!(g.accepts(b));
        lifecycle(&mut g, "chat", "final", Some("r"));
        assert!(!g.accepts(b));
        let c = g.begin_refresh();
        g.disconnect();
        assert!(!g.accepts(c));
        let d = g.begin_refresh();
        g.generation += 1;
        assert!(!g.accepts(d));
    }
    #[test]
    fn superseded_refreshes_do_not_schedule_successors_that_starve_the_latest() {
        let mut g = gateway();
        // A at 0ms and B at 500ms, both taking 1s. Retrying A after 400ms
        // would invalidate B at 1400ms, then B would invalidate that retry.
        let a = g.begin_refresh();
        let b = g.begin_refresh();
        assert!(g.snapshot(a, None, None).is_none());
        assert!(!g.needs_refresh(a));
        assert!(g.snapshot(b, Some(&json!({"agents":[]})), Some(&json!({"sessions":[]}))).is_some());
        assert!(g.roster.loaded);
        assert!(!g.needs_refresh(b));
    }
    #[test]
    fn only_the_latest_refresh_retries_after_intervening_events() {
        let mut g = gateway();
        let a = g.begin_refresh();
        lifecycle(&mut g, "chat", "status", Some("r"));
        assert!(g.needs_refresh(a));
        let b = g.begin_refresh();
        assert!(!g.needs_refresh(a));
        assert!(!g.needs_refresh(b));
        lifecycle(&mut g, "chat", "final", Some("r"));
        assert!(g.needs_refresh(b));
        g.disconnect();
        assert!(!g.needs_refresh(b));
    }
    #[test]
    fn completed_turn_is_not_revived_by_a_lagging_roster() {
        let mut g = gateway();
        lifecycle(&mut g, "chat", "status", Some("r"));
        lifecycle(&mut g, "chat", "final", Some("r"));
        let ticket = g.begin_refresh();
        g.snapshot(ticket, None, Some(&json!({"sessions":[{"key":"agent:a:main","hasActiveRun":true}]}))).unwrap();
        assert!(g.running.is_empty());
    }
    #[test]
    fn resolved_approvals_cannot_return_via_duplicate_requests_or_snapshots() {
        let mut g = gateway();
        let c = g.connection();
        let approval = json!({"id":"approval","sessionKey":"agent:a:main"});
        assert!(g.event("exec.approval.requested", &approval).iter().any(|e| matches!(e, Effect::Approval(_))));
        assert!(!g.event("exec.approval.requested", &approval).iter().any(|e| matches!(e, Effect::Approval(_))));
        g.event("exec.approval.resolved", &approval);
        g.approval_snapshot(c, vec![approval.clone()]);
        g.event("exec.approval.requested", &approval);
        assert!(g.approvals.is_empty());
        g.disconnect();
        g.approval_snapshot(c, vec![approval]);
        assert!(g.approvals.is_empty());
    }
    #[test]
    fn replaced_removed_and_disabled_connections_ignore_late_statuses() {
        let mut state = State { gateways: vec![gateway()], ..Default::default() };
        let old = state.gateways[0].connection();
        state.gateways[0].generation = 2;
        assert!(state.status(&ConnStatus { gateway_id: "g".into(), phase: Phase::AuthFailed, ..Default::default() }, 1).is_none());
        assert!(state.current("g", old).is_none());
        let current = state.gateways[0].connection();
        state.gateways[0].roster.enabled = false;
        assert!(state.current("g", current).is_none());
        state.gateways.clear();
        assert!(state.current("g", current).is_none());
    }
    #[test]
    fn reconnect_clears_running_and_subscriptions_without_notifying() {
        let mut g = gateway();
        lifecycle(&mut g, "chat", "status", Some("r"));
        g.watched.insert("agent:a:main".into());
        let old = g.connection();
        g.disconnect();
        assert_ne!(g.connection(), old);
        assert!(g.running.is_empty());
        assert!(g.watched.is_empty());
        assert!(!lifecycle(&mut g, "chat", "final", Some("r")).iter().any(|e| matches!(e, Effect::Reply(_))));
    }
    #[test]
    fn shared_wire_fixture_keeps_the_roster_contract_and_notifies_once() {
        let fixture: Value = serde_json::from_str(include_str!("../../../tests/fixtures/chat-contract.json")).unwrap();
        let mut g = gateway();
        let effects: Vec<_> = fixture["events"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|frame| g.event(frame["event"].as_str().unwrap(), &frame["payload"]))
            .collect();
        assert_eq!(effects.iter().filter(|e| matches!(e, Effect::Reply(_))).count(), 1);
        assert!(g.running.is_empty());
        let roster = serde_json::to_value(&g.roster).unwrap();
        assert!(roster.get("defaultAgentId").is_some());
        assert!(roster.get("approvals").unwrap().is_array());
    }
}
