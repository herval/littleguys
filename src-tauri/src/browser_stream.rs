//! Live view of an OpenClaw guy's browser: a screencast WebSocket relayed to
//! the webview over a Tauri channel.
//!
//! `browser.request {POST /screencast}` mints a single-use ticket bound to our
//! gateway connection; we open `wsPath` on the gateway's own address and pass
//! every message through, prefixed with one byte so the webview can tell them
//! apart:
//! - `0` + UTF-8 JSON — the gateway's `{type: ready|meta|error, …}` messages
//! - `1` + the gateway's binary frame (4-byte BE header length, JSON header, JPEG)
//! - `2` + `{code, reason}` JSON — the stream ended (the panel decides whether to restart)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::gateway::client::{GatewayClient, RpcError};
use crate::hub::Hub;

const TEXT: u8 = 0;
const FRAME: u8 = 1;
const ENDED: u8 = 2;

/// Screencasts being relayed, by id. Sending on (or dropping) the stopper ends one.
#[derive(Default)]
pub struct Streams {
    next: AtomicU64,
    live: Mutex<HashMap<u64, oneshot::Sender<()>>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    profile: Option<String>,
    target_id: Option<String>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    quality: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStarted {
    id: u64,
    target_id: Option<String>,
    url: Option<String>,
}

fn openclaw(hub: &Hub, gateway: &str) -> Result<GatewayClient, RpcError> {
    let client = hub.client(gateway).ok_or_else(|| RpcError::new("UNKNOWN_GATEWAY", format!("no gateway {gateway}")))?;
    client
        .as_openclaw()
        .cloned()
        .ok_or_else(|| RpcError::new("UNSUPPORTED", "only OpenClaw guys have a browser littleguys can show"))
}

/// `wsPath` (`/browser/screencast?token=…`) on the gateway's own address.
fn screencast_url(gateway_url: &str, ws_path: &str) -> Option<String> {
    let (scheme, rest) = gateway_url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next().filter(|h| !h.is_empty())?;
    let scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => other,
    };
    let path = if ws_path.starts_with('/') { ws_path.to_string() } else { format!("/{ws_path}") };
    Some(format!("{scheme}://{host}{path}"))
}

fn tagged(tag: u8, bytes: &[u8]) -> InvokeResponseBody {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(tag);
    out.extend_from_slice(bytes);
    InvokeResponseBody::Raw(out)
}

/// Start relaying a guy's browser tab. Returns once the stream is open.
#[tauri::command]
pub async fn browser_stream_start(
    hub: State<'_, Arc<Hub>>,
    streams: State<'_, Arc<Streams>>,
    gateway: String,
    options: Option<StreamOptions>,
    on_message: Channel<InvokeResponseBody>,
) -> Result<StreamStarted, RpcError> {
    let options = options.unwrap_or_default();
    let client = openclaw(&hub, &gateway)?;
    let gateway_url = client.status().url.ok_or_else(|| RpcError::new("NOT_CONNECTED", "not connected to the gateway"))?;

    let mut body = serde_json::Map::new();
    if let Some(t) = &options.target_id {
        body.insert("targetId".into(), json!(t));
    }
    for (key, value) in [("maxWidth", options.max_width), ("maxHeight", options.max_height), ("quality", options.quality)] {
        if let Some(v) = value {
            body.insert(key.into(), json!(v));
        }
    }
    let query = options.profile.as_ref().map(|p| json!({ "profile": p })).unwrap_or_else(|| json!({}));
    let ticket = client
        .call("browser.request", json!({ "method": "POST", "path": "/screencast", "query": query, "body": body }))
        .await?;
    let ws_path = ticket["wsPath"].as_str().ok_or_else(|| RpcError::new("BAD_RESPONSE", "the gateway didn't return a screencast address"))?;
    let url = screencast_url(&gateway_url, ws_path).ok_or_else(|| RpcError::new("BAD_RESPONSE", format!("can't use gateway address {gateway_url}")))?;

    let mut request = url.as_str().into_client_request().map_err(|e| RpcError::new("BAD_RESPONSE", e.to_string()))?;
    if let Ok(ua) = HeaderValue::from_str(&format!("littleguys/{}", env!("CARGO_PKG_VERSION"))) {
        request.headers_mut().insert("User-Agent", ua);
    }
    let config = WebSocketConfig::default().max_message_size(Some(16 << 20)).max_frame_size(Some(16 << 20));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(RpcError::new("UNAVAILABLE", format!("couldn't open the browser stream: {e}"))),
        Err(_) => return Err(RpcError::new("TIMEOUT", "the browser stream didn't open in time")),
    };

    let id = streams.next.fetch_add(1, Ordering::SeqCst) + 1;
    let (stop, mut stopped) = oneshot::channel();
    streams.live.lock().unwrap().insert(id, stop);
    let registry = streams.inner().clone();
    tokio::spawn(async move {
        let (_sink, mut stream) = ws.split();
        let (code, reason) = loop {
            tokio::select! {
                _ = &mut stopped => break (1000, "stopped".to_string()),
                msg = stream.next() => match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        if on_message.send(tagged(FRAME, &bytes)).is_err() {
                            break (1001, "viewer went away".into());
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if on_message.send(tagged(TEXT, text.as_bytes())).is_err() {
                            break (1001, "viewer went away".into());
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        break frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1005, String::new()));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break (1006, e.to_string()),
                    None => break (1006, "connection closed".into()),
                },
            }
        };
        log::info!("[{gateway}] browser stream {id} ended ({code} {reason})");
        registry.live.lock().unwrap().remove(&id);
        let _ = on_message.send(tagged(ENDED, json!({ "code": code, "reason": reason }).to_string().as_bytes()));
    });

    Ok(StreamStarted {
        id,
        target_id: ticket["targetId"].as_str().map(str::to_string),
        url: ticket["url"].as_str().map(str::to_string),
    })
}

