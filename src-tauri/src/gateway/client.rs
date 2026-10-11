//! WebSocket client for the OpenClaw gateway (protocol v4).
//!
//! One long-lived connection, owned by the Rust core so the menu-bar guy and
//! notifications keep working while every window is hidden, and so the
//! gateway's browser-origin allowlist never applies (we send no Origin).
//! Webviews talk to the gateway through `call` (generic RPC) and receive every
//! event frame via the `on_event` hook.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

use super::identity::{build_payload_v3, DeviceAuthPayload, DeviceIdentity, DeviceToken, DeviceTokenStore};
use crate::openclaw::config::{self, Credential, Endpoint, GatewayMode, OpenclawPaths};

pub const PROTOCOL_VERSION: u64 = 4;
const CLIENT_ID: &str = "openclaw-macos";
const CLIENT_MODE: &str = "ui";
const ROLE: &str = "operator";
const PLATFORM: &str = "macos";
const DEVICE_FAMILY: &str = "Mac";
pub const SCOPES: [&str; 4] = ["operator.read", "operator.write", "operator.admin", "operator.approvals"];
const CAPS: [&str; 4] = ["tool-events", "session-scoped-events", "approvals", "exec-approvals"];
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self::local(code, message)
    }

    fn local(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: None }
    }

    fn from_shape(error: Option<&Value>) -> Self {
        let error = error.cloned().unwrap_or(Value::Null);
        Self {
            code: error.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN").to_string(),
            message: error.get("message").and_then(Value::as_str).unwrap_or("request failed").to_string(),
            details: error.get("details").cloned().filter(|d| !d.is_null()),
        }
    }

    fn detail_code(&self) -> Option<&str> {
        self.details.as_ref()?.get("code")?.as_str()
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    Starting,
    Connecting,
    Connected,
    /// The gateway wants this device approved (`openclaw devices approve`).
    Pairing,
    /// Credentials were rejected or are missing.
    AuthFailed,
    /// Gateway unreachable (not running, network down).
    Offline,
    /// No OpenClaw config and nothing listening locally.
    NoConfig,
    Error,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnStatus {
    pub gateway_id: String,
    pub gateway_name: String,
    /// Which kind of bot runtime this is, and what it can do.
    pub kind: crate::runtime::RuntimeKind,
    pub capabilities: crate::runtime::Capabilities,
    pub phase: Phase,
    pub mode: Option<GatewayMode>,
    pub url: Option<String>,
    pub message: Option<String>,
    pub detail_code: Option<String>,
    pub request_id: Option<String>,
    pub device_id: String,
    pub server_version: Option<String>,
    pub config_version: Option<String>,
    pub config_path: String,
    pub scopes: Vec<String>,
    pub retry_at_ms: Option<u64>,
    pub credential_issue: Option<String>,
    /// hello-ok payload of the live connection (snapshot, policy, features).
    pub hello: Option<Value>,
}

pub struct Hooks {
    pub status: Box<dyn Fn(&ConnStatus) + Send + Sync>,
    /// Called with (event name, payload, full frame) for every event frame.
    pub event: Box<dyn Fn(&str, &Value, &Value) + Send + Sync>,
    /// Called once per successful handshake, after the status flips to connected.
    pub connected: Box<dyn Fn() + Send + Sync>,
}

type Pending = HashMap<String, (String, oneshot::Sender<Result<Value, RpcError>>)>;

/// Where a gateway's address and credential come from.
#[derive(Debug, Clone)]
pub enum EndpointSource {
    /// The gateway `~/.openclaw/openclaw.json` points at (local or remote),
    /// re-read on every connect so config edits are picked up.
    OpenclawConfig,
    /// A gateway added in littleguys.
    Fixed { url: String },
}

struct Inner {
    id: String,
    name: Mutex<String>,
    source: EndpointSource,
    stopped: AtomicBool,
    identity: Arc<DeviceIdentity>,
    tokens: Arc<DeviceTokenStore>,
    paths: OpenclawPaths,
    credential_override: Mutex<Option<Credential>>,
    writer: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    pending: Mutex<Pending>,
    status: Mutex<ConnStatus>,
    wake: Notify,
    drop_connection: Notify,
    hooks: Hooks,
    app_version: String,
    instance_id: String,
}

#[derive(Clone)]
pub struct GatewayClient {
    inner: Arc<Inner>,
}

enum Outcome {
    Closed { was_connected: bool, reason: String },
    Unreachable(String),
    Pairing { request_id: Option<String>, hint: String, code: String },
    Auth { message: String, code: Option<String> },
    /// The gateway is refusing sign-ins from this address for a while;
    /// retrying early only extends the lockout.
    RateLimited { wait: Duration },
    RetryAfter(Duration),
    /// The gateway refuses this route for a reason only a change on the
    /// gateway host fixes (no credential gets past it).
    Blocked { message: String, code: String },
    Fatal(String),
}

impl GatewayClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        name: String,
        source: EndpointSource,
        credential: Option<Credential>,
        identity: Arc<DeviceIdentity>,
        tokens: Arc<DeviceTokenStore>,
        paths: OpenclawPaths,
        app_version: String,
        hooks: Hooks,
    ) -> Self {
        let status = ConnStatus {
            gateway_id: id.clone(),
            gateway_name: name.clone(),
            device_id: identity.device_id.clone(),
            config_path: paths.config_path.display().to_string(),
            ..Default::default()
        };
        Self {
            inner: Arc::new(Inner {
                id,
                name: Mutex::new(name),
                source,
                stopped: AtomicBool::new(false),
                identity,
                tokens,
                paths,
                credential_override: Mutex::new(credential),
                writer: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                status: Mutex::new(status),
                wake: Notify::new(),
                drop_connection: Notify::new(),
                hooks,
                app_version,
                instance_id: uuid::Uuid::new_v4().to_string(),
            }),
        }
    }

    pub fn start(&self) {
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move { run(inner).await });
    }

    pub fn status(&self) -> ConnStatus {
        self.inner.status.lock().unwrap().clone()
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn source(&self) -> &EndpointSource {
        &self.inner.source
    }

    pub fn set_name(&self, name: String) {
        *self.inner.name.lock().unwrap() = name.clone();
        set_status(&self.inner, |s| s.gateway_name = name);
    }

    /// Disconnect for good (gateway removed or disabled).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.drop_connection.notify_one();
        self.inner.wake.notify_one();
    }

    pub fn paths(&self) -> &OpenclawPaths {
        &self.inner.paths
    }

    /// The gateway's HTTP origin (`https://host` for `wss://host`), for its
    /// web endpoints.
    pub fn http_origin(&self) -> Option<String> {
        let url = self.inner.status.lock().unwrap().url.clone()?;
        let (scheme, rest) = url.split_once("://")?;
        let host = rest.split(['/', '?', '#']).next().filter(|h| !h.is_empty())?;
        let scheme = match scheme {
            "wss" => "https",
            "ws" => "http",
            other => other,
        };
        Some(format!("{scheme}://{host}"))
    }

    /// The device token for the current address, for the gateway's HTTP
    /// endpoints (`Authorization: Bearer …`).
    pub fn http_bearer(&self) -> Option<String> {
        let url = self.inner.status.lock().unwrap().url.clone()?;
        self.inner.tokens.load(&url, ROLE).map(|t| t.token)
    }

    /// Drop the current connection (if any) and reconnect immediately,
    /// re-reading the OpenClaw config.
    pub fn reconnect_now(&self) {
        self.inner.drop_connection.notify_one();
        self.inner.wake.notify_one();
    }

    /// Use a user-supplied gateway token/password instead of the config's
    /// (for SecretRef-backed credentials littleguys can't read).
    pub fn set_credential_override(&self, credential: Option<Credential>) {
        *self.inner.credential_override.lock().unwrap() = credential;
        self.reconnect_now();
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        let writer = self
            .inner
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| RpcError::local("NOT_CONNECTED", "not connected to the gateway"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id.clone(), (method.to_string(), tx));
        let frame = json!({ "type": "req", "id": id, "method": method, "params": params });
        if writer.send(Message::Text(frame.to_string().into())).is_err() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(RpcError::local("NOT_CONNECTED", "connection closed"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::local("DISCONNECTED", "connection closed before the gateway replied")),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(RpcError::local("TIMEOUT", format!("{method} timed out")))
            }
        }
    }
}

