//! What the bridge needs from the rest of the app: the bots, their calls and
//! events. `AppHost` (app_host.rs) is the real one; tests and `bridge_demo`
//! use fakes, so sessions are tested without Tauri.

use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

use crate::gateway::client::RpcError;

#[derive(Debug, Clone)]
pub enum HostEvent {
    /// One bot's connection status changed: its `ConnStatus`, as JSON.
    Status(Value),
    /// A runtime event (`gw:event`).
    Event { gateway: String, name: String, payload: Value },
    /// A bot was added, removed, renamed, turned on or off.
    BotsChanged,
    /// Something the Mac notifies about, for paired phones' pushes.
    Notice(Notice),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Reply,
    Routine,
    Approval,
    /// Takes down an approval's notification (a background push).
    ApprovalResolved,
}

impl NoticeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NoticeKind::Reply => "reply",
            NoticeKind::Routine => "routine",
            NoticeKind::Approval => "approval",
            NoticeKind::ApprovalResolved => "approval.resolved",
        }
    }
}

/// What the Mac shows in its own notification, and where it points.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
    /// The Mac's gateway id.
    pub gateway: String,
    pub session_key: Option<String>,
    pub approval_id: Option<String>,
}

/// A file a guy points at, ready to stream.
pub enum MediaFetch {
    Unavailable { code: Option<String>, reason: Option<String> },
    Available { mime_type: Option<String>, size_bytes: Option<u64>, body: BoxStream<'static, Result<Vec<u8>, String>> },
}

pub trait BridgeHost: Send + Sync + 'static {
    /// The `ConnStatus` (as JSON) of every bot the phone should see, in display order.
    fn statuses(&self) -> Vec<Value>;
    fn call(&self, gateway: &str, method: &str, params: Value, timeout: Option<Duration>) -> BoxFuture<'static, Result<Value, RpcError>>;
    fn reconnect(&self, gateway: &str) -> Result<(), RpcError>;
    fn media(
        &self,
        gateway: &str,
        source: &str,
        session_key: Option<&str>,
        agent_id: Option<&str>,
    ) -> BoxFuture<'static, Result<MediaFetch, RpcError>>;
    fn subscribe(&self) -> broadcast::Receiver<HostEvent>;
}

/// A `ConnStatus` as phones see it ("BotStatus"): Mac-only fields dropped,
/// `hello` cut down to `{policy, features}`, plus `kindLabel` and `endpoint`.
pub fn bot_status(raw: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "gatewayId",
        "gatewayName",
        "kind",
        "capabilities",
        "phase",
        "url",
        "message",
        "detailCode",
        "requestId",
        "serverVersion",
        "scopes",
        "retryAtMs",
    ] {
        out.insert(key.into(), raw.get(key).cloned().unwrap_or(Value::Null));
    }
    let hello = raw.get("hello").filter(|h| h.is_object()).map(|h| {
        json!({ "policy": h.get("policy").cloned().unwrap_or(Value::Null), "features": h.get("features").cloned().unwrap_or(Value::Null) })
    });
    out.insert("hello".into(), hello.unwrap_or(Value::Null));
    let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("openclaw");
    out.insert("kindLabel".into(), kind_label(kind).into());
    out.insert("endpoint".into(), raw.get("url").and_then(Value::as_str).and_then(endpoint).map(Value::from).unwrap_or(Value::Null));
    Value::Object(out)
}

pub fn kind_label(kind: &str) -> String {
    match kind {
        "openclaw" => "OpenClaw".into(),
        "hermes" => "Hermes".into(),
        "acp" => "ACP agent".into(),
        "claudecode" => "Claude Code".into(),
        other => other.to_string(),
    }
}

/// `scheme://host[:port][/path]`, lowercased and without a trailing slash, for
/// network addresses only (a local agent has none).
fn endpoint(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "ws" | "wss" | "http" | "https") {
        return None;
    }
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, String::new()));
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{}{}", host.to_ascii_lowercase(), path.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_status_drops_mac_only_fields() {
        let raw = json!({
            "gatewayId": "gw-1", "gatewayName": "Studio", "kind": "openclaw",
            "capabilities": {"routines": true}, "phase": "connected", "mode": "remote",
            "url": "WSS://Studio.ts.net/", "deviceId": "secret-ish", "configPath": "/Users/x/.openclaw/openclaw.json",
            "configVersion": "1", "credentialIssue": "x", "scopes": ["operator.admin"],
            "hello": {"policy": {"maxPayload": 1}, "features": {"methods": []}, "snapshot": {"big": true}},
        });
        let s = bot_status(&raw);
        for gone in ["mode", "deviceId", "configPath", "configVersion", "credentialIssue"] {
            assert!(s.get(gone).is_none(), "{gone} leaked");
        }
        assert_eq!(s["hello"], json!({"policy": {"maxPayload": 1}, "features": {"methods": []}}));
        assert_eq!(s["kindLabel"], "OpenClaw");
        assert_eq!(s["endpoint"], "wss://studio.ts.net");
        assert_eq!(s["message"], Value::Null);
    }

    #[test]
    fn local_agents_have_no_endpoint() {
        let s = bot_status(&json!({"gatewayId": "acp-1", "kind": "acp", "url": "stdio"}));
        assert_eq!(s["endpoint"], Value::Null);
        assert_eq!(s["kindLabel"], "ACP agent");
        assert_eq!(bot_status(&json!({"kind": "claudecode"}))["kindLabel"], "Claude Code");
        assert_eq!(bot_status(&json!({"kind": "zeroclaw"}))["kindLabel"], "zeroclaw");
    }
}