#[tauri::command]
pub fn browser_stream_stop(streams: State<'_, Arc<Streams>>, id: u64) {
    if let Some(stop) = streams.live.lock().unwrap().remove(&id) {
        let _ = stop.send(());
    }
}

/// One still of a tab, for browsers that can't stream (existing-session
/// profiles, no Playwright): `/screenshot` saves it on the gateway host, and
/// we fetch it over the gateway's media endpoint. Returns a `data:` URL.
#[tauri::command]
pub async fn browser_screenshot(
    hub: State<'_, Arc<Hub>>,
    gateway: String,
    profile: Option<String>,
    target_id: Option<String>,
) -> Result<Value, RpcError> {
    let client = openclaw(&hub, &gateway)?;
    let mut body = json!({ "type": "jpeg" });
    if let Some(t) = &target_id {
        body["targetId"] = json!(t);
    }
    let query = profile.as_ref().map(|p| json!({ "profile": p })).unwrap_or_else(|| json!({}));
    // A tab that isn't painting can hold a screenshot for a long time; give up well before the panel's next try would.
    let shot = client
        .call_with_timeout(
            "browser.request",
            json!({ "method": "POST", "path": "/screenshot", "query": query, "body": body }),
            Duration::from_secs(20),
        )
        .await?;
    let path = shot["path"].as_str().ok_or_else(|| RpcError::new("BAD_RESPONSE", "the gateway didn't return a screenshot"))?;
    let origin = client.http_origin().ok_or_else(|| RpcError::new("BAD_RESPONSE", "unknown gateway address"))?;
    let mut req = reqwest::Client::new()
        .get(format!("{origin}/__openclaw__/assistant-media"))
        .query(&[("source", path)])
        .timeout(Duration::from_secs(15));
    if let Some(token) = client.http_bearer() {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
    if !res.status().is_success() {
        return Err(RpcError::new("UNAVAILABLE", format!("the gateway refused the screenshot ({})", res.status())));
    }
    let mime = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = res.bytes().await.map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(json!({
        "src": format!("data:{mime};base64,{data}"),
        "targetId": shot["targetId"],
        "url": shot["url"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_ws_path_against_gateway_address() {
        assert_eq!(
            screencast_url("wss://attic.tail1234.ts.net", "/browser/screencast?token=abc").as_deref(),
            Some("wss://attic.tail1234.ts.net/browser/screencast?token=abc")
        );
        assert_eq!(
            screencast_url("ws://127.0.0.1:18789/", "browser/screencast?token=x").as_deref(),
            Some("ws://127.0.0.1:18789/browser/screencast?token=x")
        );
        assert_eq!(screencast_url("https://h:1/a?b", "/p").as_deref(), Some("wss://h:1/p"));
        assert_eq!(screencast_url("nonsense", "/p"), None);
    }

    #[test]
    fn tags_messages() {
        let InvokeResponseBody::Raw(bytes) = tagged(FRAME, &[9, 8]) else { panic!("raw") };
        assert_eq!(bytes, vec![FRAME, 9, 8]);
    }
}
