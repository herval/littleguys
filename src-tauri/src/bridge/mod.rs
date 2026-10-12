//! The bridge: littleguys for iPhone pairs with this Mac and reaches every bot
//! it runs, one to one, over an end-to-end encrypted Noise session on Wi-Fi or
//! Tailscale. The wire format is docs/BRIDGE-PROTOCOL.md.
//!
//! Nothing listens until a phone is paired or a pairing is open. The phone's
//! calls and events go through a [`BridgeHost`]: `AppHost` (Hub + Activity) in
//! the app, a fake in tests and `examples/bridge_demo.rs`.

pub mod addresses;
pub mod app_host;
mod bonjour;
pub mod commands;
mod conn;
pub mod demo;
pub mod host;
pub mod keys;
mod listener;
pub mod pairing;
pub mod phone;
pub mod push;
pub mod records;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

pub use addresses::{Address, AddressKind};
pub use host::{BridgeHost, HostEvent, MediaFetch, Notice, NoticeKind};

use crate::gateway::identity::{now_ms, write_private};
use keys::MacKeys;
use pairing::{b64, Pairings, Phone, PhoneStore, PAIRING_TTL_MS};

/// The port phones try first.
pub const DEFAULT_PORT: u16 = 47100;
/// Failed handshakes allowed per address per minute.
const MAX_FAILURES: usize = 10;

pub struct BridgeConfig {
    pub data_dir: PathBuf,
    pub mac_name: String,
    pub app_version: String,
    /// Preferred port; any free one when it's taken (0 = always any).
    pub port: u16,
    /// The push relay (`push::DEFAULT_RELAY`); bridge.json's `relayUrl` overrides it, `""` turns push off.
    pub relay_url: Option<String>,
    /// Advertise `_littleguys._tcp` (off in tests).
    pub bonjour: bool,
    /// Put 127.0.0.1 first in pairing links (`bridge_demo` and the Simulator).
    pub loopback_first: bool,
    pub tick_interval: Duration,
}

impl BridgeConfig {
    pub fn new(data_dir: PathBuf, mac_name: String, app_version: String) -> Self {
        Self {
            data_dir,
            mac_name,
            app_version,
            port: DEFAULT_PORT,
            relay_url: Some(push::DEFAULT_RELAY.into()),
            bonjour: true,
            loopback_first: false,
            tick_interval: Duration::from_secs(15),
        }
    }
}

/// What Settings → Phones shows (`bridge:state`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeState {
    /// "Let paired phones connect".
    pub enabled: bool,
    pub listening: bool,
    pub port: Option<u16>,
    pub mac_name: String,
    pub mac_id: String,
    pub addresses: Vec<Address>,
    /// Where pushes go; none when push is off.
    pub relay_url: Option<String>,
    pub phones: Vec<PhoneState>,
    pub pairing: Option<PairingState>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneState {
    pub key: String,
    pub name: String,
    pub model: String,
    pub paired_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub connected: bool,
    /// How it's connected right now.
    pub via: Option<Via>,
    /// Registered for pushes.
    pub push: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingState {
    pub uri: String,
    pub expires_at_ms: u64,
    /// Set once a phone used this code.
    pub paired_with: Option<PairedPhone>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedPhone {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Wifi,
    Tailscale,
    Local,
}

impl Via {
    fn of(ip: IpAddr) -> Self {
        if ip.is_loopback() {
            Via::Local
        } else if addresses::is_tailnet(ip) {
            Via::Tailscale
        } else {
            Via::Wifi
        }
    }
}

/// `{uri, expiresAt}` for `bridge_pair_start`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairStart {
    pub uri: String,
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default = "yes")]
    enabled: bool,
    /// The port last listened on, so saved addresses keep working.
    #[serde(default)]
    port: Option<u16>,
    /// Overrides the push relay; `""` turns push off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_url: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CloseReason {
    /// Removed in Settings: `phone.removed`, then 4010.
    Removed,
    /// The bridge stopped: 1000.
    Shutdown,
    /// The event queue is full: 4008.
    TooSlow,
    /// A malformed record or frame: 4000.
    Protocol,
}