fn set_status(inner: &Inner, update: impl FnOnce(&mut ConnStatus)) {
    let snapshot = {
        let mut status = inner.status.lock().unwrap();
        update(&mut status);
        status.clone()
    };
    (inner.hooks.status)(&snapshot);
}

fn backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(5);
    Duration::from_secs(secs.min(30))
}

async fn run(inner: Arc<Inner>) {
    let mut attempt: u32 = 0;
    loop {
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let resolved = match &inner.source {
            EndpointSource::OpenclawConfig => config::load_config(&inner.paths)
                .map(|cfg| (config::resolve_endpoint(cfg.as_ref(), &inner.paths), cfg.is_some())),
            EndpointSource::Fixed { url } => Ok((fixed_endpoint(url), true)),
        };
        let (endpoint, has_config) = match resolved {
            Ok(pair) => pair,
            Err(e) => {
                set_status(&inner, |s| {
                    s.phase = Phase::Error;
                    s.message = Some(e.to_string());
                    s.hello = None;
                });
                wait_or_wake(&inner, Duration::from_secs(30)).await;
                continue;
            }
        };
        if endpoint.url.is_empty() {
            set_status(&inner, |s| {
                s.phase = Phase::NoConfig;
                s.mode = Some(endpoint.mode);
                s.message = Some("Your OpenClaw config is in remote mode but has no gateway.remote.url.".into());
                s.hello = None;
            });
            wait_or_wake(&inner, Duration::from_secs(30)).await;
            continue;
        }

        let credential = inner
            .credential_override
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| endpoint.credential.clone());
        set_status(&inner, |s| {
            s.phase = s.phase.max_connecting();
            s.mode = Some(endpoint.mode);
            s.url = Some(endpoint.url.clone());
            s.config_version = endpoint.config_version.clone();
            s.credential_issue = endpoint.credential_issue.clone();
            s.hello = None;
        });

        let outcome = connect_and_serve(&inner, &endpoint, &credential).await;
        let delay = match outcome {
            Outcome::Closed { was_connected, reason } => {
                if was_connected {
                    attempt = 0;
                }
                set_status(&inner, |s| {
                    s.phase = Phase::Offline;
                    s.message = Some(reason);
                    s.detail_code = None;
                    s.hello = None;
                });
                backoff(attempt)
            }
            Outcome::Unreachable(message) => {
                let no_install = !has_config && endpoint.mode == GatewayMode::Local;
                set_status(&inner, |s| {
                    s.phase = if no_install { Phase::NoConfig } else { Phase::Offline };
                    s.message = Some(message);
                    s.detail_code = None;
                    s.hello = None;
                });
                backoff(attempt)
            }
            Outcome::Pairing { request_id, hint, code } => {
                set_status(&inner, |s| {
                    s.phase = Phase::Pairing;
                    s.request_id = request_id;
                    s.message = Some(hint);
                    s.detail_code = Some(code);
                    s.hello = None;
                });
                Duration::from_secs(4)
            }
            Outcome::Auth { message, code } => {
                set_status(&inner, |s| {
                    s.phase = Phase::AuthFailed;
                    s.message = Some(message);
                    s.detail_code = code;
                    s.hello = None;
                });
                // Retrying a rejected credential just counts more failures
                // (gateways lock out after 10/min). Wait for the user to paste
                // a token / setup code (which wakes us), or 10 minutes.
                Duration::from_secs(600)
            }
            Outcome::RateLimited { wait } => {
                set_status(&inner, |s| {
                    s.phase = Phase::AuthFailed;
                    s.detail_code = Some("AUTH_RATE_LIMITED".into());
                    s.message = Some(format!(
                        "The gateway is temporarily refusing sign-ins from this Mac after too many failed attempts (by something else on this Mac, or a stale token). littleguys will try again in {} min.",
                        wait.as_secs().div_ceil(60)
                    ));
                    s.hello = None;
                });
                wait
            }
            Outcome::RetryAfter(delay) => delay,
            Outcome::Blocked { message, code } => {
                set_status(&inner, |s| {
                    s.phase = Phase::Error;
                    s.message = Some(message);
                    s.detail_code = Some(code);
                    s.hello = None;
                });
                // Picks up the fix on the gateway host without a click.
                Duration::from_secs(30)
            }
            Outcome::Fatal(message) => {
                set_status(&inner, |s| {
                    s.phase = Phase::Error;
                    s.message = Some(message);
                    s.detail_code = None;
                    s.hello = None;
                });
                Duration::from_secs(60)
            }
        };
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        attempt = attempt.saturating_add(1);
        set_status(&inner, |s| s.retry_at_ms = Some(super::identity::now_ms() + delay.as_millis() as u64));
        if wait_or_wake(&inner, delay).await {
            attempt = 0;
        }
    }
}

