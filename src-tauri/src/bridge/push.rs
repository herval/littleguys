//! Pushes to paired phones through `littleguys-relay` (its PROTOCOL.md, and
//! docs/BRIDGE-PROTOCOL.md "Push"): requests signed with the Mac's id key,
//! messages signed with it too and sealed (HPKE) to each phone's box key, so
//! the relay forwards what it can't read.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeS, Serializable};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::host::{Notice, NoticeKind};
use super::keys::MacKeys;
use super::pairing::{b64, PushRegistration};
use super::Inner;
use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;

pub const DEFAULT_RELAY: &str = "https://littleguys-relay.vercel.app";
/// The relay refuses longer ciphertexts.
pub const MAX_CIPHERTEXT: usize = 3_000;
const INFO: &[u8] = b"lg-push-v1";

/// What gets signed: `lg-relay-v1`, method, path, time, nonce, hex SHA-256 of the body (auth.ts).
pub fn signing_string(method: &str, path: &str, ts: &str, nonce: &str, body: &[u8]) -> String {
    let digest: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    ["lg-relay-v1", &method.to_ascii_uppercase(), path, ts, nonce, &digest].join("\n")
}

#[derive(Debug)]
pub enum RelayError {
    /// 410: APNs says the phone's token is dead; the relay dropped the registration.
    Gone,
    Status(u16, Value),
    Http(String),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::Gone => write!(f, "the phone's registration is gone"),
            RelayError::Status(status, body) => write!(f, "the relay answered {status}: {body}"),
            RelayError::Http(e) => write!(f, "couldn't reach the relay: {e}"),
        }
    }
}

pub struct RelayClient {
    base: String,
    http: reqwest::Client,
}

impl RelayClient {
    pub fn new(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), http: reqwest::Client::new() }
    }

    async fn request(&self, keys: &MacKeys, method: reqwest::Method, path: &str, payload: Option<&Value>) -> Result<Value, RelayError> {
        let url = reqwest::Url::parse(&format!("{}{path}", self.base)).map_err(|e| RelayError::Http(e.to_string()))?;
        let body = payload.map(|p| serde_json::to_vec(p).unwrap_or_default()).unwrap_or_default();
        let ts = now_ms().to_string();
        let mut nonce = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let nonce = b64(&nonce);
        let signature = keys.sign(signing_string(method.as_str(), url.path(), &ts, &nonce, &body).as_bytes());
        let mut req = self
            .http
            .request(method, url)
            .timeout(Duration::from_secs(20))
            .header("lg-key", keys.id_b64())
            .header("lg-ts", ts)
            .header("lg-nonce", nonce)
            .header("lg-sig", b64(&signature));
        if payload.is_some() {
            req = req.header("content-type", "application/json").body(body);
        }
        let res = req.send().await.map_err(|e| RelayError::Http(e.to_string()))?;
        let status = res.status().as_u16();
        let value: Value = res.json().await.unwrap_or(Value::Null);
        match status {
            200..=299 => Ok(value),
            410 => Err(RelayError::Gone),
            _ => Err(RelayError::Status(status, value)),
        }
    }

    /// `GET /v1/registrations`: the phones that allow this Mac (no tokens).
    pub async fn list_registrations(&self, keys: &MacKeys) -> Result<Vec<Value>, RelayError> {
        let v = self.request(keys, reqwest::Method::GET, "/v1/registrations", None).await?;
        Ok(v["registrations"].as_array().cloned().unwrap_or_default())
    }

    /// `POST /v1/send` → `{apnsId}`.
    pub async fn send(
        &self,
        keys: &MacKeys,
        registration_id: &str,
        kind: &str,
        ciphertext: &str,
        collapse_id: Option<&str>,
        ttl: Option<u64>,
    ) -> Result<Value, RelayError> {
        let mut push = json!({ "id": registration_id, "type": kind, "ciphertext": ciphertext });
        if let Some(c) = collapse_id {
            push["collapseId"] = json!(c);
        }
        if let Some(t) = ttl {
            push["ttl"] = json!(t);
        }
        self.request(keys, reqwest::Method::POST, "/v1/send", Some(&push)).await
    }

    /// `DELETE /v1/registrations/:id` (the sender may forget a phone too).
    pub async fn delete(&self, keys: &MacKeys, registration_id: &str) -> Result<Value, RelayError> {
        self.request(keys, reqwest::Method::DELETE, &format!("/v1/registrations/{registration_id}"), None).await
    }
}

