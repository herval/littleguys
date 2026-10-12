//! One phone session (docs/BRIDGE-PROTOCOL.md "Session", "Frames"): the Noise
//! IK handshake, then requests from the phone and events to it, as records.
//!
//! Three tasks per session: this one reads, a writer owns the socket's send
//! half (responses, media, events, ticks, closing), and a pump turns host
//! events into phone events, closing with 4008 when the phone falls behind.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::host::{bot_status, HostEvent, MediaFetch};
use super::keys::{prologue, NOISE_PARAMS};
use super::pairing::{b64, Phone, ProofCheck};
use super::records::{self, Joiner, Record, MAX_NOISE_MESSAGE, MAX_STREAM_CHUNK};
use super::{CloseReason, Inner};
use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;

pub const PROTOCOL: u64 = 1;
/// The biggest JSON frame either side sends (`policy.maxPayload`).
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
pub const MAX_MEDIA_BYTES: u64 = 100 * 1024 * 1024;
const EVENT_QUEUE: usize = 2_000;
const MAX_STREAMS: usize = 4;
const MAX_CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// What fits in handshake message 2 after the ephemeral key and the tag.
const MAX_HELLO: usize = MAX_NOISE_MESSAGE - 32 - 16;

type Sink<S> = SplitSink<WebSocketStream<S>, Message>;
type Transport = Arc<Mutex<TransportState>>;

enum Admission {
    Known,
    Pairing { secret_id: u64 },
}

enum Out {
    Frame(Vec<u8>),
    Stream { id: u32, bytes: Vec<u8>, end: bool, aborted: bool },
    Close(u16, &'static str),
}

pub(super) async fn serve<S>(inner: Arc<Inner>, ws: WebSocketStream<S>, ip: IpAddr, deadline: Instant)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();