fn fixed_endpoint(url: &str) -> Endpoint {
    let loopback = ["ws://127.0.0.1", "ws://localhost", "ws://[::1]"].iter().any(|p| url.starts_with(p));
    Endpoint {
        mode: if loopback { GatewayMode::Local } else { GatewayMode::Remote },
        url: url.to_string(),
        port: None,
        credential: Credential::None,
        credential_issue: None,
        config_version: None,
    }
}

impl Phase {
    /// While (re)connecting, keep showing an actionable phase (pairing/auth)
    /// instead of flickering to "connecting" on every retry.
    fn max_connecting(self) -> Phase {
        match self {
            Phase::Pairing | Phase::AuthFailed | Phase::Offline | Phase::NoConfig | Phase::Error => self,
            _ => Phase::Connecting,
        }
    }
}

/// Returns true when woken early by `reconnect_now`.
async fn wait_or_wake(inner: &Inner, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = inner.wake.notified() => true,
    }
}

async fn next_json<S>(stream: &mut S) -> Result<Value, String>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|e| format!("invalid frame: {e}"));
            }
            Some(Ok(Message::Close(frame))) => {
                return Err(frame
                    .map(|f| f.reason.to_string())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "gateway closed the connection".into()));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.to_string()),
            None => return Err("connection closed".into()),
        }
    }
}

