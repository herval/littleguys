//! Pairing (docs/BRIDGE-PROTOCOL.md "Pairing link", "Committing a pairing"):
//! one-time secrets that live in memory, the proof a phone makes from one, and
//! the phones this Mac trusts (`phones.json`, 0600).

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::gateway::identity::write_private;

/// How long a pairing secret works.
pub const PAIRING_TTL_MS: u64 = 10 * 60 * 1000;
/// Spent and expired secrets are remembered this long, to tell
/// `PAIRING_EXPIRED` from `BAD_PROOF`.
const REMEMBER_MS: u64 = 60 * 60 * 1000;

/// `HMAC-SHA256(secret, "lg-bridge-pair-v1\n" + phone key + "\n" + Mac Noise key)`, keys in base64url.
pub fn proof(secret: &[u8], phone_key_b64: &str, mac_noise_b64: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(format!("lg-bridge-pair-v1\n{phone_key_b64}\n{mac_noise_b64}").as_bytes());
    mac.finalize().into_bytes().into()
}

struct Secret {
    id: u64,
    bytes: [u8; 32],
    created_ms: u64,
    used: bool,
    /// Replaced by a newer code or cancelled: it no longer pairs.
    closed: bool,
}

#[derive(Debug, PartialEq)]
pub enum ProofCheck {
    /// Matches the open secret with this id.
    Match(u64),
    Expired,
    Bad,
}

/// Open (and recently closed) pairing secrets. In memory only.
#[derive(Default)]
pub struct Pairings {
    secrets: Vec<Secret>,
    next_id: u64,
}

impl Pairings {
    /// A fresh secret; any open one stops working.
    pub fn open(&mut self, now_ms: u64) -> (u64, [u8; 32]) {
        self.prune(now_ms);
        for s in &mut self.secrets {
            s.closed = true;
        }
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        self.next_id += 1;
        self.secrets.push(Secret { id: self.next_id, bytes, created_ms: now_ms, used: false, closed: false });
        (self.next_id, bytes)
    }

    pub fn close_all(&mut self) {
        for s in &mut self.secrets {
            s.closed = true;
        }
    }

    /// The secret that's open right now, if any (with its expiry).
    pub fn current(&self, now_ms: u64) -> Option<(u64, u64)> {
        self.secrets
            .iter()
            .rev()
            .find(|s| !s.closed && !s.used && now_ms < s.created_ms + PAIRING_TTL_MS)
            .map(|s| (s.id, s.created_ms + PAIRING_TTL_MS))
    }

    pub fn check(&self, proof_bytes: &[u8], phone_key_b64: &str, mac_noise_b64: &str, now_ms: u64) -> ProofCheck {
        use subtle_eq::ct_eq;
        let found = self.secrets.iter().find(|s| ct_eq(&proof(&s.bytes, phone_key_b64, mac_noise_b64), proof_bytes));
        match found {
            Some(s) if !s.used && !s.closed && now_ms < s.created_ms + PAIRING_TTL_MS => ProofCheck::Match(s.id),
            Some(_) => ProofCheck::Expired,
            None => ProofCheck::Bad,
        }
    }

    /// Spend a secret; false when someone else already did.
    pub fn commit(&mut self, id: u64) -> bool {
        match self.secrets.iter_mut().find(|s| s.id == id) {
            Some(s) if !s.used => {
                s.used = true;
                true
            }
            _ => false,
        }
    }

    fn prune(&mut self, now_ms: u64) {
        self.secrets.retain(|s| now_ms < s.created_ms + PAIRING_TTL_MS + REMEMBER_MS);
    }
}

/// Constant-time comparison, so a proof can't be guessed byte by byte.
mod subtle_eq {
    pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Phone {
    /// The phone's bridge key (X25519), base64url.
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub model: String,
    pub paired_at_ms: u64,
    #[serde(default)]
    pub last_seen_at_ms: u64,
    /// Where pushes for this phone go (`push.register`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushRegistration>,
}

/// A phone's registration with the push relay, checked against the relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushRegistration {
    pub registration_id: String,
    /// The phone's relay key (Ed25519, base64url): the registration's owner.
    pub relay_key: String,
    /// The phone's box key (X25519, base64url): pushes are sealed to it.
    pub box_key: String,
    pub environment: String,
}

/// The phones this Mac trusts.
pub struct PhoneStore {
    path: PathBuf,
    phones: Vec<Phone>,
}