    // ---- handshake -------------------------------------------------------------
    let Ok(Some(first)) = tokio::time::timeout_at(deadline, next_binary(&mut stream)).await else {
        return;
    };
    let prologue = prologue(&inner.keys.mac_id());
    let Ok(mut hs) = snow::Builder::new(NOISE_PARAMS.parse().expect("valid Noise params"))
        .local_private_key(inner.keys.noise_private())
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_responder())
    else {
        return;
    };
    let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
    let Ok(n) = hs.read_message(&first, &mut buf) else {
        inner.note_failure(ip);
        close(&mut sink, 4001, "handshake failed").await;
        return;
    };
    let phone_key = b64(hs.get_remote_static().unwrap_or_default());
    let msg1: Value = serde_json::from_slice(&buf[..n]).unwrap_or(Value::Null);
    // Subscribe before reading the bot list, so nothing falls in between.
    let events = inner.host.subscribe();

    let admission = match admit(&inner, ip, &phone_key, &msg1) {
        Ok(a) => a,
        Err(error) => {
            if error["code"] != "RATE_LIMITED" {
                inner.note_failure(ip);
            }
            log::info!("bridge: turned away a phone ({})", error["code"]);
            let payload = serde_json::to_vec(&json!({ "error": error })).unwrap_or_default();
            if let Ok(n) = hs.write_message(&payload, &mut buf) {
                let _ = sink.send(Message::Binary(buf[..n].to_vec().into())).await;
            }
            close(&mut sink, 4003, "rejected").await;
            return;
        }
    };

    let bots: Vec<Value> = inner.host.statuses().iter().map(bot_status).collect();
    let listed: HashSet<String> = bots.iter().filter_map(|b| b["gatewayId"].as_str().map(str::to_string)).collect();
    let mut hello = hello(&inner, bots.clone(), &phone_key);
    let mut payload = serde_json::to_vec(&hello).unwrap_or_default();
    // Too many bots for one Noise message: they follow as a `bots` event.
    let bots_follow = payload.len() > MAX_HELLO;
    if bots_follow {
        hello["bots"] = json!([]);
        payload = serde_json::to_vec(&hello).unwrap_or_default();
    }
    let Ok(n) = hs.write_message(&payload, &mut buf) else { return };
    if sink.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
        return;
    }
    let Ok(transport) = hs.into_transport_mode() else { return };
    let transport: Transport = Arc::new(Mutex::new(transport));

    // ---- session -----------------------------------------------------------------
    let client = msg1.get("client").cloned().unwrap_or(Value::Null);
    let (close_tx, close_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::channel(64);
    let (ev_tx, ev_rx) = mpsc::channel(EVENT_QUEUE);
    let mut session_id = None;
    let mut pending = match admission {
        Admission::Known => {
            session_id = Some(inner.register_session(&phone_key, ip, close_tx.clone()));
            None
        }
        Admission::Pairing { secret_id } => Some(secret_id),
    };
    log::info!("bridge: phone connected ({})", if pending.is_some() { "pairing" } else { "paired" });

    let mut writer = tokio::spawn(writer(sink, transport.clone(), out_rx, ev_rx, close_rx, inner.tick_interval()));
    let pump = tokio::spawn(pump(inner.clone(), events, ev_tx, close_tx.clone(), listed, bots_follow.then_some(bots)));
    let session = Arc::new(Session {
        inner: inner.clone(),
        phone: phone_key.clone(),
        out: out_tx,
        streams: AtomicUsize::new(0),
        next_stream: AtomicU32::new(0),
        id: AtomicU64::new(session_id.unwrap_or(0)),
    });

    let mut joiner = Joiner::new(MAX_PAYLOAD);
    let mut plain = vec![0u8; MAX_NOISE_MESSAGE];
    loop {
        let msg = tokio::select! {
            msg = stream.next() => msg,
            _ = &mut writer => break,
        };
        let bytes = match msg {
            Some(Ok(Message::Binary(b))) => b,
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Text(_))) => {
                let _ = close_tx.send(CloseReason::Protocol);
                continue;
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
        };
        let decrypted = transport.lock().unwrap().read_message(&bytes, &mut plain);
        let Ok(n) = decrypted else {
            let _ = close_tx.send(CloseReason::Protocol);
            continue;
        };
        if let Some(secret_id) = pending.take() {
            let phone = Phone {
                key: phone_key.clone(),
                name: client["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or("iPhone").trim().to_string(),
                model: client["model"].as_str().unwrap_or_default().to_string(),
                paired_at_ms: now_ms(),
                last_seen_at_ms: now_ms(),
                push: None,
            };
            if !inner.commit_pairing(secret_id, phone) {
                let _ = close_tx.send(CloseReason::Protocol);
                continue;
            }
            log::info!("bridge: paired a phone");
            let id = inner.register_session(&phone_key, ip, close_tx.clone());
            session.id.store(id, Ordering::SeqCst);
            session_id = Some(id);
        }
        match records::parse(&plain[..n]) {
            Ok(Record::Json { chunk, more }) => match joiner.push(chunk, more) {
                Ok(Some(frame)) => dispatch(session.clone(), &frame),
                Ok(None) => {}
                Err(_) => {
                    let _ = close_tx.send(CloseReason::Protocol);
                }
            },
            // Only the Mac sends media.
            Ok(Record::Stream { .. }) => {}
            Err(_) => {
                let _ = close_tx.send(CloseReason::Protocol);
            }
        }
    }

    pump.abort();
    if !writer.is_finished() {
        // The phone went away first: give the writer a moment to say goodbye.
        let _ = close_tx.send(CloseReason::Shutdown);
        if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
            writer.abort();
        }
    }
    if let Some(id) = session_id {
        inner.unregister_session(id);
    }
    log::info!("bridge: phone disconnected");
}

