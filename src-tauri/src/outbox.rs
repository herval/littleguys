//! Messages you wrote while a guy was working, held by littleguys until its turn ends.
//!
//! `chat.send` with `delivery: "queue"` lands here while the thread's turn is under way
//! (Activity knows which threads are working); otherwise it goes out at once. When the
//! turn ends (`chat` final or error), Activity hands everything waiting over as one
//! message. A stopped turn (`aborted`) keeps them waiting: you stopped it, so they go
//! when you say. Claude Code takes them sooner, in its Stop hook, so the same turn
//! carries on with them.
//!
//! Until then you can send one now (`chat.queue.send`) or take it back
//! (`chat.queue.remove`), and `chat.history` lists them (`queued`). Every change is a
//! `chat.queue {sessionKey, queued, sent?:{id, text}, accepted?:{id, receipt}, unsent?, error?}`
//! event. They're
//! kept in memory: quitting littleguys drops what's waiting.
//!
//! [`call`] and [`hand_over`] are the rules. Activity runs them for the Mac's windows and
//! paired phones; the bridge's demo host runs the same ones, so phones can be tested against it.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;

/// Whoever runs the threads: Activity in the app, the demo host in the bridge's tests and `bridge_demo`.
pub trait Threads {
    /// Whether this thread's turn is under way, as far as its events tell.
    fn is_running(&self, gateway: &str, key: &str) -> bool;
    /// Tell every window and phone that a thread's queue changed (a `chat.queue` event).
    fn queue_changed(&self, gateway: &str, event: &Value);
}

