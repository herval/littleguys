//! The Mac's two bridge keys (docs/BRIDGE-PROTOCOL.md "Keys"): the id key
//! (Ed25519; its identity, and the push sender key) and the Noise key
//! (X25519; the responder static of every session). Stored as a 0600 file in
//! the data dir, like the gateway device identity.

use std::path::Path;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gateway::identity::{now_ms, write_private};

pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

pub struct MacKeys {
    id: SigningKey,
    noise_private: [u8; 32],
    /// Raw Ed25519 public key.
    pub id_public: [u8; 32],
    /// Raw X25519 public key.
    pub noise_public: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredKeys {
    version: u32,
    /// Base64 (standard) of the 32-byte Ed25519 seed.
    id_seed: String,
    noise_private: String,
    noise_public: String,
    created_at_ms: u64,
}

impl MacKeys {
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("bridge-keys.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let stored: StoredKeys = serde_json::from_str(&raw)?;
            return Ok(Self::from_parts(
                decode32(&stored.id_seed)?,
                decode32(&stored.noise_private)?,
                decode32(&stored.noise_public)?,
            ));
        }
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        let noise = snow::Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
        let stored = StoredKeys {
            version: 1,
            id_seed: STANDARD.encode(seed),
            noise_private: STANDARD.encode(&noise.private),
            noise_public: STANDARD.encode(&noise.public),
            created_at_ms: now_ms(),
        };
        write_private(&path, &serde_json::to_vec_pretty(&stored)?)?;
        Ok(Self::from_parts(seed, decode32(&stored.noise_private)?, decode32(&stored.noise_public)?))
    }

    fn from_parts(seed: [u8; 32], noise_private: [u8; 32], noise_public: [u8; 32]) -> Self {
        let id = SigningKey::from_bytes(&seed);
        Self { id_public: id.verifying_key().to_bytes(), id, noise_private, noise_public }
    }

    pub fn noise_private(&self) -> &[u8; 32] {
        &self.noise_private
    }

    /// An Ed25519 signature by the id key (relay requests, push messages).
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.id.sign(message).to_bytes()
    }

    /// A fixed id key and a fresh Noise key (fixtures).
    #[cfg(test)]
    pub(crate) fn with_id_seed(seed: [u8; 32]) -> Self {
        let noise = snow::Builder::new(NOISE_PARAMS.parse().unwrap()).generate_keypair().unwrap();
        Self::from_parts(seed, noise.private.try_into().unwrap(), noise.public.try_into().unwrap())
    }

    /// `hex(SHA-256(id key)[0..16])`.
    pub fn mac_id(&self) -> String {
        mac_id(&self.id_public)
    }

    pub fn id_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.id_public)
    }

    pub fn noise_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.noise_public)
    }
}

pub fn mac_id(id_public: &[u8]) -> String {
    Sha256::digest(id_public)[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The Noise prologue: `"lg-bridge-v1\n" + macId`.
pub fn prologue(mac_id: &str) -> Vec<u8> {
    format!("lg-bridge-v1\n{mac_id}").into_bytes()
}

fn decode32(b64: &str) -> anyhow::Result<[u8; 32]> {
    STANDARD
        .decode(b64)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("bridge key has the wrong length"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_survive_a_reload() {
        let dir = std::env::temp_dir().join(format!("lg-bridge-keys-{}", uuid::Uuid::new_v4().simple()));
        let a = MacKeys::load_or_create(&dir).unwrap();
        let b = MacKeys::load_or_create(&dir).unwrap();
        assert_eq!(a.id_public, b.id_public);
        assert_eq!(a.noise_public, b.noise_public);
        assert_eq!(a.mac_id().len(), 32);
        let _ = std::fs::remove_dir_all(dir);
    }
}