fn local_time_zone() -> Option<String> {
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let target = target.to_string_lossy();
    target.split("zoneinfo/").nth(1).map(str::to_string)
}

async fn connect_and_serve(inner: &Arc<Inner>, endpoint: &Endpoint, credential: &Credential) -> Outcome {
    let mut request = match endpoint.url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return Outcome::Fatal(format!("invalid gateway URL {}: {e}", endpoint.url)),
    };
    if let Ok(ua) = HeaderValue::from_str(&format!("littleguys/{}", inner.app_version)) {
        request.headers_mut().insert("User-Agent", ua);
    }
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(64 << 20))
        .max_frame_size(Some(64 << 20));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false);
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(tungstenite::Error::Http(response))) => {
            let retry_after = response.headers().get("Retry-After").and_then(|v| v.to_str().ok()?.trim().parse().ok());
            let body = response.body().as_deref().unwrap_or_default();
            return classify_upgrade_rejection(&endpoint.url, response.status().as_u16(), retry_after, body);
        }
        Ok(Err(e)) => return Outcome::Unreachable(format!("can't reach {}: {e}", endpoint.url)),
        Err(_) => return Outcome::Unreachable(format!("timed out reaching {}", endpoint.url)),
    };
    let (mut sink, mut stream) = ws.split();

    // 1. connect.challenge
    let challenge = match tokio::time::timeout(HANDSHAKE_TIMEOUT, next_json(&mut stream)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => return Outcome::Unreachable(e),
        Err(_) => return Outcome::Unreachable("gateway did not send a connect challenge".into()),
    };
    if challenge.get("event").and_then(Value::as_str) != Some("connect.challenge") {
        return Outcome::Fatal("unexpected first frame from gateway (not a connect challenge)".into());
    }
    let nonce = challenge["payload"]["nonce"].as_str().unwrap_or_default().to_string();
    let Some(signed_at_ms) = challenge["payload"]["ts"].as_u64() else {
        return Outcome::Fatal("gateway challenge had no timestamp".into());
    };

    // 2. signed connect request
    let stored = inner.tokens.load(&endpoint.url, ROLE);
    let (auth, signature_token, used_device_token) = select_auth(credential, stored.as_ref());
    let scopes: Vec<String> = match &stored {
        Some(token) if !token.scopes.is_empty() => token.scopes.clone(),
        _ => SCOPES.iter().map(|s| s.to_string()).collect(),
    };
    let payload = build_payload_v3(&DeviceAuthPayload {
        device_id: &inner.identity.device_id,
        client_id: CLIENT_ID,
        client_mode: CLIENT_MODE,
        role: ROLE,
        scopes: &scopes,
        signed_at_ms,
        token: signature_token.as_deref(),
        nonce: &nonce,
        platform: PLATFORM,
        device_family: DEVICE_FAMILY,
    });
    let mut client = json!({
        "id": CLIENT_ID,
        "displayName": "littleguys",
        "version": inner.app_version,
        "platform": PLATFORM,
        "deviceFamily": DEVICE_FAMILY,
        "mode": CLIENT_MODE,
        "instanceId": inner.instance_id,
    });
    if let Some(tz) = local_time_zone() {
        client["timeZone"] = json!(tz);
    }
    let mut params = json!({
        "minProtocol": PROTOCOL_VERSION,
        "maxProtocol": PROTOCOL_VERSION,
        "client": client,
        "role": ROLE,
        "scopes": scopes,
        "caps": CAPS,
        "userAgent": format!("littleguys/{}", inner.app_version),
        "device": {
            "id": inner.identity.device_id,
            "publicKey": inner.identity.public_key,
            "signature": inner.identity.sign(&payload),
            "signedAt": signed_at_ms,
            "nonce": nonce,
        },
    });
    if let Some(auth) = auth {
        params["auth"] = auth;
    }
    let connect_id = format!("connect-{}", uuid::Uuid::new_v4());
    let req = json!({ "type": "req", "id": connect_id, "method": "connect", "params": params });
    if let Err(e) = sink.send(Message::Text(req.to_string().into())).await {
        return Outcome::Unreachable(e.to_string());
    }

    // 3. hello-ok (or a typed rejection)
    let response = loop {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, next_json(&mut stream)).await {
            Ok(Ok(frame)) if frame.get("type").and_then(Value::as_str) == Some("res")
                && frame.get("id").and_then(Value::as_str) == Some(connect_id.as_str()) =>
            {
                break frame
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => return Outcome::Unreachable(e),
            Err(_) => return Outcome::Unreachable("gateway did not answer the connect request".into()),
        }
    };
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = RpcError::from_shape(response.get("error"));
        return classify_connect_error(inner, endpoint, error, used_device_token);
    }
    let hello = response.get("payload").cloned().unwrap_or(Value::Null);
    if let Some(device_token) = hello["auth"]["deviceToken"].as_str() {
        let granted: Vec<String> = hello["auth"]["scopes"]
            .as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let record = DeviceToken { token: device_token.to_string(), scopes: granted };
        if stored.as_ref() != Some(&record) {
            if let Err(e) = inner.tokens.store(&endpoint.url, ROLE, record) {
                log::warn!("could not persist device token: {e}");
            }
        }
    }
    if matches!(credential, Credential::Bootstrap(_)) {
        // Setup-code tokens are single-use; the device token takes over now.
        let mut current = inner.credential_override.lock().unwrap();
        if matches!(*current, Some(Credential::Bootstrap(_))) {
            *current = None;
        }
    }
    let tick_ms = hello["policy"]["tickIntervalMs"].as_u64().unwrap_or(15_000).max(1_000);

    // 4. serve
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    *inner.writer.lock().unwrap() = Some(tx);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    set_status(inner, |s| {
        s.phase = Phase::Connected;
        s.message = None;
        s.detail_code = None;
        s.request_id = None;
        s.retry_at_ms = None;
        s.server_version = hello["server"]["version"].as_str().map(str::to_string);
        s.scopes = hello["auth"]["scopes"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        s.capabilities = crate::runtime::Capabilities::openclaw_from_hello(&hello, &s.scopes);
        s.hello = Some(hello.clone());
    });
    (inner.hooks.connected)();

    let stale_after = Duration::from_millis(tick_ms * 2 + 5_000);
    let mut last_rx = Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let reason = loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    last_rx = Instant::now();
                    handle_frame(inner, &text);
                }
                Some(Ok(Message::Close(frame))) => {
                    break frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty())
                        .unwrap_or_else(|| "gateway closed the connection".into());
                }
                Some(Ok(_)) => last_rx = Instant::now(),
                Some(Err(e)) => break e.to_string(),
                None => break "connection closed".into(),
            },
            _ = watchdog.tick() => {
                if last_rx.elapsed() > stale_after {
                    break "gateway stopped responding".into();
                }
            }
            _ = inner.drop_connection.notified() => break "reconnecting".into(),
        }
    };

    *inner.writer.lock().unwrap() = None;
    writer_task.abort();
    let pending: Vec<_> = inner.pending.lock().unwrap().drain().collect();
    for (_, (_, tx)) in pending {
        let _ = tx.send(Err(RpcError::local("DISCONNECTED", "connection to the gateway was lost")));
    }
    Outcome::Closed { was_connected: true, reason }
}

