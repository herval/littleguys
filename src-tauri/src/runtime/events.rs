//! The lifecycle subset Activity consumes. Parse once at the adapter boundary;
//! preserve the original payload for webviews and phones.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RunPhase {
    Start,
    Status,
    Delta,
    End,
    Final,
    Error,
    Aborted,
}
impl RunPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Status => "status",
            Self::Delta => "delta",
            Self::End => "end",
            Self::Final => "final",
            Self::Error => "error",
            Self::Aborted => "aborted",
        }
    }
}
#[derive(Debug, PartialEq)]
pub(crate) struct RunEvent<'a> {
    pub session_key: &'a str,
    pub run_id: Option<&'a str>,
    pub phase: RunPhase,
}
impl<'a> RunEvent<'a> {
    pub fn decode(name: &str, payload: &'a Value) -> Option<Self> {
        let field = match name {
            "chat" => "state",
            "sessions.changed" => "phase",
            _ => return None,
        };
        let phase = match payload.get(field)?.as_str()? {
            "start" => RunPhase::Start,
            "status" => RunPhase::Status,
            "delta" => RunPhase::Delta,
            "end" => RunPhase::End,
            "final" => RunPhase::Final,
            "error" => RunPhase::Error,
            "aborted" => RunPhase::Aborted,
            _ => return None,
        };
        let session_key = payload.get("sessionKey")?.as_str().filter(|k| !k.is_empty())?;
        let run_id = payload.get("runId").and_then(Value::as_str).filter(|id| !id.is_empty());
        Some(Self { session_key, run_id, phase })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn legacy_events_and_future_extensions_are_compatible() {
        assert!(RunEvent::decode("sessions.changed", &json!({"sessionKey":"s","phase":"end"})).is_some());
        assert!(
            RunEvent::decode("chat", &json!({"sessionKey":"s","runId":"r","state":"status","phase":"thinking","future":true})).is_some()
        );
        assert!(RunEvent::decode("chat", &json!({"sessionKey":"s","state":"future"})).is_none());
        assert!(RunEvent::decode("chat", &json!({"state":"final"})).is_none());
    }
}
