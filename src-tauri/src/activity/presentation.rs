use super::*;

impl Activity {
    pub(super) fn snapshot(&self) -> Roster {
        let mut state = self.state.lock().unwrap();
        for g in state.gateways.iter_mut() {
            g.roster.running = g.running.iter().cloned().collect();
            g.roster.approvals = g.approvals.values().cloned().collect();
        }
        let prefs = self.preferences.snapshot();
        let hidden = prefs.hidden_guys;
        let notifications = prefs.notifications;
        let faces = self.avatars.snapshot();
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
        Roster { gateways, hidden, notifications, faces: with_faces, coder: crate::code::prefs(), updated_at: now_ms() }
    }

    /// Streamed text arrives in bursts across every running thread; one roster
    /// update per beat is plenty for a preview line.
    pub(super) fn publish_soon(self: &Arc<Self>) {
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

    pub(super) fn publish(&self) {
        let roster = self.snapshot();
        let _ = self.app.emit("lg:roster", &roster);
        let automated: HashMap<String, HashSet<String>> =
            self.state.lock().unwrap().gateways.iter().map(|g| (g.roster.id.clone(), g.automated.clone())).collect();
        self.tray.set_mood(mood(&roster, &automated));
    }
}

pub(super) fn mood(roster: &Roster, automated: &HashMap<String, HashSet<String>>) -> Mood {
    let enabled: Vec<&GatewayRoster> = roster.gateways.iter().filter(|g| g.enabled).collect();
    let connected: Vec<&&GatewayRoster> = enabled.iter().filter(|g| g.status.phase == Phase::Connected).collect();
    let shown =
        |g: &GatewayRoster, key: &str| agent_id_from_key(key).is_none_or(|agent| !roster.hidden.contains(&format!("{}/{agent}", g.id)));
    if connected.iter().any(|g| !g.approvals.is_empty())
        || enabled.iter().any(|g| matches!(g.status.phase, Phase::Pairing | Phase::AuthFailed))
    {
        Mood::NeedsYou
    } else if connected.iter().any(|g| {
        // Routine runs happen in the background; only work you're waiting on animates.
        let by_routine = automated.get(&g.id);
        g.running.iter().any(|k| shown(g, k) && !k.contains(":cron:") && !by_routine.is_some_and(|a| a.contains(k)))
    }) {
        Mood::Working
    } else if connected
        .iter()
        .any(|g| g.sessions.iter().any(|r| is_notable_unread(r) && r.get("key").and_then(Value::as_str).is_some_and(|k| shown(g, k))))
    {
        Mood::News
    } else if !connected.is_empty() {
        Mood::Idle
    } else if enabled.iter().any(|g| matches!(g.status.phase, Phase::Starting | Phase::Connecting | Phase::NoConfig)) {
        Mood::Setup
    } else {
        Mood::Offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_prioritizes_approvals_then_visible_user_work() {
        let key = "agent:a:main".to_string();
        let mut roster = Roster {
            gateways: vec![GatewayRoster {
                id: "g".into(),
                enabled: true,
                status: ConnStatus { phase: Phase::Connected, ..Default::default() },
                running: vec![key.clone()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(mood(&roster, &HashMap::new()), Mood::Working);
        roster.hidden = vec!["g/a".into()];
        assert_eq!(mood(&roster, &HashMap::new()), Mood::Idle);
        roster.gateways[0].approvals = vec![json!({"id":"approval"})];
        assert_eq!(mood(&roster, &HashMap::new()), Mood::NeedsYou);
        roster.hidden.clear();
        roster.gateways[0].approvals.clear();
        assert_eq!(mood(&roster, &HashMap::from([("g".into(), HashSet::from([key]))])), Mood::Idle);
        roster.gateways[0].running.clear();
        roster.gateways[0].sessions = vec![json!({"key":"agent:a:main","unread":true})];
        assert_eq!(mood(&roster, &HashMap::new()), Mood::News);
        roster.gateways[0].enabled = false;
        assert_eq!(mood(&roster, &HashMap::new()), Mood::Offline);
    }
}
