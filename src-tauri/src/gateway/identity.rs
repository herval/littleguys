//! Device identity for gateway pairing: an Ed25519 keypair plus the per-device
//! tokens the gateway issues after pairing. Stored as 0600 files in the app's
//! data dir (the same approach the openclaw CLI uses for its own identity), so
//! dev rebuilds with changing code signatures don't trigger Keychain prompts.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub struct DeviceIdentity {
    signing_key: SigningKey,
    pub device_id: String,
    pub public_key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    version: u32,
    /// Base64 (standard) of the 32-byte Ed25519 seed.
    seed: String,
    created_at_ms: u64,
}

impl DeviceIdentity {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(&seed);
        let raw_public = signing_key.verifying_key().to_bytes();
        Self {
            device_id: hex(&Sha256::digest(raw_public)),
            public_key: URL_SAFE_NO_PAD.encode(raw_public),
            signing_key,
        }
    }

    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("device-identity.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let stored: StoredIdentity = serde_json::from_str(&raw)?;
            let seed: [u8; 32] = STANDARD
                .decode(stored.seed)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("device identity seed has the wrong length"))?;
            return Ok(Self::from_seed(seed));
        }
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        let stored = StoredIdentity {
            version: 1,
            seed: STANDARD.encode(seed),
            created_at_ms: now_ms(),
        };
        write_private(&path, &serde_json::to_vec_pretty(&stored)?)?;
        Ok(Self::from_seed(seed))
    }

    /// Unpadded base64url Ed25519 signature over the UTF-8 payload.
    pub fn sign(&self, payload: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload.as_bytes()).to_bytes())
    }
}

pub struct DeviceAuthPayload<'a> {
    pub device_id: &'a str,
    pub client_id: &'a str,
    pub client_mode: &'a str,
    pub role: &'a str,
    pub scopes: &'a [String],
    pub signed_at_ms: u64,
    pub token: Option<&'a str>,
    pub nonce: &'a str,
    pub platform: &'a str,
    pub device_family: &'a str,
}

/// `v3|deviceId|clientId|mode|role|scopes|signedAtMs|token|nonce|platform|deviceFamily`
/// (packages/gateway-client/src/device-auth.ts `buildDeviceAuthPayloadV3`).
pub fn build_payload_v3(p: &DeviceAuthPayload) -> String {
    [
        "v3",
        p.device_id,
        p.client_id,
        p.client_mode,
        p.role,
        &p.scopes.join(","),
        &p.signed_at_ms.to_string(),
        p.token.unwrap_or(""),
        p.nonce,
        &normalize_metadata(p.platform),
        &normalize_metadata(p.device_family),
    ]
    .join("|")
}

/// Trim and ASCII-lowercase, matching `normalizeDeviceMetadataForAuth`.
fn normalize_metadata(value: &str) -> String {
    value.trim().chars().map(|c| c.to_ascii_lowercase()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceToken {
    pub token: String,
    pub scopes: Vec<String>,
}

/// Device tokens keyed by gateway URL + role, so switching gateways never
/// replays a token to the wrong server.
pub struct DeviceTokenStore {
    path: PathBuf,
}

impl DeviceTokenStore {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("device-tokens.json") }
    }

    fn read_all(&self) -> HashMap<String, DeviceToken> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn key(url: &str, role: &str) -> String {
        format!("{}|{role}", url.trim_end_matches('/'))
    }

    pub fn load(&self, url: &str, role: &str) -> Option<DeviceToken> {
        self.read_all().remove(&Self::key(url, role))
    }

    pub fn store(&self, url: &str, role: &str, token: DeviceToken) -> anyhow::Result<()> {
        let mut all = self.read_all();
        all.insert(Self::key(url, role), token);
        write_private(&self.path, &serde_json::to_vec_pretty(&all)?)
    }

    pub fn clear(&self, url: &str, role: &str) -> anyhow::Result<()> {
        let mut all = self.read_all();
        if all.remove(&Self::key(url, role)).is_some() {
            write_private(&self.path, &serde_json::to_vec_pretty(&all)?)?;
        }
        Ok(())
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = crate::platform::private_file().write(true).create(true).truncate(true).open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture produced by running the reference implementation
    // (node:crypto ed25519 + buildDeviceAuthPayloadV3) with seed = [7; 32].
    // See scripts/device-auth-fixture.mjs.
    const FIXTURE: &str = include_str!("../../tests/device-auth-fixture.json");

    #[test]
    fn matches_reference_implementation() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let identity = DeviceIdentity::from_seed([7u8; 32]);
        assert_eq!(identity.device_id, fixture["deviceId"].as_str().unwrap());
        assert_eq!(identity.public_key, fixture["publicKey"].as_str().unwrap());

        let scopes: Vec<String> = fixture["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let payload = build_payload_v3(&DeviceAuthPayload {
            device_id: &identity.device_id,
            client_id: "openclaw-macos",
            client_mode: "ui",
            role: "operator",
            scopes: &scopes,
            signed_at_ms: 1_737_264_000_000,
            token: Some("shared-token"),
            nonce: "nonce-123",
            platform: " MacOS ",
            device_family: "Mac",
        });
        assert_eq!(payload, fixture["payload"].as_str().unwrap());
        assert_eq!(identity.sign(&payload), fixture["signature"].as_str().unwrap());
    }
}