/// The message as the phone's extension reads it.
pub fn message(notice: &Notice, ts: u64) -> Value {
    let mut m = json!({
        "kind": notice.kind.as_str(),
        "title": notice.title,
        "body": notice.body,
        "gateway": notice.gateway,
        "ts": ts,
    });
    if let Some(key) = &notice.session_key {
        m["sessionKey"] = json!(key);
    }
    if let Some(id) = &notice.approval_id {
        m["approvalId"] = json!(id);
    }
    m
}

/// `base64url(enc ‖ sealed)` of `<base64url signature>.<message>`, sealed to the box key.
pub fn seal(keys: &MacKeys, registration_id: &str, box_key: &[u8], message: &[u8]) -> Result<String, String> {
    let mut signed = format!("lg-msg-v1\n{registration_id}\n").into_bytes();
    signed.extend_from_slice(message);
    let mut plaintext = b64(&keys.sign(&signed)).into_bytes();
    plaintext.push(b'.');
    plaintext.extend_from_slice(message);
    let recipient = <X25519HkdfSha256 as Kem>::PublicKey::from_bytes(box_key).map_err(|e| format!("bad box key: {e:?}"))?;
    let (enc, sealed) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
        &OpModeS::Base,
        &recipient,
        INFO,
        &plaintext,
        registration_id.as_bytes(),
        &mut rand::rngs::OsRng,
    )
    .map_err(|e| format!("couldn't seal: {e:?}"))?;
    let mut out = enc.to_bytes().to_vec();
    out.extend_from_slice(&sealed);
    Ok(b64(&out))
}

/// Seal the notice, shortening `body` (ending it with `…`) until it fits the relay's limit.
pub fn seal_fitting(keys: &MacKeys, registration_id: &str, box_key: &[u8], notice: &Notice, ts: u64) -> Result<String, String> {
    let mut notice = notice.clone();
    notice.title = cut(&notice.title, 200);
    loop {
        let ciphertext = seal(keys, registration_id, box_key, &serde_json::to_vec(&message(&notice, ts)).unwrap_or_default())?;
        if ciphertext.len() <= MAX_CIPHERTEXT {
            return Ok(ciphertext);
        }
        if notice.body.is_empty() {
            return Err("the notification doesn't fit even without its text".into());
        }
        // Base64 grows bytes by 4/3; cut a little more than the overshoot.
        let over = (ciphertext.len() - MAX_CIPHERTEXT) * 3 / 4 + 8;
        let keep = notice.body.trim_end_matches('…').len().saturating_sub(over);
        notice.body = cut(&notice.body, keep);
        if keep == 0 {
            notice.body.clear();
        }
    }
}

/// At most `max_bytes` of `s` (on a character boundary), with `…` when shortened.
fn cut(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s[..end].trim_end())
}