impl PhoneStore {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("phones.json");
        let phones = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path, phones }
    }

    pub fn all(&self) -> &[Phone] {
        &self.phones
    }

    pub fn get(&self, key: &str) -> Option<&Phone> {
        self.phones.iter().find(|p| p.key == key)
    }

    /// Add a phone, or refresh its name and model if it paired before (its push registration stays).
    pub fn upsert(&mut self, mut phone: Phone) -> anyhow::Result<()> {
        match self.phones.iter_mut().find(|p| p.key == phone.key) {
            Some(existing) => {
                if phone.push.is_none() {
                    phone.push = existing.push.take();
                }
                *existing = phone;
            }
            None => self.phones.push(phone),
        }
        self.save()
    }

    /// Set or drop a phone's push registration; false when there's no such phone.
    pub fn set_push(&mut self, key: &str, push: Option<PushRegistration>) -> anyhow::Result<bool> {
        let Some(phone) = self.phones.iter_mut().find(|p| p.key == key) else { return Ok(false) };
        phone.push = push;
        self.save()?;
        Ok(true)
    }

    /// The relay said this registration is gone (410).
    pub fn drop_registration(&mut self, registration_id: &str) -> anyhow::Result<()> {
        let mut changed = false;
        for phone in self.phones.iter_mut().filter(|p| p.push.as_ref().is_some_and(|r| r.registration_id == registration_id)) {
            phone.push = None;
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn remove(&mut self, key: &str) -> anyhow::Result<bool> {
        let before = self.phones.len();
        self.phones.retain(|p| p.key != key);
        let removed = self.phones.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn rename(&mut self, key: &str, name: &str) -> anyhow::Result<bool> {
        let Some(phone) = self.phones.iter_mut().find(|p| p.key == key) else { return Ok(false) };
        phone.name = name.trim().to_string();
        self.save()?;
        Ok(true)
    }

    /// Remembered in memory; written when a session ends.
    pub fn touch(&mut self, key: &str, now_ms: u64) {
        if let Some(phone) = self.phones.iter_mut().find(|p| p.key == key) {
            phone.last_seen_at_ms = now_ms;
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(&self.phones)?)
    }
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: &str = "phone-key";
    const MAC: &str = "mac-key";

    #[test]
    fn a_secret_pairs_once() {
        let mut p = Pairings::default();
        let (id, secret) = p.open(1_000);
        let good = proof(&secret, PHONE, MAC);
        assert_eq!(p.check(&good, PHONE, MAC, 2_000), ProofCheck::Match(id));
        assert!(p.commit(id));
        assert!(!p.commit(id));
        assert_eq!(p.check(&good, PHONE, MAC, 3_000), ProofCheck::Expired);
    }

    #[test]
    fn proofs_bind_both_keys() {
        let mut p = Pairings::default();
        let (_, secret) = p.open(0);
        assert_eq!(p.check(&proof(&secret, "other-phone", MAC), PHONE, MAC, 1), ProofCheck::Bad);
        assert_eq!(p.check(&proof(&secret, PHONE, "other-mac"), PHONE, MAC, 1), ProofCheck::Bad);
        assert_eq!(p.check(&[0u8; 32], PHONE, MAC, 1), ProofCheck::Bad);
    }

    #[test]
    fn secrets_expire_after_ten_minutes() {
        let mut p = Pairings::default();
        let (_, secret) = p.open(0);
        let good = proof(&secret, PHONE, MAC);
        assert_eq!(p.check(&good, PHONE, MAC, PAIRING_TTL_MS + 1), ProofCheck::Expired);
        assert_eq!(p.current(PAIRING_TTL_MS + 1), None);
    }

    #[test]
    fn a_new_code_closes_the_old_one() {
        let mut p = Pairings::default();
        let (_, old) = p.open(0);
        let (id, new) = p.open(10);
        assert_eq!(p.check(&proof(&old, PHONE, MAC), PHONE, MAC, 20), ProofCheck::Expired);
        assert_eq!(p.check(&proof(&new, PHONE, MAC), PHONE, MAC, 20), ProofCheck::Match(id));
        assert_eq!(p.current(20).map(|c| c.0), Some(id));
    }

    #[test]
    fn phones_persist() {
        let dir = std::env::temp_dir().join(format!("lg-phones-{}", uuid::Uuid::new_v4().simple()));
        let mut store = PhoneStore::load(&dir);
        let phone = Phone { key: "k".into(), name: "Phone".into(), model: "iPhone".into(), paired_at_ms: 1, last_seen_at_ms: 0, push: None };
        store.upsert(phone.clone()).unwrap();
        assert!(store.rename("k", " Herval's iPhone ").unwrap());
        assert_eq!(PhoneStore::load(&dir).get("k").unwrap().name, "Herval's iPhone");
        assert!(store.remove("k").unwrap());
        assert!(PhoneStore::load(&dir).all().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