pub(crate) struct SessionEntry {
    phone: String,
    via: Via,
    close: mpsc::UnboundedSender<CloseReason>,
    /// The last `presence` said the app is in the foreground: no pushes.
    foreground: bool,
}

struct Running {
    port: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _advert: Option<bonjour::Advert>,
}

impl Running {
    fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

struct PairingView {
    secret_id: u64,
    uri: String,
    expires_at_ms: u64,
    paired_with: Option<PairedPhone>,
}

type StateHook = Arc<dyn Fn(&BridgeState) + Send + Sync>;

pub(crate) struct Inner {
    cfg: BridgeConfig,
    pub(crate) keys: MacKeys,
    pub(crate) host: Arc<dyn BridgeHost>,
    rt: Handle,
    phones: Mutex<PhoneStore>,
    pub(crate) pairings: Mutex<Pairings>,
    settings: Mutex<Settings>,
    running: Mutex<Option<Running>>,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
    next_session: AtomicU64,
    failures: Mutex<HashMap<IpAddr, Vec<u64>>>,
    addresses: Mutex<Vec<Address>>,
    pairing: Mutex<Option<PairingView>>,
    on_state: Mutex<Option<StateHook>>,
}

/// Cheap to clone.
#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Inner>,
}

impl Bridge {
    /// `rt` runs the listener and sessions (Tauri's runtime in the app).
    pub fn new(cfg: BridgeConfig, host: Arc<dyn BridgeHost>, rt: Handle) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let keys = MacKeys::load_or_create(&cfg.data_dir)?;
        let phones = PhoneStore::load(&cfg.data_dir);
        let settings = std::fs::read_to_string(cfg.data_dir.join("bridge.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Settings { enabled: true, port: None, relay_url: None });
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                keys,
                host,
                rt,
                phones: Mutex::new(phones),
                pairings: Mutex::new(Pairings::default()),
                settings: Mutex::new(settings),
                running: Mutex::new(None),
                sessions: Mutex::new(HashMap::new()),
                next_session: AtomicU64::new(1),
                failures: Mutex::new(HashMap::new()),
                addresses: Mutex::new(Vec::new()),
                pairing: Mutex::new(None),
                on_state: Mutex::new(None),
            }),
        })
    }

    /// Called with the new state after every change.
    pub fn on_state(&self, hook: impl Fn(&BridgeState) + Send + Sync + 'static) {
        *self.inner.on_state.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Listen if there's a paired phone, and push the host's notices to registered phones.
    pub fn start(&self) {
        self.inner.reconcile();
        let inner = self.inner.clone();
        let mut events = inner.host.subscribe();
        self.inner.rt.spawn(async move {
            loop {
                match events.recv().await {
                    Ok(HostEvent::Notice(notice)) => {
                        inner.push_notice(&notice).await;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Push a notice to every registered phone that isn't in the foreground, now;
    /// one `(phone name, relay answer)` each.
    pub async fn push(&self, notice: &Notice) -> Vec<push::Delivery> {
        self.inner.push_notice(notice).await
    }

    /// Point pushes at another relay (`None` = the default, `""` = off).
    pub fn set_relay_url(&self, url: Option<String>) -> anyhow::Result<()> {
        self.inner.settings.lock().unwrap().relay_url = url;
        self.inner.save_settings()?;
        self.inner.publish();
        Ok(())
    }

    pub fn state(&self) -> BridgeState {
        self.inner.state()
    }

    pub fn mac_id(&self) -> String {
        self.inner.keys.mac_id()
    }

    /// The port being listened on, if any.
    pub fn port(&self) -> Option<u16> {
        self.inner.running.lock().unwrap().as_ref().map(|r| r.port)
    }

    /// "Let paired phones connect". Off closes every session; phones stay paired.
    pub fn set_enabled(&self, on: bool) -> anyhow::Result<()> {
        self.inner.settings.lock().unwrap().enabled = on;
        self.inner.save_settings()?;
        if !on {
            self.inner.pairings.lock().unwrap().close_all();
            *self.inner.pairing.lock().unwrap() = None;
        }
        self.inner.reconcile();
        Ok(())
    }

    /// Open a pairing: a fresh one-time secret, and the link to show as a QR code.
    pub async fn pair_start(&self) -> anyhow::Result<PairStart> {
        let inner = &self.inner;
        if !inner.settings.lock().unwrap().enabled {
            inner.settings.lock().unwrap().enabled = true;
            inner.save_settings()?;
        }
        let now = now_ms();
        let (secret_id, secret) = inner.pairings.lock().unwrap().open(now);
        inner.reconcile();
        let port = self.port().ok_or_else(|| anyhow::anyhow!("littleguys couldn't start listening for phones"))?;
        let addresses = addresses::discover(port, inner.cfg.loopback_first).await;
        *inner.addresses.lock().unwrap() = addresses.clone();
        let uri = invite_uri(
            &inner.cfg.mac_name,
            &inner.keys.id_b64(),
            &inner.keys.noise_b64(),
            &addresses,
            inner.relay_url().as_deref(),
            &b64(&secret),
        );
        let expires_at_ms = now + PAIRING_TTL_MS;
        *inner.pairing.lock().unwrap() = Some(PairingView { secret_id, uri: uri.clone(), expires_at_ms, paired_with: None });
        // Stop listening when the code runs out and no phone used it.
        let after = self.inner.clone();
        inner.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(PAIRING_TTL_MS + 500)).await;
            after.reconcile();
        });
        inner.publish();
        Ok(PairStart { uri, expires_at: expires_at_ms })
    }

    pub fn pair_cancel(&self) {
        self.inner.pairings.lock().unwrap().close_all();
        *self.inner.pairing.lock().unwrap() = None;
        self.inner.reconcile();
    }

    /// Forget a phone and end its sessions (`phone.removed`, then 4010).
    pub fn remove_phone(&self, key: &str) -> anyhow::Result<()> {
        let registration = self.inner.phones.lock().unwrap().get(key).and_then(|p| p.push.clone());
        self.inner.phones.lock().unwrap().remove(key)?;
        if let Some(r) = registration {
            self.inner.forget_registration(r.registration_id);
        }
        self.inner.close_phone(key, CloseReason::Removed);
        self.inner.reconcile();
        Ok(())
    }

    pub fn rename_phone(&self, key: &str, name: &str) -> anyhow::Result<()> {
        self.inner.phones.lock().unwrap().rename(key, name)?;
        self.inner.publish();
        Ok(())
    }

    /// Stop listening and end every session (for tests and examples).
    pub fn shutdown(&self) {
        if let Some(running) = self.inner.running.lock().unwrap().take() {
            running.stop();
        }
        self.inner.close_all(CloseReason::Shutdown);
    }
}

impl Inner {
    /// Listen exactly when it's on and there's a paired phone or an open pairing.
    fn reconcile(self: &Arc<Self>) {
        let enabled = self.settings.lock().unwrap().enabled;
        let has_phones = !self.phones.lock().unwrap().all().is_empty();
        let pairing_open = self.pairings.lock().unwrap().current(now_ms()).is_some();
        let want = enabled && (has_phones || pairing_open);
        let mut stopped = false;
        {
            let mut running = self.running.lock().unwrap();
            if want && running.is_none() {
                let preferred = self.settings.lock().unwrap().port.unwrap_or(self.cfg.port);
                match listener::start(self.clone(), preferred) {
                    Ok(r) => {
                        log::info!("bridge listening on port {}", r.port);
                        let port = r.port;
                        *running = Some(r);
                        let changed = self.settings.lock().unwrap().port.replace(port) != Some(port);
                        if changed && self.cfg.port != 0 {
                            let _ = self.save_settings();
                        }
                        // What Settings shows as reachable (pairing refreshes it too).
                        let this = self.clone();
                        self.rt.spawn(async move {
                            let found = addresses::discover(port, this.cfg.loopback_first).await;
                            *this.addresses.lock().unwrap() = found;
                            this.publish();
                        });
                    }
                    Err(e) => log::warn!("bridge couldn't listen: {e:#}"),
                }
            } else if !want {
                if let Some(r) = running.take() {
                    r.stop();
                    stopped = true;
                }
            }
        }
        if stopped {
            log::info!("bridge stopped listening");
            self.close_all(CloseReason::Shutdown);
        }
        self.publish();
    }

    fn save_settings(&self) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(&*self.settings.lock().unwrap())?;
        write_private(&self.cfg.data_dir.join("bridge.json"), &bytes)
    }

    pub(crate) fn state(&self) -> BridgeState {
        let enabled = self.settings.lock().unwrap().enabled;
        let port = self.running.lock().unwrap().as_ref().map(|r| r.port);
        let live: HashMap<String, Via> =
            self.sessions.lock().unwrap().values().map(|s| (s.phone.clone(), s.via)).collect();
        let phones = self
            .phones
            .lock()
            .unwrap()
            .all()
            .iter()
            .map(|p| PhoneState {
                key: p.key.clone(),
                name: p.name.clone(),
                model: p.model.clone(),
                paired_at_ms: p.paired_at_ms,
                last_seen_at_ms: p.last_seen_at_ms,
                connected: live.contains_key(&p.key),
                via: live.get(&p.key).copied(),
                push: p.push.is_some(),
            })
            .collect();
        let pairing = self.pairing.lock().unwrap().as_ref().map(|p| PairingState {
            uri: p.uri.clone(),
            expires_at_ms: p.expires_at_ms,
            paired_with: p.paired_with.clone(),
        });
        BridgeState {
            enabled,
            listening: port.is_some(),
            port,
            mac_name: self.cfg.mac_name.clone(),
            mac_id: self.keys.mac_id(),
            addresses: self.addresses.lock().unwrap().clone(),
            relay_url: self.relay_url(),
            phones,
            pairing,
        }
    }

    pub(crate) fn publish(&self) {
        let hook = self.on_state.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(&self.state());
        }
    }

    // ---- sessions (conn.rs) ------------------------------------------------------

    pub(crate) fn mac_name(&self) -> &str {
        &self.cfg.mac_name
    }

    pub(crate) fn app_version(&self) -> &str {
        &self.cfg.app_version
    }

    /// The push relay in use, if any.
    pub(crate) fn relay_url(&self) -> Option<String> {
        match &self.settings.lock().unwrap().relay_url {
            Some(url) => Some(url.trim().to_string()).filter(|u| !u.is_empty()),
            None => self.cfg.relay_url.clone(),
        }
    }

    pub(crate) fn tick_interval(&self) -> Duration {
        self.cfg.tick_interval
    }

    pub(crate) fn rt(&self) -> &Handle {
        &self.rt
    }

    pub(crate) fn is_paired(&self, key: &str) -> bool {
        self.phones.lock().unwrap().get(key).is_some()
    }

    pub(crate) fn register_session(&self, phone: &str, ip: IpAddr, close: mpsc::UnboundedSender<CloseReason>) -> u64 {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        self.sessions.lock().unwrap().insert(id, SessionEntry { phone: phone.to_string(), via: Via::of(ip), close, foreground: false });
        self.phones.lock().unwrap().touch(phone, now_ms());
        self.publish();
        id
    }

    pub(crate) fn unregister_session(self: &Arc<Self>, id: u64) {
        let entry = self.sessions.lock().unwrap().remove(&id);
        if let Some(entry) = entry {
            let mut phones = self.phones.lock().unwrap();
            phones.touch(&entry.phone, now_ms());
            if phones.get(&entry.phone).is_some() {
                let _ = phones.save();
            }
        }
        self.reconcile();
    }

    /// The first transport message of a pairing session arrived: trust the phone.
    pub(crate) fn commit_pairing(&self, secret_id: u64, phone: Phone) -> bool {
        if !self.pairings.lock().unwrap().commit(secret_id) {
            return false;
        }
        let paired = PairedPhone { key: phone.key.clone(), name: phone.name.clone() };
        if let Err(e) = self.phones.lock().unwrap().upsert(phone) {
            log::warn!("couldn't save the paired phone: {e:#}");
            return false;
        }
        if let Some(view) = self.pairing.lock().unwrap().as_mut().filter(|v| v.secret_id == secret_id) {
            view.paired_with = Some(paired);
        }
        true
    }

    /// `phone.forget`: the phone unpaired itself.
    pub(crate) fn forget_phone(self: &Arc<Self>, key: &str) {
        let registration = self.phones.lock().unwrap().get(key).and_then(|p| p.push.clone());
        if let Err(e) = self.phones.lock().unwrap().remove(key) {
            log::warn!("couldn't forget the phone: {e:#}");
        }
        if let Some(r) = registration {
            self.forget_registration(r.registration_id);
        }
        self.publish();
    }

    // ---- push (push.rs) ------------------------------------------------------------

    pub(crate) fn has_push(&self, key: &str) -> bool {
        self.phones.lock().unwrap().get(key).is_some_and(|p| p.push.is_some())
    }

    pub(crate) fn set_push(&self, key: &str, push: Option<pairing::PushRegistration>) {
        if let Err(e) = self.phones.lock().unwrap().set_push(key, push) {
            log::warn!("couldn't save the push registration: {e:#}");
        }
        self.publish();
    }

    pub(crate) fn drop_registration(&self, registration_id: &str) {
        if let Err(e) = self.phones.lock().unwrap().drop_registration(registration_id) {
            log::warn!("couldn't drop the push registration: {e:#}");
        }
        self.publish();
    }

    /// `presence`: whether this session's app is in the foreground.
    pub(crate) fn set_foreground(&self, session: u64, foreground: bool) {
        if let Some(s) = self.sessions.lock().unwrap().get_mut(&session) {
            s.foreground = foreground;
        }
    }

    /// Registered phones that aren't looking at the app right now: `(name, registration)`.
    pub(crate) fn push_targets(&self) -> Vec<(String, pairing::PushRegistration)> {
        let sessions = self.sessions.lock().unwrap();
        let in_front = |key: &str| sessions.values().any(|s| s.phone == key && s.foreground);
        self.phones
            .lock()
            .unwrap()
            .all()
            .iter()
            .filter(|p| !in_front(&p.key))
            .filter_map(|p| p.push.clone().map(|r| (p.name.clone(), r)))
            .collect()
    }

    fn close_phone(&self, key: &str, reason: CloseReason) {
        for s in self.sessions.lock().unwrap().values().filter(|s| s.phone == key) {
            let _ = s.close.send(reason);
        }
    }

    fn close_all(&self, reason: CloseReason) {
        for s in self.sessions.lock().unwrap().values() {
            let _ = s.close.send(reason);
        }
    }

    /// `Some(retryAfterMs)` when this address failed too many handshakes in the last minute.
    pub(crate) fn rate_limited(&self, ip: IpAddr) -> Option<u64> {
        let now = now_ms();
        let mut failures = self.failures.lock().unwrap();
        let list = failures.entry(ip).or_default();
        list.retain(|t| now.saturating_sub(*t) < 60_000);
        (list.len() >= MAX_FAILURES).then(|| (list[0] + 60_000).saturating_sub(now).max(1))
    }

    pub(crate) fn note_failure(&self, ip: IpAddr) {
        self.failures.lock().unwrap().entry(ip).or_default().push(now_ms());
    }
}

/// `littleguys://pair?v=1&name=…&id=…&nk=…&addr=…[&relay=…]&secret=…`
pub fn invite_uri(name: &str, id: &str, nk: &str, addresses: &[Address], relay: Option<&str>, secret: &str) -> String {
    let addr = addresses.iter().map(Address::host_port).collect::<Vec<_>>().join(",");
    let mut uri = format!("littleguys://pair?v=1&name={}&id={id}&nk={nk}&addr={addr}", encode_query(name));
    if let Some(relay) = relay {
        uri.push_str(&format!("&relay={}", encode_query(relay)));
    }
    uri.push_str(&format!("&secret={secret}"));
    uri
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// This Mac's name, as people see it (System Settings → General → Sharing).
pub fn computer_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Mac".into())
}
