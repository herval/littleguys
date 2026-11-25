//! Fan-out preserves the existing webview and paired-phone envelopes.
use super::*;
impl Activity {
    pub(super) fn broadcast_event(&self, gateway: &str, name: &str, payload: &Value) {
        let _ = self.app.emit("gw:event", json!({ "gateway": gateway, "event": name, "payload": payload }));
        let _ = self.bridge.send(crate::bridge::HostEvent::Event {
            gateway: gateway.to_string(),
            name: name.to_string(),
            payload: payload.clone(),
        });
    }
    pub(super) fn broadcast_status(&self, status: &ConnStatus) {
        let _ = self.app.emit("gw:status", status);
        if let Ok(raw) = serde_json::to_value(status) {
            let _ = self.bridge.send(crate::bridge::HostEvent::Status(raw));
        }
    }
    /// Paired phones keep a copy of the settings they show: send them the new one. Only
    /// when they change (windows get them in every roster).
    pub(super) fn settings_changed(&self) {
        let _ = self.bridge.send(crate::bridge::HostEvent::Settings(self.phone_settings()));
    }
    /// The same notification for paired phones, as a push (bridge/push.rs).
    pub(super) fn push_to_phones(
        &self,
        kind: NoticeKind,
        title: &str,
        body: &str,
        gateway: &str,
        session_key: Option<String>,
        approval_id: Option<String>,
    ) {
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
