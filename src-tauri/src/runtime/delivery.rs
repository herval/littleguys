//! Correlation for local CLI handoffs. A client send ID identifies a prompt,
//! not a runtime turn (several queued prompts may share one turn).
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct PendingMessage {
    pub text: String,
    pub client_send_id: Option<String>,
}
impl From<String> for PendingMessage {
    fn from(text: String) -> Self {
        Self { text, client_send_id: None }
    }
}
impl From<&str> for PendingMessage {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}
impl PendingMessage {
    pub fn new(text: String, params: &Value) -> Self {
        Self { text, client_send_id: params.get("idempotencyKey").and_then(Value::as_str).map(str::to_string) }
    }
}
pub fn text(messages: &[PendingMessage]) -> String {
    messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n\n")
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Delivery<'a> {
    session_key: &'a str,
    client_send_ids: Vec<&'a str>,
    status: &'a str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<&'a str>,
}
pub fn event(key: &str, messages: &[PendingMessage], error: Option<&str>) -> (&'static str, Value) {
    (
        "chat.delivery",
        serde_json::to_value(Delivery {
            session_key: key,
            client_send_ids: messages.iter().filter_map(|m| m.client_send_id.as_deref()).collect(),
            text: text(messages),
            status: if error.is_some() { "unknown" } else { "delivered" },
            error_message: error,
        })
        .expect("delivery contains only strings"),
    )
}
pub fn receipt(run_id: &str, params: &Value) -> Value {
    json!({ "runId": run_id, "status": "started", "delivery": "queued", "clientSendId": params.get("idempotencyKey") })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batched_prompts_keep_each_client_id_and_legacy_text() {
        let prompts = vec![
            PendingMessage::new("one".into(), &json!({"idempotencyKey":"send-1"})),
            PendingMessage::new("two".into(), &json!({"idempotencyKey":"send-2"})),
            "terminal".into(),
        ];
        assert_eq!(text(&prompts), "one\n\ntwo\n\nterminal");
        assert_eq!(
            event("thread", &prompts, None).1,
            json!({"sessionKey":"thread","clientSendIds":["send-1","send-2"],"status":"delivered","text":"one\n\ntwo\n\nterminal"})
        );
        assert_eq!(event("thread", &prompts, Some("timeout")).1["status"], "unknown");
        assert_eq!(receipt("run", &json!({"idempotencyKey":"send-1"}))["clientSendId"], "send-1");
    }
}
