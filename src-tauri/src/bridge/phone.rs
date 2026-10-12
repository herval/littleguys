//! A phone, in Rust: the reference client for docs/BRIDGE-PROTOCOL.md. The
//! tests and `examples/bridge_probe.rs` use it; littleguys for iPhone does
//! the same in Swift.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use snow::TransportState;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::keys::{mac_id, prologue, NOISE_PARAMS};
use super::pairing::proof;
use super::records::{self, Joiner, Record, MAX_NOISE_MESSAGE};

/// A parsed `littleguys://pair?…` link.
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub name: String,
    pub id_key: Vec<u8>,
    pub noise_key: Vec<u8>,
    pub addrs: Vec<String>,
    pub relay: Option<String>,
    pub secret: Vec<u8>,
}

impl Invite {
    pub fn parse(link: &str) -> Result<Self, String> {
        let query = link.strip_prefix("littleguys://pair?").ok_or("not a littleguys pairing link")?;
        let mut params: HashMap<String, String> = HashMap::new();
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            params.insert(k.to_string(), percent_decode(v));
        }
        if params.get("v").map(String::as_str) != Some("1") {
            return Err("unsupported pairing link version".into());
        }
        let key = |name: &str| -> Result<Vec<u8>, String> {
            let raw = params.get(name).ok_or(format!("{name} is missing"))?;
            let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|e| format!("{name}: {e}"))?;
            (bytes.len() == 32).then_some(bytes).ok_or(format!("{name} has the wrong length"))
        };
        Ok(Self {
            name: params.get("name").cloned().unwrap_or_default(),
            id_key: key("id")?,
            noise_key: key("nk")?,
            addrs: params.get("addr").map(|a| a.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default(),
            relay: params.get("relay").cloned(),
            secret: key("secret")?,
        })
    }

    pub fn mac_id(&self) -> String {
        mac_id(&self.id_key)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The phone's bridge key (X25519).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneKeys {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl PhoneKeys {
    pub fn generate() -> Self {
        let pair = snow::Builder::new(NOISE_PARAMS.parse().unwrap()).generate_keypair().expect("X25519 keypair");
        Self { private: pair.private, public: pair.public }
    }

    pub fn public_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.public)
    }
}

#[derive(Debug)]
pub enum ConnectError {
    Io(String),
    /// The Mac closed the socket (4001: it couldn't read the handshake).
    Closed(Option<u16>),
    /// Handshake message 2 carried `{error}`.
    Rejected { code: String, message: String },
    Noise(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Io(e) => write!(f, "couldn't connect: {e}"),
            ConnectError::Closed(code) => write!(f, "the Mac closed the connection ({code:?})"),
            ConnectError::Rejected { code, message } => write!(f, "{code}: {message}"),
            ConnectError::Noise(e) => write!(f, "handshake failed: {e}"),
        }
    }
}

/// One piece of a media stream.
#[derive(Debug)]
pub struct Piece {
    pub bytes: Vec<u8>,
    pub end: bool,
    pub aborted: bool,
}

struct Slot {
    tx: mpsc::UnboundedSender<Piece>,
    rx: Option<mpsc::UnboundedReceiver<Piece>>,
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Shared {
    sink: tokio::sync::Mutex<SplitSink<Ws, Message>>,
    transport: Mutex<TransportState>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    streams: Mutex<HashMap<u32, Slot>>,
    close_code: Mutex<Option<u16>>,
}

impl Shared {
    fn slot(&self, id: u32) -> (mpsc::UnboundedSender<Piece>, Option<mpsc::UnboundedReceiver<Piece>>) {
        let mut streams = self.streams.lock().unwrap();
        let slot = streams.entry(id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            Slot { tx, rx: Some(rx) }
        });
        (slot.tx.clone(), None)
    }

    fn take_rx(&self, id: u32) -> Option<mpsc::UnboundedReceiver<Piece>> {
        self.slot(id);
        self.streams.lock().unwrap().get_mut(&id).and_then(|s| s.rx.take())
    }
}