/// Prefer this device's issued token (survives shared-token rotation), else
/// the shared gateway credential. Returns (auth object, signature token,
/// used stored device token).
fn select_auth(credential: &Credential, stored: Option<&DeviceToken>) -> (Option<Value>, Option<String>, bool) {
    if let Some(device) = stored {
        return (Some(json!({ "deviceToken": device.token })), Some(device.token.clone()), true);
    }
    match credential {
        Credential::Token(token) => (Some(json!({ "token": token })), Some(token.clone()), false),
        Credential::Password(password) => (Some(json!({ "password": password })), None, false),
        Credential::Bootstrap(token) => (Some(json!({ "bootstrapToken": token })), Some(token.clone()), false),
        Credential::None => (None, None, false),
    }
}

fn classify_connect_error(inner: &Inner, endpoint: &Endpoint, error: RpcError, used_device_token: bool) -> Outcome {
    let detail = error.detail_code().unwrap_or_default().to_string();
    let details = error.details.clone().unwrap_or(Value::Null);
    if detail == "PAIRING_REQUIRED" {
        return Outcome::Pairing {
            request_id: details["requestId"].as_str().map(str::to_string),
            hint: details["remediationHint"].as_str().unwrap_or(&error.message).to_string(),
            code: details["reason"].as_str().unwrap_or("not-paired").to_string(),
        };
    }
    if error.code == "UNAVAILABLE" {
        let ms = details["retryAfterMs"].as_u64().unwrap_or(2_000);
        return Outcome::RetryAfter(Duration::from_millis(ms.clamp(250, 30_000)));
    }
    if detail == "AUTH_RATE_LIMITED" || error.message.contains("too many failed authentication attempts") {
        // Default gateway lockout is 5 minutes; wait it out plus a little.
        let ms = details["retryAfterMs"].as_u64().unwrap_or(300_000);
        return Outcome::RateLimited { wait: Duration::from_millis(ms.clamp(30_000, 900_000) + 5_000) };
    }
    if detail == "PROTOCOL_MISMATCH" {
        return Outcome::Fatal(format!(
            "This gateway speaks a different protocol version (littleguys speaks v{PROTOCOL_VERSION}). Update OpenClaw or littleguys."
        ));
    }
    let auth_problem = detail.starts_with("AUTH_") || detail.starts_with("DEVICE_AUTH") || error.code == "NOT_PAIRED";
    if auth_problem && used_device_token {
        // Our stored device token went stale (revoked, gateway reset): forget
        // it and retry right away with the shared credential.
        let _ = inner.tokens.clear(&endpoint.url, ROLE);
        return Outcome::RetryAfter(Duration::from_millis(250));
    }
    if auth_problem || error.code == "FORBIDDEN" {
        return Outcome::Auth { message: error.message, code: Some(detail).filter(|d| !d.is_empty()) };
    }
    Outcome::Fatal(error.message)
}

