//! Sessions end to end: a real listener on 127.0.0.1 with the demo bots,
//! driven by the Rust phone client (phone.rs).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::demo::{Demo, DemoHost, CLAUDE, HERMES, OPENCLAW};
use super::phone::{connect, ConnectError, Invite, PhoneKeys};
use super::{Bridge, BridgeConfig};

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("lg-bridge-{}", uuid::Uuid::new_v4().simple()))
}

fn bridge(host: &'static DemoHost, dir: &std::path::Path) -> Bridge {
    bridge_with_relay(host, dir, None)
}

fn bridge_with_relay(host: &'static DemoHost, dir: &std::path::Path, relay: Option<String>) -> Bridge {
    let mut cfg = BridgeConfig::new(dir.to_path_buf(), "Test Mac".into(), "0.0.0-test".into());
    cfg.relay_url = relay;
    cfg.port = 0;
    cfg.bonjour = false;
    cfg.loopback_first = true;
    cfg.tick_interval = Duration::from_millis(300);
    Bridge::new(cfg, Arc::new(Demo(host)), tokio::runtime::Handle::current()).unwrap()
}

fn client() -> Value {
    json!({ "name": "Test iPhone", "model": "iPhone17,1", "version": "0.1.0", "platform": "ios" })
}

async fn pair(bridge: &Bridge) -> (Invite, String) {
    let start = bridge.pair_start().await.unwrap();
    let invite = Invite::parse(&start.uri).unwrap();
    let addr = format!("127.0.0.1:{}", bridge.port().unwrap());
    (invite, addr)
}

async fn timeout<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f).await.expect("timed out")
}