/// Who may in: a paired phone, or one with a good proof for an open pairing.
fn admit(inner: &Inner, ip: IpAddr, phone_key: &str, msg1: &Value) -> Result<Admission, Value> {
    if let Some(retry) = inner.rate_limited(ip) {
        return Err(json!({ "code": "RATE_LIMITED", "message": "Too many tries. Wait a minute.", "retryAfterMs": retry }));
    }
    if msg1["v"].as_u64() != Some(PROTOCOL) {
        return Err(json!({ "code": "PROTOCOL_MISMATCH", "message": "This version of littleguys for Mac speaks bridge protocol 1." }));
    }
    if let Some(proof) = msg1["pair"]["proof"].as_str() {
        let proof = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, proof).unwrap_or_default();
        let check = inner.pairings.lock().unwrap().check(&proof, phone_key, &inner.keys.noise_b64(), now_ms());
        return match check {
            ProofCheck::Match(secret_id) => Ok(Admission::Pairing { secret_id }),
            ProofCheck::Expired => Err(json!({ "code": "PAIRING_EXPIRED", "message": "That code expired. Make a new one on your Mac." })),
            ProofCheck::Bad => Err(json!({ "code": "BAD_PROOF", "message": "That code isn't from this Mac." })),
        };
    }
    if inner.is_paired(phone_key) {
        Ok(Admission::Known)
    } else {
        Err(json!({ "code": "NOT_PAIRED", "message": "This Mac doesn't know this phone. Pair it again." }))
    }
}

fn hello(inner: &Inner, bots: Vec<Value>, phone: &str) -> Value {
    let mut hello = json!({
        "protocol": PROTOCOL,
        "mac": { "name": inner.mac_name(), "version": inner.app_version(), "id": inner.keys.id_b64() },
        "bots": bots,
        "policy": {
            "maxPayload": MAX_PAYLOAD,
            "tickIntervalMs": inner.tick_interval().as_millis() as u64,
            "maxMediaBytes": MAX_MEDIA_BYTES,
        },
    });
    if let Some(relay) = inner.relay_url() {
        hello["push"] = json!({ "relay": relay, "registered": inner.has_push(phone) });
    }
    hello
}

async fn next_binary<S>(stream: &mut SplitStream<WebSocketStream<S>>) -> Option<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match stream.next().await? {
            Ok(Message::Binary(b)) => return Some(b.to_vec()),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
            _ => return None,
        }
    }
}

async fn close<S>(sink: &mut Sink<S>, code: u16, reason: &'static str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
    let _ = sink.send(Message::Close(Some(frame))).await;
    let _ = sink.close().await;
}

/// Owns the send half: responses and media (in order), events (numbered), ticks.
async fn writer<S>(
    mut sink: Sink<S>,
    transport: Transport,
    mut out: mpsc::Receiver<Out>,
    mut events: mpsc::Receiver<(String, Value)>,
    mut closing: mpsc::UnboundedReceiver<CloseReason>,
    tick: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut seq: u64 = 0;
    let mut ticker = tokio::time::interval_at(Instant::now() + tick, tick);
    let mut cipher = vec![0u8; MAX_NOISE_MESSAGE];
    let event = |seq: &mut u64, name: &str, payload: Value| {
        *seq += 1;
        serde_json::to_vec(&json!({ "type": "event", "event": name, "payload": payload, "seq": *seq })).unwrap_or_default()
    };
    loop {
        let plaintexts: Vec<Vec<u8>> = tokio::select! {
            reason = closing.recv() => {
                let (code, text) = match reason.unwrap_or(CloseReason::Shutdown) {
                    CloseReason::Removed => {
                        let frame = event(&mut seq, "phone.removed", json!({}));
                        let _ = send_records(&mut sink, &transport, &mut cipher, records::json_records(&frame)).await;
                        (4010, "removed")
                    }
                    CloseReason::Shutdown => (1000, "bye"),
                    CloseReason::TooSlow => (4008, "too slow"),
                    CloseReason::Protocol => (4000, "protocol error"),
                };
                close(&mut sink, code, text).await;
                return;
            }
            Some(item) = out.recv() => match item {
                Out::Frame(frame) => records::json_records(&frame),
                Out::Stream { id, bytes, end, aborted } => vec![records::stream_record(id, &bytes, end, aborted)],
                Out::Close(code, text) => {
                    close(&mut sink, code, text).await;
                    return;
                }
            },
            Some((name, payload)) = events.recv() => records::json_records(&event(&mut seq, &name, payload)),
            _ = ticker.tick() => records::json_records(&event(&mut seq, "tick", json!({ "ts": now_ms() }))),
        };
        if send_records(&mut sink, &transport, &mut cipher, plaintexts).await.is_err() {
            return;
        }
    }
}