/// The gateway answered the WebSocket upgrade with an HTTP error instead of a
/// connect challenge, i.e. before any credential was exchanged. OpenClaw
/// explains itself in a JSON body: `{error: {message, type}}`.
fn classify_upgrade_rejection(url: &str, status: u16, retry_after_secs: Option<u64>, body: &[u8]) -> Outcome {
    let error = serde_json::from_slice::<Value>(body).map(|v| v["error"].clone()).unwrap_or(Value::Null);
    match (status, error["type"].as_str()) {
        // Reached through a proxy (usually a Tailscale Serve route the user set
        // up by hand) that the gateway doesn't trust to say who's connecting.
        (403, Some("proxy_attribution_required")) => Outcome::Blocked {
            message: "The gateway refuses connections that come through a Tailscale Serve route (or other proxy) it doesn't manage. \
                      A token or setup code won't get past this; it needs a setting changed on that machine."
                .into(),
            code: "PROXY_ATTRIBUTION_REQUIRED".into(),
        },
        (429, _) => Outcome::RateLimited { wait: Duration::from_secs(retry_after_secs.unwrap_or(300).clamp(30, 900) + 5) },
        _ => {
            let reason = error["message"].as_str().filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
            Outcome::Unreachable(format!("{url} turned littleguys away (HTTP {status}){reason}"))
        }
    }
}