/// First 16 hex characters of SHA-256: replies and routines group by thread, approvals by id.
pub fn collapse_id(notice: &Notice) -> Option<String> {
    let source = match notice.kind {
        NoticeKind::Reply | NoticeKind::Routine => format!("{}|{}", notice.gateway, notice.session_key.as_deref()?),
        NoticeKind::Approval => notice.approval_id.clone()?,
        NoticeKind::ApprovalResolved => return None,
    };
    Some(Sha256::digest(source.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// `(type, ttl seconds)`.
pub fn delivery(kind: NoticeKind) -> (&'static str, u64) {
    match kind {
        NoticeKind::Reply | NoticeKind::Routine => ("alert", 3_600),
        NoticeKind::Approval => ("alert", 600),
        NoticeKind::ApprovalResolved => ("background", 0),
    }
}

/// One phone's answer to a push: the relay's `{apnsId}`, or why it didn't go.
pub type Delivery = (String, Result<Value, String>);

impl Inner {
    /// `push.register`: trust the registration once the relay confirms the phone owns it.
    pub(crate) async fn register_push(&self, phone: &str, params: &Value) -> Result<Value, RpcError> {
        let relay = self.relay_url().ok_or_else(|| RpcError::new("NO_RELAY", "Push isn't set up on this Mac."))?;
        let field = |k: &str| {
            params[k].as_str().filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| RpcError::new("INVALID_PARAMS", format!("{k} is required")))
        };
        let registration = PushRegistration {
            registration_id: field("registrationId")?,
            relay_key: field("relayKey")?,
            box_key: field("boxKey")?,
            environment: params["environment"].as_str().unwrap_or("production").to_string(),
        };
        let box_key = URL_SAFE_NO_PAD.decode(&registration.box_key).unwrap_or_default();
        if <X25519HkdfSha256 as Kem>::PublicKey::from_bytes(&box_key).is_err() {
            return Err(RpcError::new("INVALID_PARAMS", "boxKey isn't an X25519 key"));
        }
        let listed = RelayClient::new(&relay)
            .list_registrations(&self.keys)
            .await
            .map_err(|e| RpcError::new("RELAY_UNAVAILABLE", e.to_string()))?;
        let owned = listed
            .iter()
            .any(|r| r["id"] == registration.registration_id.as_str() && r["owner"] == registration.relay_key.as_str());
        if !owned {
            return Err(RpcError::new("NOT_REGISTERED", "The relay has no such registration for this phone."));
        }
        self.set_push(phone, Some(registration));
        Ok(json!({}))
    }

    /// Send a notice to every registered phone that isn't looking at the app.
    pub(crate) async fn push_notice(self: &Arc<Self>, notice: &Notice) -> Vec<Delivery> {
        let Some(relay) = self.relay_url() else { return Vec::new() };
        let relay = RelayClient::new(&relay);
        let (kind, ttl) = delivery(notice.kind);
        let collapse = collapse_id(notice);
        let mut out = Vec::new();
        for (name, registration) in self.push_targets() {
            let box_key = URL_SAFE_NO_PAD.decode(&registration.box_key).unwrap_or_default();
            let sealed = seal_fitting(&self.keys, &registration.registration_id, &box_key, notice, now_ms());
            let result = match sealed {
                Err(e) => Err(e),
                Ok(ciphertext) => {
                    match relay.send(&self.keys, &registration.registration_id, kind, &ciphertext, collapse.as_deref(), Some(ttl)).await {
                        Ok(v) => Ok(v),
                        Err(RelayError::Gone) => {
                            self.drop_registration(&registration.registration_id);
                            Err("gone: the phone has to register again".into())
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
            };
            if let Err(e) = &result {
                log::warn!("bridge: push to {name} failed: {e}");
            }
            out.push((name, result));
        }
        out
    }

    /// Best effort, when a phone is forgotten.
    pub(crate) fn forget_registration(self: &Arc<Self>, registration_id: String) {
        let Some(relay) = self.relay_url() else { return };
        let this = self.clone();
        self.rt().spawn(async move {
            if let Err(e) = RelayClient::new(&relay).delete(&this.keys, &registration_id).await {
                log::debug!("bridge: couldn't delete the push registration: {e}");
            }
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hpke::OpModeR;

    fn keys() -> MacKeys {
        MacKeys::with_id_seed([7u8; 32])
    }

    fn notice(body: &str) -> Notice {
        Notice {
            kind: NoticeKind::Reply,
            title: "🦊 Fox".into(),
            body: body.into(),
            gateway: "openclaw".into(),
            session_key: Some("agent:fox:main".into()),
            approval_id: None,
        }
    }

    /// Made with the relay's own code (src/auth.ts `signingString`, node:crypto Ed25519, seed [7; 32]).
    #[test]
    fn signs_requests_like_the_relay() {
        let body = br#"{"id":"r1","type":"alert","ciphertext":"abc"}"#;
        let s = signing_string("post", "/v1/send", "1760000000000", "bm9uY2Utbm9uY2Utbm9uY2U", body);
        assert_eq!(
            s,
            "lg-relay-v1\nPOST\n/v1/send\n1760000000000\nbm9uY2Utbm9uY2Utbm9uY2U\n4ab882d3e7737f154c1972ea8b07d2ee4435d12083bf352ff1c6f38510bce734"
        );
        let keys = keys();
        assert_eq!(keys.id_b64(), "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw");
        assert_eq!(
            b64(&keys.sign(s.as_bytes())),
            "9Ea5tqS3aC8PuuzrUlZlnMKtojzK4BzGiEIJT-DFFveyPdvNfIOkTysRLtkExxfDS2bqgmaiNPC3H8zgGqv_DQ"
        );
    }

    /// What the phone's extension does: open, split at the first '.', check the signature.
    pub(crate) fn open(sk: &<X25519HkdfSha256 as Kem>::PrivateKey, registration_id: &str, ciphertext: &str, sender: &[u8; 32]) -> Value {
        let raw = URL_SAFE_NO_PAD.decode(ciphertext).unwrap();
        let (enc, sealed) = raw.split_at(32);
        let enc = <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(enc).unwrap();
        let plain = hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
            &OpModeR::Base,
            sk,
            &enc,
            INFO,
            sealed,
            registration_id.as_bytes(),
        )
        .expect("opens");
        let dot = plain.iter().position(|b| *b == b'.').unwrap();
        let (sig, message) = (&plain[..dot], &plain[dot + 1..]);
        let sig: [u8; 64] = URL_SAFE_NO_PAD.decode(sig).unwrap().try_into().unwrap();
        let mut signed = format!("lg-msg-v1\n{registration_id}\n").into_bytes();
        signed.extend_from_slice(message);
        ed25519_dalek::VerifyingKey::from_bytes(sender)
            .unwrap()
            .verify_strict(&signed, &ed25519_dalek::Signature::from_bytes(&sig))
            .expect("signed by the Mac");
        serde_json::from_slice(message).unwrap()
    }

    pub(crate) fn box_keypair() -> (<X25519HkdfSha256 as Kem>::PrivateKey, Vec<u8>) {
        let (sk, pk) = X25519HkdfSha256::gen_keypair(&mut rand::rngs::OsRng);
        (sk, pk.to_bytes().to_vec())
    }

    #[test]
    fn seals_and_opens() {
        let keys = keys();
        let (sk, pk) = box_keypair();
        let ciphertext = seal_fitting(&keys, "reg-1", &pk, &notice("All done."), 42).unwrap();
        let m = open(&sk, "reg-1", &ciphertext, &keys.id_public);
        assert_eq!(m, json!({ "kind": "reply", "title": "🦊 Fox", "body": "All done.", "gateway": "openclaw", "sessionKey": "agent:fox:main", "ts": 42 }));
        // The registration id is the AAD: another id doesn't open it.
        let raw = URL_SAFE_NO_PAD.decode(&ciphertext).unwrap();
        let enc = <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(&raw[..32]).unwrap();
        assert!(hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(&OpModeR::Base, &sk, &enc, INFO, &raw[32..], b"reg-2").is_err());
    }

    #[test]
    fn long_bodies_are_shortened_to_fit() {
        let keys = keys();
        let (sk, pk) = box_keypair();
        let long = "Here's everything I found. ".repeat(200) + "😀";
        let ciphertext = seal_fitting(&keys, "reg-1", &pk, &notice(&long), 1).unwrap();
        assert!(ciphertext.len() <= MAX_CIPHERTEXT);
        let body = open(&sk, "reg-1", &ciphertext, &keys.id_public)["body"].as_str().unwrap().to_string();
        assert!(body.ends_with('…'));
        assert!(body.len() > 1_000, "kept as much as fits ({} bytes)", body.len());
        assert!(long.starts_with(body.trim_end_matches('…')));
    }

    #[test]
    fn grouping_and_expiry() {
        let reply = notice("x");
        let id = collapse_id(&reply).unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(id, Sha256::digest(b"openclaw|agent:fox:main").iter().take(8).map(|b| format!("{b:02x}")).collect::<String>());
        let approval = Notice { kind: NoticeKind::Approval, approval_id: Some("appr-1".into()), ..reply.clone() };
        assert_eq!(collapse_id(&approval).unwrap(), Sha256::digest(b"appr-1").iter().take(8).map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(delivery(NoticeKind::Reply), ("alert", 3_600));
        assert_eq!(delivery(NoticeKind::Approval), ("alert", 600));
        assert_eq!(delivery(NoticeKind::ApprovalResolved), ("background", 0));
        assert_eq!(collapse_id(&Notice { kind: NoticeKind::ApprovalResolved, ..approval }), None);
    }
}