async fn send_records<S>(sink: &mut Sink<S>, transport: &Transport, cipher: &mut [u8], plaintexts: Vec<Vec<u8>>) -> Result<(), ()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for plain in plaintexts {
        let n = transport.lock().unwrap().write_message(&plain, cipher).map_err(|_| ())?;
        sink.send(Message::Binary(cipher[..n].to_vec().into())).await.map_err(|_| ())?;
    }
    Ok(())
}

/// Host events → phone events, for the bots the phone was told about.
async fn pump(
    inner: Arc<Inner>,
    mut events: broadcast::Receiver<HostEvent>,
    queue: mpsc::Sender<(String, Value)>,
    closing: mpsc::UnboundedSender<CloseReason>,
    mut listed: HashSet<String>,
    initial: Option<Vec<Value>>,
) {
    let push = |name: &str, payload: Value| -> bool {
        if queue.try_send((name.to_string(), payload)).is_err() {
            let _ = closing.send(CloseReason::TooSlow);
            return false;
        }
        true
    };
    if let Some(bots) = initial {
        if !push("bots", json!({ "bots": bots })) {
            return;
        }
    }
    loop {
        let sent = match events.recv().await {
            Ok(HostEvent::Status(raw)) => match raw["gatewayId"].as_str() {
                Some(id) if listed.contains(id) => push("bot.status", json!({ "status": bot_status(&raw) })),
                _ => true,
            },
            Ok(HostEvent::Event { gateway, name, payload }) if listed.contains(&gateway) => {
                push("bot.event", json!({ "gateway": gateway, "event": name, "payload": payload }))
            }
            Ok(HostEvent::Event { .. } | HostEvent::Notice(_)) => true,
            Ok(HostEvent::BotsChanged) => {
                let bots: Vec<Value> = inner.host.statuses().iter().map(bot_status).collect();
                listed = bots.iter().filter_map(|b| b["gatewayId"].as_str().map(str::to_string)).collect();
                push("bots", json!({ "bots": bots }))
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let _ = closing.send(CloseReason::TooSlow);
                false
            }
            Err(broadcast::error::RecvError::Closed) => false,
        };
        if !sent {
            return;
        }
    }
}

struct Session {
    inner: Arc<Inner>,
    phone: String,
    out: mpsc::Sender<Out>,
    streams: AtomicUsize,
    next_stream: AtomicU32,
    /// The registered session (0 until a pairing commits).
    id: AtomicU64,
}

fn dispatch(session: Arc<Session>, frame: &[u8]) {
    let Ok(req) = serde_json::from_slice::<Value>(frame) else { return };
    if req["type"] != "req" {
        return;
    }
    let id = match &req["id"] {
        Value::String(s) => s.clone(),
        Value::Null => return,
        other => other.to_string(),
    };
    let method = req["method"].as_str().unwrap_or_default().to_string();
    let params = req.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({}));
    tokio::spawn(async move {
        match method.as_str() {
            "bot.media" => session.media(&id, &params).await,
            "phone.forget" => {
                session.inner.forget_phone(&session.phone);
                session.reply(&id, Ok(json!({}))).await;
                let _ = session.out.send(Out::Close(4010, "forgotten")).await;
            }
            _ => {
                let result = session.call(&method, params).await;
                session.reply(&id, result).await;
            }
        }
    });
}

fn param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::new("INVALID_PARAMS", format!("{key} is required")))
}