fn handle_frame(inner: &Inner, text: &str) {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        log::warn!("dropping unparseable gateway frame");
        return;
    };
    match frame.get("type").and_then(Value::as_str) {
        Some("res") => {
            let Some(id) = frame.get("id").and_then(Value::as_str) else { return };
            let mut pending = inner.pending.lock().unwrap();
            // The `agent` RPC acks with {status:"accepted"} before its final
            // response; keep waiting for the second frame.
            if let Some((method, _)) = pending.get(id) {
                if method == "agent" && frame["payload"]["status"].as_str() == Some("accepted") {
                    return;
                }
            }
            if let Some((_, tx)) = pending.remove(id) {
                let result = if frame.get("ok").and_then(Value::as_bool) == Some(true) {
                    Ok(frame.get("payload").cloned().unwrap_or(Value::Null))
                } else {
                    Err(RpcError::from_shape(frame.get("error")))
                };
                let _ = tx.send(result);
            }
        }
        Some("event") => {
            let name = frame.get("event").and_then(Value::as_str).unwrap_or_default();
            let payload = frame.get("payload").unwrap_or(&Value::Null);
            (inner.hooks.event)(name, payload, &frame);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_stored_device_token() {
        let stored = DeviceToken { token: "dev".into(), scopes: vec![] };
        let (auth, sig, used) = select_auth(&Credential::Token("shared".into()), Some(&stored));
        assert_eq!(auth.unwrap()["deviceToken"], "dev");
        assert_eq!(sig.as_deref(), Some("dev"));
        assert!(used);
    }

    #[test]
    fn falls_back_to_shared_token() {
        let (auth, sig, used) = select_auth(&Credential::Token("shared".into()), None);
        assert_eq!(auth.unwrap()["token"], "shared");
        assert_eq!(sig.as_deref(), Some("shared"));
        assert!(!used);
    }

    #[test]
    fn setup_code_token_is_sent_as_bootstrap() {
        let (auth, sig, used) = select_auth(&Credential::Bootstrap("boot".into()), None);
        assert_eq!(auth.unwrap()["bootstrapToken"], "boot");
        assert_eq!(sig.as_deref(), Some("boot"));
        assert!(!used);
    }

    #[test]
    fn untrusted_proxy_rejection_needs_a_gateway_fix() {
        let body = br#"{"error":{"message":"Proxy client attribution is required.","type":"proxy_attribution_required"}}"#;
        match classify_upgrade_rejection("wss://attic.tail1234.ts.net", 403, None, body) {
            Outcome::Blocked { code, .. } => assert_eq!(code, "PROXY_ATTRIBUTION_REQUIRED"),
            _ => panic!("expected Blocked"),
        }
    }

    #[test]
    fn other_upgrade_rejections_keep_the_gateways_reason() {
        let body = br#"{"error":{"message":"websocket handlers unavailable"}}"#;
        match classify_upgrade_rejection("wss://attic.tail1234.ts.net", 503, None, body) {
            Outcome::Unreachable(m) => assert!(m.contains("HTTP 503") && m.contains("websocket handlers unavailable"), "{m}"),
            _ => panic!("expected Unreachable"),
        }
        match classify_upgrade_rejection("wss://attic.tail1234.ts.net", 429, Some(120), b"") {
            Outcome::RateLimited { wait } => assert_eq!(wait, Duration::from_secs(125)),
            _ => panic!("expected RateLimited"),
        }
    }

    #[test]
    fn password_is_not_part_of_signature() {
        let (auth, sig, _) = select_auth(&Credential::Password("pw".into()), None);
        assert_eq!(auth.unwrap()["password"], "pw");
        assert!(sig.is_none());
    }
}