#[tokio::test]
async fn nothing_listens_until_a_pairing_opens() {
    let dir = temp_dir();
    let bridge = bridge(DemoHost::new().leak(), &dir);
    bridge.start();
    assert_eq!(bridge.port(), None);
    let (invite, _) = pair(&bridge).await;
    assert!(bridge.port().is_some());
    assert_eq!(invite.name, "Test Mac");
    assert_eq!(invite.mac_id(), bridge.mac_id());
    assert!(invite.addrs[0].starts_with("127.0.0.1:"));
    bridge.pair_cancel();
    assert_eq!(bridge.port(), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn pairs_then_reconnects_and_talks_to_bots() {
    let dir = temp_dir();
    let mut host = DemoHost::new();
    host.word_delay = Duration::from_millis(5);
    let host = host.leak();
    let bridge = bridge(host, &dir);
    let (invite, addr) = pair(&bridge).await;
    let keys = PhoneKeys::generate();

    // Pair: hello lists the bots, trimmed.
    let phone = timeout(connect(&addr, &invite, &keys, client(), true)).await.unwrap();
    let bots = phone.hello["bots"].as_array().unwrap();
    let ids: Vec<&str> = bots.iter().map(|b| b["gatewayId"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![OPENCLAW, HERMES, CLAUDE]);
    assert_eq!(bots[0]["kindLabel"], "OpenClaw");
    assert!(bots[0].get("configPath").is_none());
    assert_eq!(bots[0]["hello"]["policy"]["maxPayload"], 26_214_400);
    assert!(bots[0]["hello"].get("snapshot").is_none());
    assert_eq!(phone.hello["protocol"], 1);
    assert_eq!(phone.hello["policy"]["maxPayload"], 33_554_432);
    let state = bridge.state();
    assert_eq!(state.phones.len(), 1);
    assert_eq!(state.phones[0].name, "Test iPhone");
    assert!(state.phones[0].connected);
    assert_eq!(state.pairing.unwrap().paired_with.unwrap().name, "Test iPhone");
    phone.close().await;
    drop(phone);

    // The secret is spent: pairing again with it fails.
    let again = timeout(connect(&addr, &invite, &PhoneKeys::generate(), client(), true)).await;
    assert!(matches!(again, Err(ConnectError::Rejected { ref code, .. }) if code == "PAIRING_EXPIRED"), "{:?}", again.err());

    // A paired phone reconnects without a secret.
    let mut phone = timeout(connect(&addr, &invite, &keys, client(), false)).await.unwrap();
    let agents = phone
        .request("bot.call", json!({ "gateway": OPENCLAW, "method": "agents.list", "params": {} }))
        .await
        .unwrap();
    assert_eq!(agents["agents"][1]["name"], "Fox");
    let err = phone.request("bot.call", json!({ "gateway": "nope", "method": "agents.list" })).await.unwrap_err();
    assert_eq!(err["code"], "UNKNOWN_GATEWAY");
    let err = phone.request("nope", json!({})).await.unwrap_err();
    assert_eq!(err["code"], "UNKNOWN_METHOD");

    // chat.send streams the reply as bot.events.
    let sent = phone
        .request("bot.call", json!({ "gateway": OPENCLAW, "method": "chat.send",
                                     "params": { "sessionKey": "agent:fox:main", "message": "hi", "idempotencyKey": "run-1" } }))
        .await
        .unwrap();
    assert_eq!(sent["runId"], "run-1");
    let mut saw_delta = false;
    loop {
        let frame = timeout(phone.wait_event("bot.event")).await.unwrap();
        assert_eq!(frame["payload"]["gateway"], OPENCLAW);
        let p = &frame["payload"]["payload"];
        if frame["payload"]["event"] == "chat" && p["state"] == "delta" {
            saw_delta = true;
        }
        if frame["payload"]["event"] == "chat" && p["state"] == "final" {
            assert!(p["message"]["content"][0]["text"].as_str().unwrap().contains("hi"));
            break;
        }
    }
    assert!(saw_delta);

    // Ticks keep coming.
    let tick = timeout(phone.wait_event("tick")).await.unwrap();
    assert!(tick["payload"]["ts"].as_u64().unwrap() > 0);
    assert!(tick["seq"].as_u64().unwrap() > 1);

    // Media streams, re-chunked into records.
    let (answer, bytes) = timeout(phone.media(OPENCLAW, "/tmp/big.bin")).await.unwrap();
    assert_eq!(answer["available"], true);
    assert_eq!(bytes.len(), 200_000);
    assert!(bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    let (answer, _) = timeout(phone.media(OPENCLAW, "/tmp/missing.txt")).await.unwrap();
    assert_eq!(answer["available"], false);
    assert_eq!(answer["code"], "not-found");

    // A bot turned off on the Mac drops out of the list.
    host.set_enabled(HERMES, false);
    let bots = timeout(phone.wait_event("bots")).await.unwrap();
    assert_eq!(bots["payload"]["bots"].as_array().unwrap().len(), 2);
    host.emit(HERMES, "chat", json!({ "sessionKey": "agent:default:main" }));
    host.emit(OPENCLAW, "sessions.changed", json!({ "reason": "test" }));
    let next = timeout(phone.wait_event("bot.event")).await.unwrap();
    assert_eq!(next["payload"]["gateway"], OPENCLAW, "events from bots that are off aren't sent");

    // Removed on the Mac: phone.removed, then 4010, then NOT_PAIRED.
    let key = bridge.state().phones[0].key.clone();
    bridge.remove_phone(&key).unwrap();
    timeout(phone.wait_event("phone.removed")).await.unwrap();
    assert!(timeout(phone.next_event()).await.is_none());
    assert_eq!(phone.close_code(), Some(4010));
    assert!(bridge.port().is_none(), "no phones and no pairing: stop listening");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn turns_away_strangers() {
    let dir = temp_dir();
    let bridge = bridge(DemoHost::new().leak(), &dir);
    let (invite, addr) = pair(&bridge).await;

    // Unknown phone, no secret.
    let res = timeout(connect(&addr, &invite, &PhoneKeys::generate(), client(), false)).await;
    assert!(matches!(res, Err(ConnectError::Rejected { ref code, .. }) if code == "NOT_PAIRED"));

    // A secret from somewhere else.
    let mut forged = invite.clone();
    forged.secret = vec![7u8; 32];
    let res = timeout(connect(&addr, &forged, &PhoneKeys::generate(), client(), true)).await;
    assert!(matches!(res, Err(ConnectError::Rejected { ref code, .. }) if code == "BAD_PROOF"));

    // The wrong Mac key: the Mac can't read message 1.
    let mut wrong = invite.clone();
    wrong.noise_key = PhoneKeys::generate().public;
    let res = timeout(connect(&addr, &wrong, &PhoneKeys::generate(), client(), true)).await;
    assert!(matches!(res, Err(ConnectError::Closed(Some(4001)))), "{:?}", res.err());

    // Too many failures from one address.
    for _ in 0..10 {
        let _ = timeout(connect(&addr, &invite, &PhoneKeys::generate(), client(), false)).await;
    }
    let res = timeout(connect(&addr, &invite, &PhoneKeys::generate(), client(), true)).await;
    assert!(matches!(res, Err(ConnectError::Rejected { ref code, .. }) if code == "RATE_LIMITED"), "{:?}", res.err());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn phone_forget_unpairs() {
    let dir = temp_dir();
    let bridge = bridge(DemoHost::new().leak(), &dir);
    let (invite, addr) = pair(&bridge).await;
    let keys = PhoneKeys::generate();
    let mut phone = timeout(connect(&addr, &invite, &keys, client(), true)).await.unwrap();
    // Keep the listener up after forgetting, with a second pairing open.
    bridge.pair_start().await.unwrap();
    assert_eq!(phone.request("phone.forget", json!({})).await.unwrap(), json!({}));
    assert!(timeout(async { while phone.next_event().await.is_some() {} }).await == ());
    assert_eq!(phone.close_code(), Some(4010));
    assert!(bridge.state().phones.is_empty());
    let res = timeout(connect(&addr, &invite, &keys, client(), false)).await;
    assert!(matches!(res, Err(ConnectError::Rejected { ref code, .. }) if code == "NOT_PAIRED"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn unknown_paths_get_a_404() {
    let dir = temp_dir();
    let bridge = bridge(DemoHost::new().leak(), &dir);
    let (_, addr) = pair(&bridge).await;
    let res = tokio_tungstenite::connect_async(format!("ws://{addr}/other")).await;
    match res {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 404),
        other => panic!("expected a 404, got {:?}", other.map(|_| ())),
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The published Noise vector for our suite (cacophony.txt), through snow.
#[test]
fn noise_ik_matches_the_reference_vector() {
    let v: Value = serde_json::from_str(include_str!("../../tests/noise-ik-vector.json")).unwrap();
    let hex = |k: &str| -> Vec<u8> {
        let s = v[k].as_str().unwrap();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    };
    let params = v["protocol_name"].as_str().unwrap().parse().unwrap();
    let (iprol, istatic, ieph, irs) = (hex("init_prologue"), hex("init_static"), hex("init_ephemeral"), hex("init_remote_static"));
    let (rprol, rstatic, reph) = (hex("resp_prologue"), hex("resp_static"), hex("resp_ephemeral"));
    let mut init = snow::Builder::new(params)
        .local_private_key(&istatic)
        .unwrap()
        .remote_public_key(&irs)
        .unwrap()
        .prologue(&iprol)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&ieph)
        .build_initiator()
        .unwrap();
    let params = v["protocol_name"].as_str().unwrap().parse().unwrap();
    let mut resp = snow::Builder::new(params)
        .local_private_key(&rstatic)
        .unwrap()
        .prologue(&rprol)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&reph)
        .build_responder()
        .unwrap();
    let messages = v["messages"].as_array().unwrap();
    let decode = |s: &str| -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() };
    let mut buf = vec![0u8; 65535];
    let mut out = vec![0u8; 65535];
    // Handshake: initiator → responder, responder → initiator.
    for (i, m) in messages.iter().take(2).enumerate() {
        let (payload, ct) = (decode(m["payload"].as_str().unwrap()), decode(m["ciphertext"].as_str().unwrap()));
        let (writer, reader) = if i == 0 { (&mut init, &mut resp) } else { (&mut resp, &mut init) };
        let n = writer.write_message(&payload, &mut buf).unwrap();
        assert_eq!(&buf[..n], &ct[..], "handshake message {i}");
        let n = reader.read_message(&ct, &mut out).unwrap();
        assert_eq!(&out[..n], &payload[..]);
    }
    assert_eq!(init.get_handshake_hash(), &hex("handshake_hash")[..]);
    let mut init = init.into_transport_mode().unwrap();
    let mut resp = resp.into_transport_mode().unwrap();
    for (i, m) in messages.iter().enumerate().skip(2) {
        let (payload, ct) = (decode(m["payload"].as_str().unwrap()), decode(m["ciphertext"].as_str().unwrap()));
        let (writer, reader) = if i % 2 == 0 { (&mut init, &mut resp) } else { (&mut resp, &mut init) };
        let n = writer.write_message(&payload, &mut buf).unwrap();
        assert_eq!(&buf[..n], &ct[..], "transport message {i}");
        let n = reader.read_message(&ct, &mut out).unwrap();
        assert_eq!(&out[..n], &payload[..]);
    }
}

// ---- push ------------------------------------------------------------------------

use std::collections::HashSet;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::host::{Notice, NoticeKind};
use super::push::signing_string;
use super::push::tests::{box_keypair, open};

#[derive(Default)]
struct RelayState {
    registrations: Vec<Value>,
    gone: HashSet<String>,
    sent: Vec<Value>,
    /// Every request carried a good signature by the key it names.
    all_signed: bool,
    requests: usize,
}

/// A stand-in for littleguys-relay: checks signatures like auth.ts, answers like relay.ts.
async fn fake_relay() -> (String, Arc<Mutex<RelayState>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(RelayState { all_signed: true, ..Default::default() }));
    let shared = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let state = shared.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or_default().split(' ');
                let (method, path) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
                let headers: std::collections::HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = buf[head_end..head_end + len].to_vec();

                let h = |k: &str| headers.get(k).cloned().unwrap_or_default();
                let key: [u8; 32] = URL_SAFE_NO_PAD.decode(h("lg-key")).unwrap_or_default().try_into().unwrap_or([0; 32]);
                let sig: [u8; 64] = URL_SAFE_NO_PAD.decode(h("lg-sig")).unwrap_or_default().try_into().unwrap_or([0; 64]);
                let message = signing_string(&method, &path, &h("lg-ts"), &h("lg-nonce"), &body);
                let signed = ed25519_dalek::VerifyingKey::from_bytes(&key)
                    .map(|k| k.verify_strict(message.as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig)).is_ok())
                    .unwrap_or(false);

                let (status, answer) = {
                    let mut st = state.lock().unwrap();
                    st.requests += 1;
                    st.all_signed &= signed;
                    match (method.as_str(), path.as_str()) {
                        _ if !signed => (401, json!({ "error": "bad_signature" })),
                        ("GET", "/v1/registrations") => (200, json!({ "registrations": st.registrations })),
                        ("POST", "/v1/send") => {
                            let push: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                            if st.gone.contains(push["id"].as_str().unwrap_or_default()) {
                                (410, json!({ "error": "gone" }))
                            } else {
                                st.sent.push(push);
                                (200, json!({ "apnsId": "apns-1" }))
                            }
                        }
                        ("DELETE", _) => (200, json!({ "deleted": true })),
                        _ => (404, json!({ "error": "not_found" })),
                    }
                };
                let text = answer.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (url, state)
}

fn reply_notice() -> Notice {
    Notice {
        kind: NoticeKind::Reply,
        title: "🦊 Fox".into(),
        body: "Found three flights under $900.".into(),
        gateway: OPENCLAW.into(),
        session_key: Some("agent:fox:main".into()),
        approval_id: None,
    }
}

#[tokio::test]
async fn registers_with_the_relay_and_pushes_sealed_messages() {
    let dir = temp_dir();
    let (relay_url, relay) = fake_relay().await;
    let bridge = bridge_with_relay(DemoHost::new().leak(), &dir, Some(relay_url.clone()));
    let (invite, addr) = pair(&bridge).await;
    assert_eq!(invite.relay.as_deref(), Some(relay_url.as_str()), "the link carries the relay");
    let keys = PhoneKeys::generate();
    let phone = timeout(connect(&addr, &invite, &keys, client(), true)).await.unwrap();
    assert_eq!(phone.hello["push"], json!({ "relay": relay_url, "registered": false }));

    let (box_secret, box_public) = box_keypair();
    let relay_key = URL_SAFE_NO_PAD.encode([9u8; 32]);
    relay.lock().unwrap().registrations =
        vec![json!({ "id": "reg-1", "owner": relay_key, "environment": "sandbox", "meta": "{\"v\":1,\"name\":\"Test iPhone\"}" })];
    let register = |owner: &str| {
        json!({ "registrationId": "reg-1", "relayKey": owner, "boxKey": URL_SAFE_NO_PAD.encode(&box_public), "environment": "sandbox" })
    };

    // The relay has to confirm the phone owns the registration.
    let err = phone.request("push.register", register("someone-else")).await.unwrap_err();
    assert_eq!(err["code"], "NOT_REGISTERED");
    let err = phone.request("push.register", json!({ "registrationId": "reg-1" })).await.unwrap_err();
    assert_eq!(err["code"], "INVALID_PARAMS");
    assert_eq!(phone.request("push.register", register(&relay_key)).await.unwrap(), json!({}));
    assert!(bridge.state().phones[0].push);

    // In the foreground: no pushes.
    phone.request("presence", json!({ "foreground": true })).await.unwrap();
    assert!(bridge.push(&reply_notice()).await.is_empty());

    // Backgrounded: one sealed push, signed by the Mac, grouped by thread.
    phone.request("presence", json!({ "foreground": false })).await.unwrap();
    let results = bridge.push(&reply_notice()).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "Test iPhone");
    assert_eq!(results[0].1.as_ref().unwrap()["apnsId"], "apns-1");
    let sent = relay.lock().unwrap().sent.last().cloned().unwrap();
    assert_eq!(sent["id"], "reg-1");
    assert_eq!(sent["type"], "alert");
    assert_eq!(sent["ttl"], 3_600);
    assert_eq!(sent["collapseId"].as_str().unwrap().len(), 16);
    let id_key: [u8; 32] = invite.id_key.clone().try_into().unwrap();
    let m = open(&box_secret, "reg-1", sent["ciphertext"].as_str().unwrap(), &id_key);
    assert_eq!(m["kind"], "reply");
    assert_eq!(m["title"], "🦊 Fox");
    assert_eq!(m["gateway"], OPENCLAW);
    assert_eq!(m["sessionKey"], "agent:fox:main");

    // Approval resolved: a background push with no expiry.
    let resolved = Notice {
        kind: NoticeKind::ApprovalResolved,
        title: String::new(),
        body: String::new(),
        gateway: OPENCLAW.into(),
        session_key: None,
        approval_id: Some("appr-1".into()),
    };
    bridge.push(&resolved).await;
    let sent = relay.lock().unwrap().sent.last().cloned().unwrap();
    assert_eq!((sent["type"].as_str(), sent["ttl"].as_u64()), (Some("background"), Some(0)));
    assert_eq!(open(&box_secret, "reg-1", sent["ciphertext"].as_str().unwrap(), &id_key)["approvalId"], "appr-1");

    // The registration survives a reconnect.
    phone.close().await;
    drop(phone);
    let phone = timeout(connect(&addr, &invite, &keys, client(), false)).await.unwrap();
    assert_eq!(phone.hello["push"]["registered"], true);

    // 410 from the relay: the registration is dropped.
    relay.lock().unwrap().gone.insert("reg-1".into());
    phone.request("presence", json!({ "foreground": false })).await.unwrap();
    let results = bridge.push(&reply_notice()).await;
    assert!(results[0].1.as_ref().unwrap_err().starts_with("gone"));
    assert!(!bridge.state().phones[0].push);
    assert!(bridge.push(&reply_notice()).await.is_empty());

    // push.unregister drops it too.
    relay.lock().unwrap().gone.clear();
    phone.request("push.register", register(&relay_key)).await.unwrap();
    assert!(bridge.state().phones[0].push);
    phone.request("push.unregister", json!({})).await.unwrap();
    assert!(!bridge.state().phones[0].push);

    assert!(relay.lock().unwrap().all_signed, "every relay request was signed by the Mac's id key");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn push_needs_a_relay() {
    let dir = temp_dir();
    let bridge = bridge(DemoHost::new().leak(), &dir);
    let (invite, addr) = pair(&bridge).await;
    assert_eq!(invite.relay, None);
    let phone = timeout(connect(&addr, &invite, &PhoneKeys::generate(), client(), true)).await.unwrap();
    assert!(phone.hello.get("push").is_none());
    let err = phone
        .request("push.register", json!({ "registrationId": "r", "relayKey": "k", "boxKey": "b", "environment": "sandbox" }))
        .await
        .unwrap_err();
    assert_eq!(err["code"], "NO_RELAY");
    let _ = std::fs::remove_dir_all(dir);
}