impl Session {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let host = &self.inner.host;
        match method {
            "presence" => {
                self.inner.set_foreground(self.id.load(Ordering::SeqCst), params["foreground"] == true);
                Ok(json!({}))
            }
            "bots.list" => Ok(json!({ "bots": host.statuses().iter().map(bot_status).collect::<Vec<_>>() })),
            "bot.call" => {
                let gateway = param(&params, "gateway")?;
                let method = param(&params, "method")?;
                let args = params.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({}));
                let timeout = params["timeoutMs"].as_u64().map(|ms| Duration::from_millis(ms).min(MAX_CALL_TIMEOUT));
                host.call(gateway, method, args, timeout).await
            }
            "bot.reconnect" => {
                host.reconnect(param(&params, "gateway")?)?;
                Ok(json!({}))
            }
            "push.register" => self.inner.register_push(&self.phone, &params).await,
            "push.unregister" => {
                self.inner.set_push(&self.phone, None);
                Ok(json!({}))
            }
            _ => Err(RpcError::new("UNKNOWN_METHOD", format!("{method} isn't a bridge method"))),
        }
    }

    async fn reply(&self, id: &str, result: Result<Value, RpcError>) {
        let frame = match result {
            Ok(payload) => json!({ "type": "res", "id": id, "ok": true, "payload": payload }),
            Err(e) => json!({ "type": "res", "id": id, "ok": false, "error": e }),
        };
        let mut bytes = serde_json::to_vec(&frame).unwrap_or_default();
        if bytes.len() > MAX_PAYLOAD {
            let e = RpcError::new("TOO_LARGE", "The answer is too big to send to the phone.");
            bytes = serde_json::to_vec(&json!({ "type": "res", "id": id, "ok": false, "error": e })).unwrap_or_default();
        }
        let _ = self.out.send(Out::Frame(bytes)).await;
    }

    /// `bot.media`: answer with a stream id, then send the file as stream records.
    async fn media(&self, id: &str, params: &Value) {
        let (gateway, source) = match (param(params, "gateway"), param(params, "source")) {
            (Ok(g), Ok(s)) => (g, s),
            (Err(e), _) | (_, Err(e)) => return self.reply(id, Err(e)).await,
        };
        if self.streams.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
            self.streams.fetch_sub(1, Ordering::SeqCst);
            return self.reply(id, Ok(json!({ "available": false, "code": "BUSY" }))).await;
        }
        let fetched = self.inner.host.media(gateway, source, params["sessionKey"].as_str(), params["agentId"].as_str()).await;
        match fetched {
            Err(e) => self.reply(id, Err(e)).await,
            Ok(MediaFetch::Unavailable { code, reason }) => {
                self.reply(id, Ok(json!({ "available": false, "code": code, "reason": reason }))).await
            }
            Ok(MediaFetch::Available { size_bytes: Some(size), .. }) if size > MAX_MEDIA_BYTES => {
                self.reply(id, Ok(json!({ "available": false, "code": "TOO_LARGE" }))).await
            }
            Ok(MediaFetch::Available { mime_type, size_bytes, mut body }) => {
                let stream = self.next_stream.fetch_add(1, Ordering::SeqCst) + 1;
                self.reply(id, Ok(json!({ "available": true, "stream": stream, "mimeType": mime_type, "sizeBytes": size_bytes }))).await;
                let mut total: u64 = 0;
                let mut buffered: Vec<u8> = Vec::new();
                let failure = loop {
                    match body.next().await {
                        Some(Ok(bytes)) => {
                            total += bytes.len() as u64;
                            if total > MAX_MEDIA_BYTES {
                                break Some("too large".to_string());
                            }
                            buffered.extend_from_slice(&bytes);
                            while buffered.len() >= MAX_STREAM_CHUNK {
                                let rest = buffered.split_off(MAX_STREAM_CHUNK);
                                let chunk = std::mem::replace(&mut buffered, rest);
                                if !self.send_chunk(stream, chunk, false, false).await {
                                    return self.done_streaming();
                                }
                            }
                        }
                        Some(Err(e)) => break Some(e),
                        None => break None,
                    }
                };
                match failure {
                    None => {
                        self.send_chunk(stream, buffered, true, false).await;
                    }
                    Some(reason) => {
                        let mut reason = reason.into_bytes();
                        reason.truncate(512);
                        self.send_chunk(stream, reason, true, true).await;
                    }
                }
            }
        }
        self.done_streaming();
    }

    async fn send_chunk(&self, id: u32, bytes: Vec<u8>, end: bool, aborted: bool) -> bool {
        self.out.send(Out::Stream { id, bytes, end, aborted }).await.is_ok()
    }

    fn done_streaming(&self) {
        self.streams.fetch_sub(1, Ordering::SeqCst);
    }
}