/// A call to a guy, with littleguys' queue in between. `chat.send` with `delivery: "queue"`
/// waits here while the thread works (and goes at once when it doesn't); `"steer"` asks the
/// guy to take it into the turn under way. `chat.queue.send {sessionKey, id, delivery?}` sends
/// one now, `chat.queue.remove {sessionKey, id}` takes one back, and `chat.history` lists
/// what's waiting (`queued`). Everything else goes straight to `forward`.
///
/// `steers`: the runtime takes `delivery: "steer"`. OpenClaw's gateway doesn't (it refuses
/// fields it doesn't know, and steers by default).
pub async fn call<'a, T, F, Fut>(
    threads: &T,
    gateway: &str,
    steers: bool,
    method: &'a str,
    mut params: Value,
    forward: F,
) -> Result<Value, RpcError>
where
    T: Threads + ?Sized,
    F: Fn(&'a str, Value) -> Fut,
    Fut: Future<Output = Result<Value, RpcError>>,
{
    let Some(key) = params.get("sessionKey").and_then(Value::as_str).map(str::to_string) else {
        return forward(method, params).await;
    };
    match method {
        "chat.send" => {
            let delivery = params.as_object_mut().and_then(|p| p.remove("delivery"));
            let delivery = delivery.as_ref().and_then(Value::as_str);
            if delivery == Some("queue") && threads.is_running(gateway, &key) {
                let text = params.get("message").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let attachments = params.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default();
                if text.is_empty() && attachments.is_empty() {
                    return Err(RpcError { code: "INVALID_REQUEST".into(), message: "Write something first.".into(), details: None });
                }
                let held = hold(gateway, &key, text, attachments);
                threads.queue_changed(gateway, &event(gateway, &key));
                return Ok(json!({ "runId": held.id, "status": "held" }));
            }
            if delivery == Some("steer") && steers {
                params["delivery"] = json!("steer");
            }
            forward(method, params).await
        }
        "chat.queue.send" => {
            // One you queued, now: into the turn under way, if the guy can (`delivery`).
            let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
            let held = remove(gateway, &key, id)
                .ok_or_else(|| RpcError { code: "NOT_FOUND".into(), message: "It's already been sent.".into(), details: None })?;
            let mut send = json!({ "sessionKey": key, "message": held.text, "idempotencyKey": held.id });
            if !held.attachments.is_empty() {
                send["attachments"] = Value::Array(held.attachments.clone());
            }
            if params.get("delivery").and_then(Value::as_str) == Some("steer") && steers {
                send["delivery"] = json!("steer");
            }
            threads.queue_changed(gateway, &sending(gateway, &key, &held.id, &held.text));
            let sent = forward("chat.send", send).await;
            match &sent {
                Ok(receipt) => threads.queue_changed(gateway, &accepted(gateway, &key, &held.id, receipt)),
                Err(_) => {
                    let id = held.id.clone();
                    restore(gateway, &key, vec![held]);
                    threads.queue_changed(gateway, &unsent(gateway, &key, &id, None));
                }
            }
            sent
        }
        "chat.queue.remove" => {
            let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
            let removed = remove(gateway, &key, id).is_some();
            threads.queue_changed(gateway, &event(gateway, &key));
            Ok(json!({ "ok": true, "removed": removed }))
        }
        "chat.history" => {
            let mut res = forward(method, params).await?;
            if let Some(obj) = res.as_object_mut() {
                obj.insert("queued".into(), shown(&list(gateway, &key)));
            }
            Ok(res)
        }
        _ => forward(method, params).await,
    }
}

/// Hands everything waiting for a thread over as one message, with `send` (its `chat.send`),
/// once its turn is over. What can't be sent waits again.
pub async fn hand_over<T, F, Fut>(threads: &T, gateway: &str, key: &str, send: F)
where
    T: Threads + ?Sized,
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value, RpcError>>,
{
    let items = take(gateway, key);
    if items.is_empty() {
        return;
    }
    let (text, attachments) = combine(&items);
    let id = uuid::Uuid::new_v4().to_string();
    let mut params = json!({ "sessionKey": key, "message": text, "idempotencyKey": id });
    if !attachments.is_empty() {
        params["attachments"] = Value::Array(attachments);
    }
    // Shown as sending first: the guy may put it in the thread before chat.send answers.
    threads.queue_changed(gateway, &sending(gateway, key, &id, &text));
    match send(params).await {
        Ok(receipt) => threads.queue_changed(gateway, &accepted(gateway, key, &id, &receipt)),
        Err(e) => {
            log::warn!("[{gateway}] couldn't send the queued messages for {key}: {}", e.message);
            restore(gateway, key, items);
            let error = format!("Couldn't send what was queued: {}", e.message);
            threads.queue_changed(gateway, &unsent(gateway, key, &id, Some(&error)));
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub id: String,
    pub text: String,
    /// As `chat.send` takes them (content included).
    pub attachments: Vec<Value>,
    pub at: u64,
}

impl Held {
    /// What the composer shows: no attachment content.
    fn shown(&self) -> Value {
        let files: Vec<Value> = self
            .attachments
            .iter()
            .map(|a| json!({ "type": a.get("type"), "fileName": a.get("fileName"), "mimeType": a.get("mimeType") }))
            .collect();
        json!({ "id": self.id, "text": self.text, "attachments": files, "at": self.at })
    }
}

type Key = (String, String);

static HELD: Mutex<BTreeMap<Key, Vec<Held>>> = Mutex::new(BTreeMap::new());

fn key(gateway: &str, session_key: &str) -> Key {
    (gateway.to_string(), session_key.to_string())
}

pub fn hold(gateway: &str, session_key: &str, text: String, attachments: Vec<Value>) -> Held {
    let held = Held { id: uuid::Uuid::new_v4().to_string(), text, attachments, at: now_ms() };
    HELD.lock().unwrap().entry(key(gateway, session_key)).or_default().push(held.clone());
    held
}

pub fn list(gateway: &str, session_key: &str) -> Vec<Held> {
    HELD.lock().unwrap().get(&key(gateway, session_key)).cloned().unwrap_or_default()
}

pub fn is_empty(gateway: &str, session_key: &str) -> bool {
    HELD.lock().unwrap().get(&key(gateway, session_key)).is_none_or(Vec::is_empty)
}

pub fn remove(gateway: &str, session_key: &str, id: &str) -> Option<Held> {
    let mut held = HELD.lock().unwrap();
    let k = key(gateway, session_key);
    let list = held.get_mut(&k)?;
    let at = list.iter().position(|h| h.id == id)?;
    let gone = list.remove(at);
    if list.is_empty() {
        held.remove(&k);
    }
    Some(gone)
}

/// Everything waiting for this thread, oldest first.
pub fn take(gateway: &str, session_key: &str) -> Vec<Held> {
    HELD.lock().unwrap().remove(&key(gateway, session_key)).unwrap_or_default()
}

/// Everything waiting, if it's all text (a hook can only hand text over).
pub fn take_text(gateway: &str, session_key: &str) -> Vec<String> {
    let mut held = HELD.lock().unwrap();
    let k = key(gateway, session_key);
    if held.get(&k).is_none_or(|list| list.iter().any(|h| !h.attachments.is_empty())) {
        return Vec::new();
    }
    held.remove(&k).unwrap_or_default().into_iter().map(|h| h.text).filter(|t| !t.trim().is_empty()).collect()
}

/// Puts back what couldn't be sent, ahead of anything written since.
pub fn restore(gateway: &str, session_key: &str, mut items: Vec<Held>) {
    if items.is_empty() {
        return;
    }
    let mut held = HELD.lock().unwrap();
    let list = held.entry(key(gateway, session_key)).or_default();
    items.append(list);
    *list = items;
}

/// Drops everything a gateway had waiting (it was removed).
pub fn forget(gateway: &str) {
    HELD.lock().unwrap().retain(|(g, _), _| g != gateway);
}

/// Several messages as one: their text a paragraph each, their attachments together.
pub fn combine(items: &[Held]) -> (String, Vec<Value>) {
    let text = items.iter().map(|h| h.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
    let attachments = items.iter().flat_map(|h| h.attachments.iter().cloned()).collect();
    (text, attachments)
}

pub fn shown(items: &[Held]) -> Value {
    Value::Array(items.iter().map(Held::shown).collect())
}

/// The `chat.queue` event for a thread whose queue changed.
pub fn event(gateway: &str, session_key: &str) -> Value {
    json!({ "sessionKey": session_key, "queued": shown(&list(gateway, session_key)) })
}

/// …as it hands `text` over (`id`: what to take back if that fails). It goes out before
/// the handing over, which the guy may answer with your message in its transcript.
pub fn sending(gateway: &str, session_key: &str, id: &str, text: &str) -> Value {
    let mut event = event(gateway, session_key);
    event["sent"] = json!({ "id": id, "text": text });
    event
}

/// …once `id` was handed over: what `chat.send` answered (its run), for the thread to match.
pub fn accepted(gateway: &str, session_key: &str, id: &str, receipt: &Value) -> Value {
    let mut event = event(gateway, session_key);
    event["accepted"] = json!({ "id": id, "receipt": receipt });
    event
}

/// …when handing `id` over failed, and it's waiting again.
pub fn unsent(gateway: &str, session_key: &str, id: &str, error: Option<&str>) -> Value {
    let mut event = event(gateway, session_key);
    event["unsent"] = json!(id);
    if let Some(error) = error {
        event["error"] = json!(error);
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    // The store is shared: each test uses its own gateway id.

    #[test]
    fn holds_in_order_and_takes_back_by_id() {
        let gw = "t-order";
        let a = hold(gw, "k", "first".into(), vec![]);
        let b = hold(gw, "k", "second".into(), vec![]);
        hold(gw, "other", "elsewhere".into(), vec![]);
        assert_eq!(list(gw, "k").iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(remove(gw, "k", &a.id).map(|h| h.text), Some("first".into()));
        assert_eq!(remove(gw, "k", &a.id), None);
        assert_eq!(remove(gw, "k", &b.id).map(|h| h.text), Some("second".into()));
        assert!(is_empty(gw, "k"));
        assert!(!is_empty(gw, "other"));
    }

    #[test]
    fn take_empties_and_restore_goes_first() {
        let gw = "t-restore";
        hold(gw, "k", "one".into(), vec![]);
        hold(gw, "k", "two".into(), vec![]);
        let taken = take(gw, "k");
        assert_eq!(taken.len(), 2);
        assert!(is_empty(gw, "k"));
        hold(gw, "k", "three".into(), vec![]);
        restore(gw, "k", taken);
        assert_eq!(list(gw, "k").iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
    }

    #[test]
    fn hooks_only_take_text() {
        let gw = "t-text";
        hold(gw, "k", "see this".into(), vec![]);
        hold(gw, "k", "and this".into(), vec![json!({ "type": "image", "fileName": "a.png", "content": "AAAA" })]);
        assert!(take_text(gw, "k").is_empty());
        assert_eq!(list(gw, "k").len(), 2);

        hold(gw, "plain", "hi".into(), vec![]);
        hold(gw, "plain", "  ".into(), vec![]);
        hold(gw, "plain", "there".into(), vec![]);
        assert_eq!(take_text(gw, "plain"), ["hi", "there"]);
        assert!(is_empty(gw, "plain"));
    }

    #[test]
    fn combines_text_and_attachments() {
        let gw = "t-combine";
        let image = json!({ "type": "image", "fileName": "a.png", "mimeType": "image/png", "content": "AAAA" });
        hold(gw, "k", " look at this ".into(), vec![image.clone()]);
        hold(gw, "k", "".into(), vec![]);
        hold(gw, "k", "then fix it".into(), vec![]);
        let items = take(gw, "k");
        let (text, attachments) = combine(&items);
        assert_eq!(text, "look at this\n\nthen fix it");
        assert_eq!(attachments, vec![image]);
        // The composer never gets the file's bytes.
        let shown = shown(&items);
        assert_eq!(shown[0]["attachments"][0]["fileName"], "a.png");
        assert!(shown[0]["attachments"][0].get("content").is_none());
    }

    #[test]
    fn forgets_a_removed_gateway() {
        hold("t-gone", "k", "bye".into(), vec![]);
        hold("t-kept", "k", "hi".into(), vec![]);
        forget("t-gone");
        assert!(is_empty("t-gone", "k"));
        assert!(!is_empty("t-kept", "k"));
    }
}