pub struct PhoneSession {
    shared: Arc<Shared>,
    events: mpsc::UnboundedReceiver<Value>,
    next_id: AtomicU64,
    pub hello: Value,
}

/// Connect to `host:port` (and pair, with the invite's secret), then send the
/// first `presence`, which commits a pairing.
pub async fn connect(
    addr: &str,
    invite: &Invite,
    keys: &PhoneKeys,
    client: Value,
    pair: bool,
) -> Result<PhoneSession, ConnectError> {
    let (ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(format!("ws://{addr}/v1/ws")))
        .await
        .map_err(|_| ConnectError::Io("timed out".into()))?
        .map_err(|e| ConnectError::Io(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();

    let prologue = prologue(&invite.mac_id());
    let mut hs = snow::Builder::new(NOISE_PARAMS.parse().unwrap())
        .local_private_key(&keys.private)
        .and_then(|b| b.remote_public_key(&invite.noise_key))
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.build_initiator())
        .map_err(|e| ConnectError::Noise(e.to_string()))?;
    let mut msg1 = json!({ "v": 1, "client": client });
    if pair {
        let p = proof(&invite.secret, &keys.public_b64(), &URL_SAFE_NO_PAD.encode(&invite.noise_key));
        msg1["pair"] = json!({ "proof": URL_SAFE_NO_PAD.encode(p) });
    }
    let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
    let n = hs.write_message(&serde_json::to_vec(&msg1).unwrap(), &mut buf).map_err(|e| ConnectError::Noise(e.to_string()))?;
    sink.send(Message::Binary(buf[..n].to_vec().into())).await.map_err(|e| ConnectError::Io(e.to_string()))?;

    let reply = loop {
        match stream.next().await {
            Some(Ok(Message::Binary(b))) => break b,
            Some(Ok(Message::Close(frame))) => return Err(ConnectError::Closed(frame.map(|f| u16::from(f.code)))),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(ConnectError::Io(e.to_string())),
            None => return Err(ConnectError::Closed(None)),
        }
    };
    let n = hs.read_message(&reply, &mut buf).map_err(|e| ConnectError::Noise(e.to_string()))?;
    let payload: Value = serde_json::from_slice(&buf[..n]).unwrap_or(Value::Null);
    if let Some(error) = payload.get("error") {
        return Err(ConnectError::Rejected {
            code: error["code"].as_str().unwrap_or_default().to_string(),
            message: error["message"].as_str().unwrap_or_default().to_string(),
        });
    }
    let transport = hs.into_transport_mode().map_err(|e| ConnectError::Noise(e.to_string()))?;

    let shared = Arc::new(Shared {
        sink: tokio::sync::Mutex::new(sink),
        transport: Mutex::new(transport),
        pending: Mutex::new(HashMap::new()),
        streams: Mutex::new(HashMap::new()),
        close_code: Mutex::new(None),
    });
    let (events_tx, events) = mpsc::unbounded_channel();
    let reader = shared.clone();
    tokio::spawn(async move {
        let mut joiner = Joiner::new(64 * 1024 * 1024);
        let mut plain = vec![0u8; MAX_NOISE_MESSAGE];
        while let Some(msg) = stream.next().await {
            let bytes = match msg {
                Ok(Message::Binary(b)) => b,
                Ok(Message::Close(frame)) => {
                    *reader.close_code.lock().unwrap() = frame.map(|f| u16::from(f.code));
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            };
            let Ok(n) = reader.transport.lock().unwrap().read_message(&bytes, &mut plain) else { break };
            match records::parse(&plain[..n]) {
                Ok(Record::Json { chunk, more }) => {
                    let Ok(Some(frame)) = joiner.push(chunk, more) else { continue };
                    let Ok(frame) = serde_json::from_slice::<Value>(&frame) else { continue };
                    match frame["type"].as_str() {
                        Some("res") => {
                            let id = frame["id"].as_str().unwrap_or_default().to_string();
                            if let Some(tx) = reader.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(frame);
                            }
                        }
                        Some("event") => {
                            let _ = events_tx.send(frame);
                        }
                        _ => {}
                    }
                }
                Ok(Record::Stream { id, bytes, end, aborted }) => {
                    let (tx, _) = reader.slot(id);
                    let _ = tx.send(Piece { bytes: bytes.to_vec(), end, aborted });
                }
                Err(_) => break,
            }
        }
        reader.pending.lock().unwrap().clear();
        reader.streams.lock().unwrap().clear();
    });

    let session = PhoneSession { shared, events, next_id: AtomicU64::new(1), hello: payload };
    session.request("presence", json!({ "foreground": true })).await.map_err(|e| ConnectError::Io(e.to_string()))?;
    Ok(session)
}

impl PhoneSession {
    /// `Ok(payload)`, or `Err({code, message})`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id.clone(), tx);
        let frame = serde_json::to_vec(&json!({ "type": "req", "id": id, "method": method, "params": params })).unwrap();
        self.send_records(records::json_records(&frame)).await.map_err(|e| json!({ "code": "DISCONNECTED", "message": e }))?;
        let res = rx.await.map_err(|_| json!({ "code": "DISCONNECTED", "message": "the Mac went away" }))?;
        if res["ok"] == true {
            Ok(res["payload"].clone())
        } else {
            Err(res["error"].clone())
        }
    }

    async fn send_records(&self, plaintexts: Vec<Vec<u8>>) -> Result<(), String> {
        let mut sink = self.shared.sink.lock().await;
        let mut cipher = vec![0u8; MAX_NOISE_MESSAGE];
        for plain in plaintexts {
            let n = self.shared.transport.lock().unwrap().write_message(&plain, &mut cipher).map_err(|e| e.to_string())?;
            sink.send(Message::Binary(cipher[..n].to_vec().into())).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The next event frame (`{event, payload, seq}`), or None once the Mac is gone.
    pub async fn next_event(&mut self) -> Option<Value> {
        self.events.recv().await
    }

    /// The next event with this name, skipping others (ticks, …).
    pub async fn wait_event(&mut self, name: &str) -> Option<Value> {
        while let Some(frame) = self.next_event().await {
            if frame["event"] == name {
                return Some(frame);
            }
        }
        None
    }

    /// `bot.media`, then the whole stream: `(answer, bytes)`.
    pub async fn media(&self, gateway: &str, source: &str) -> Result<(Value, Vec<u8>), Value> {
        let answer = self.request("bot.media", json!({ "gateway": gateway, "source": source })).await?;
        let Some(stream) = answer["stream"].as_u64() else { return Ok((answer, Vec::new())) };
        let mut rx = self.shared.take_rx(stream as u32).ok_or_else(|| json!({ "code": "STREAM", "message": "stream already taken" }))?;
        let mut bytes = Vec::new();
        while let Some(piece) = rx.recv().await {
            if piece.aborted {
                return Err(json!({ "code": "ABORTED", "message": String::from_utf8_lossy(&piece.bytes) }));
            }
            bytes.extend_from_slice(&piece.bytes);
            if piece.end {
                return Ok((answer, bytes));
            }
        }
        Err(json!({ "code": "DISCONNECTED", "message": "the Mac went away mid-stream" }))
    }

    /// The close code the Mac sent, once it closed.
    pub fn close_code(&self) -> Option<u16> {
        *self.shared.close_code.lock().unwrap()
    }

    pub async fn close(&self) {
        let _ = self.shared.sink.lock().await.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_invites() {
        let key = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let link = format!("littleguys://pair?v=1&name=Herval%27s%20Mac&id={key}&nk={key}&addr=192.168.1.20:47100,mac.tail.ts.net:47100&secret={key}");
        let invite = Invite::parse(&link).unwrap();
        assert_eq!(invite.name, "Herval's Mac");
        assert_eq!(invite.addrs, vec!["192.168.1.20:47100", "mac.tail.ts.net:47100"]);
        assert_eq!(invite.relay, None);
        assert!(Invite::parse("littleguys://pair?v=2").is_err());
    }
}
