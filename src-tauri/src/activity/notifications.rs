use super::*;
#[cfg(not(any(target_os = "macos", windows)))]
use tauri_plugin_notification::NotificationExt;

impl Activity {
    // ---- notifications ---------------------------------------------------------------

    pub(super) fn main_window_focused(&self) -> bool {
        self.app.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false)
    }

    /// "🦊 Fox", or "🦊 Fox · attic" once there's more than one gateway.
    pub(super) fn agent_label(&self, gateway: &str, agent_id: &str) -> String {
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
    pub(super) fn notify_reply_later(self: &Arc<Self>, gateway: String, key: String, connection: Connection) {
        if !notifiable_session(&key)
            || agent_id_from_key(&key).is_some_and(|a| self.is_hidden(&gateway, &a) || !self.notify_prefs(&gateway, &a).replies)
        {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(900)).await;
            if this.state.lock().unwrap().current(&gateway, connection).is_none_or(|g| g.running.contains(&key)) {
                return;
            }
            let agent = agent_id_from_key(&key).unwrap_or_default();
            if this.is_hidden(&gateway, &agent) || !this.notify_prefs(&gateway, &agent).replies {
                return;
            }
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
            let nav = Navigate { gateway_id: Some(gateway.clone()), agent_id: Some(agent_id), session_key: Some(key.clone()), view: None };
            this.show_notification(&title, &preview, nav);
            this.push_to_phones(NoticeKind::Reply, &title, &preview, &gateway, Some(key), None);
        });
    }

    pub(super) fn notify_routine(&self, gateway: &str, payload: &Value) {
        let job = payload.get("job").cloned().unwrap_or(Value::Null);
        let name = job.get("name").and_then(Value::as_str).unwrap_or("Routine");
        let agent_id = job.get("agentId").and_then(Value::as_str).unwrap_or_default();
        if (!agent_id.is_empty() && self.is_hidden(gateway, agent_id))
            || !routine_run_notifies(payload, self.notify_prefs(gateway, agent_id).routines)
        {
            return;
        }
        let status = payload.get("status").and_then(Value::as_str).unwrap_or("ok");
        let summary = payload.get("summary").and_then(Value::as_str).map(|s| truncate(s, 180));
        let title = if agent_id.is_empty() { name.to_string() } else { format!("{} · {name}", self.agent_label(gateway, agent_id)) };
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
        let key = payload.get("sessionKey").and_then(Value::as_str).map(str::to_string).or_else(|| routine_target(payload));
        self.push_to_phones(NoticeKind::Routine, &title, &body, gateway, key, None);
    }

    pub(super) fn notify_approval(&self, gateway: &str, payload: &Value) {
        let command = payload
            .pointer("/presentation/commandText")
            .or_else(|| payload.pointer("/request/command"))
            .or_else(|| payload.get("command"))
            .and_then(Value::as_str)
            .map(|c| truncate(c, 120))
            .unwrap_or_else(|| "a command".into());
        let agent_id = payload.pointer("/request/agentId").or_else(|| payload.get("agentId")).and_then(Value::as_str).unwrap_or_default();
        let who = if agent_id.is_empty() { "A little guy".to_string() } else { self.agent_label(gateway, agent_id) };
        let (title, body) = (approval_title(&who, payload), format!("Wants to run: {command}"));
        let key = payload.get("sessionKey").or_else(|| payload.pointer("/request/sessionKey")).and_then(Value::as_str).map(str::to_string);
        let nav = Navigate {
            gateway_id: Some(gateway.to_string()),
            agent_id: Some(agent_id.to_string()).filter(|a| !a.is_empty()),
            session_key: key.clone(),
            view: None,
        };
        self.show_notification(&title, &body, nav);
        self.push_to_phones(NoticeKind::Approval, &title, &body, gateway, key, approval_id(payload));
    }

    /// Shows a notification; clicking it opens `nav` (macOS and Windows — the
    /// notification plugin ignores clicks on desktop, so those go through
    /// mac-notification-sys and Windows toasts directly).
    pub(super) fn show_notification(&self, title: &str, body: &str, nav: Navigate) {
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
            std::thread::spawn(move || {
                match mac_notification_sys::Notification::new().title(&title).message(&body).wait_for_click(true).send() {
                    Ok(mac_notification_sys::NotificationResponse::Click) => {
                        let handle = app.clone();
                        let _ = app.run_on_main_thread(move || crate::windows::open_main(&handle, Some(nav)));
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("notification failed: {e}"),
                }
            });
        }
        #[cfg(windows)]
        {
            use tauri_winrt_notification::Toast;
            // Toasts carry the installed app's identity; a build run from target\ has
            // none (no Start menu shortcut), so it borrows PowerShell's, like the plugin.
            let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
            let unbundled = exe_dir.is_some_and(|d| d.ends_with("target\\debug") || d.ends_with("target\\release"));
            let app_id = if unbundled { Toast::POWERSHELL_APP_ID.to_string() } else { self.app.config().identifier.clone() };
            let app = self.app.clone();
            let toast = Toast::new(&app_id).title(title).text1(body).on_activated(move |_| {
                let (handle, nav) = (app.clone(), nav.clone());
                let _ = app.run_on_main_thread(move || crate::windows::open_main(&handle, Some(nav)));
                Ok(())
            });
            if let Err(e) = toast.show() {
                log::warn!("notification failed: {e}");
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = nav;
            if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
                log::warn!("notification failed: {e}");
            }
        }
    }
}
