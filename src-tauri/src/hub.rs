//! Owns one `GatewayClient` per saved gateway and keeps the saved list,
//! the live connections and the roster in sync.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::AppHandle;

use crate::activity::Activity;
use crate::gateway::client::{ConnStatus, EndpointSource, GatewayClient, Hooks};
use crate::gateway::identity::{now_ms, DeviceIdentity, DeviceTokenStore};
use crate::gateways::{self, AcpConfig, Discovered, GatewayEntry, GatewayStore, HermesConfig, OPENCLAW_GATEWAY_ID};
use crate::openclaw::config::{self, Credential, OpenclawPaths};
use crate::runtime::{acp::AcpClient, hermes::HermesClient, Runtime, RuntimeKind};

pub struct Hub {
    app_version: String,
    data_dir: PathBuf,
    identity: Arc<DeviceIdentity>,
    tokens: Arc<DeviceTokenStore>,
    paths: OpenclawPaths,
    store: GatewayStore,
    activity: Arc<Activity>,
    entries: Mutex<Vec<GatewayEntry>>,
    clients: Mutex<Vec<Runtime>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredGateway {
    #[serde(flatten)]
    pub gateway: Discovered,
    /// Id of the saved gateway at this address, if it's already added.
    pub added_as: Option<String>,
}

impl Hub {
    pub fn new(_app: &AppHandle, data_dir: PathBuf, app_version: String, activity: Arc<Activity>) -> anyhow::Result<Arc<Self>> {
        let identity = Arc::new(DeviceIdentity::load_or_create(&data_dir)?);
        let store = GatewayStore::new(&data_dir);
        let entries = store.load();
        Ok(Arc::new(Self {
            app_version,
            data_dir: data_dir.clone(),
            identity,
            tokens: Arc::new(DeviceTokenStore::new(&data_dir)),
            paths: OpenclawPaths::resolve(),
            store,
            activity,
            entries: Mutex::new(entries),
            clients: Mutex::new(Vec::new()),
        }))
    }

    pub fn start(&self) {
        let entries = self.entries.lock().unwrap().clone();
        for entry in entries {
            self.spawn(&entry);
        }
    }

    /// The name shown for a gateway: the user's, else derived from its address.
    fn display_name(&self, entry: &GatewayEntry) -> String {
        if !entry.name.trim().is_empty() {
            return entry.name.trim().to_string();
        }
        if let Some(acp) = &entry.acp {
            if !acp.agent_name.trim().is_empty() {
                return acp.agent_name.trim().to_string();
            }
        }
        if entry.kind == RuntimeKind::Hermes && entry.url.is_none() {
            return "Hermes".into();
        }
        match &entry.url {
            Some(url) => gateways::display_name_for(url),
            None => {
                let cfg = config::load_config(&self.paths).ok().flatten();
                let ep = config::resolve_endpoint(cfg.as_ref(), &self.paths);
                if ep.url.is_empty() {
                    "This Mac".into()
                } else {
                    gateways::display_name_for(&ep.url)
                }
            }
        }
    }

    fn spawn(&self, entry: &GatewayEntry) {
        let name = self.display_name(entry);
        let hooks = {
            let (a, b, c) = (self.activity.clone(), self.activity.clone(), self.activity.clone());
            let (gid_b, gid_c) = (entry.id.clone(), entry.id.clone());
            Hooks {
                status: Box::new(move |status| a.on_status(status)),
                event: Box::new(move |name, payload, _frame| b.on_event(&gid_b, name, payload)),
                connected: Box::new(move || c.on_connected(&gid_c)),
            }
        };
        let credential = self.store.credential(&entry.id);
        let client = match entry.kind {
            RuntimeKind::Openclaw => {
                let source = match &entry.url {
                    None => EndpointSource::OpenclawConfig,
                    Some(url) => EndpointSource::Fixed { url: url.clone() },
                };
                Runtime::OpenClaw(GatewayClient::new(
                    entry.id.clone(),
                    name.clone(),
                    source,
                    credential,
                    self.identity.clone(),
                    self.tokens.clone(),
                    self.paths.clone(),
                    self.app_version.clone(),
                    hooks,
                ))
            }
            RuntimeKind::Hermes => Runtime::Hermes(HermesClient::new(
                entry.id.clone(),
                name.clone(),
                entry.hermes.clone().unwrap_or_default(),
                credential,
                self.data_dir.clone(),
                hooks,
            )),
            RuntimeKind::Acp => Runtime::Acp(AcpClient::new(
                entry.id.clone(),
                name.clone(),
                entry.acp.clone().unwrap_or_default(),
                credential,
                self.data_dir.clone(),
                hooks,
            )),
        };
        self.activity.attach(client.clone(), name, entry.enabled);
        if entry.enabled {
            client.start();
        }
        self.clients.lock().unwrap().push(client);
    }

    pub fn client(&self, id: &str) -> Option<Runtime> {
        self.clients.lock().unwrap().iter().find(|c| c.id() == id).cloned()
    }

    /// The `~/.openclaw` gateway (Doctor fixes this one).
    pub fn openclaw_client(&self) -> Option<GatewayClient> {
        self.client(OPENCLAW_GATEWAY_ID).and_then(|r| r.as_openclaw().cloned())
    }

    pub fn statuses(&self) -> Vec<ConnStatus> {
        self.clients.lock().unwrap().iter().map(Runtime::status).collect()
    }

    pub fn reconnect(&self, id: Option<&str>) {
        for client in self.clients.lock().unwrap().iter() {
            if id.is_none_or(|id| client.id() == id) {
                client.reconnect_now();
            }
        }
    }

    pub fn set_credential(&self, id: &str, credential: Option<Credential>) -> anyhow::Result<()> {
        self.store.set_credential(id, credential.as_ref())?;
        if let Some(client) = self.client(id) {
            client.set_credential_override(credential);
        }
        Ok(())
    }

    /// The address the `~/.openclaw` gateway currently resolves to.
    fn openclaw_url(&self) -> Option<String> {
        let cfg = config::load_config(&self.paths).ok().flatten();
        let url = config::resolve_endpoint(cfg.as_ref(), &self.paths).url;
        (!url.is_empty()).then_some(url)
    }

    fn find_by_url(&self, url: &str) -> Option<String> {
        if self.openclaw_url().is_some_and(|u| gateways::same_endpoint(&u, url)) {
            return Some(OPENCLAW_GATEWAY_ID.into());
        }
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.url.as_deref().is_some_and(|u| gateways::same_endpoint(u, url)))
            .map(|e| e.id.clone())
    }

    pub fn add(&self, url: &str, name: Option<String>, credential: Option<Credential>) -> Result<GatewayEntry, String> {
        let url = gateways::normalize_url(url)?;
        if let Some(existing) = self.find_by_url(&url) {
            let name = self.entries.lock().unwrap().iter().find(|e| e.id == existing).map(|e| self.display_name(e));
            return Err(format!("{} is already added.", name.unwrap_or(url)));
        }
        let entry = GatewayEntry {
            id: format!("gw-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            name: name.map(|n| n.trim().to_string()).unwrap_or_default(),
            url: Some(url),
            enabled: true,
            added_at_ms: now_ms(),
            kind: RuntimeKind::Openclaw,
            acp: None,
            hermes: None,
        };
        if credential.is_some() {
            self.store.set_credential(&entry.id, credential.as_ref()).map_err(|e| e.to_string())?;
        }
        {
            let mut entries = self.entries.lock().unwrap();
            entries.push(entry.clone());
            self.store.save(&entries).map_err(|e| e.to_string())?;
        }
        self.spawn(&entry);
        Ok(entry)
    }

    /// Connect with an OpenClaw setup code: reuse the gateway at that address
    /// (or add it) and sign in with the code's one-time token. The gateway
    /// then asks for a one-time approval and issues this Mac a device token.
    pub fn use_setup_code(&self, code: &str) -> Result<String, String> {
        let (url, token) = gateways::decode_setup_code(code)?;
        let id = match self.find_by_url(&url) {
            Some(id) => id,
            None => self.add(&url, None, None)?.id,
        };
        let enabled = self.entries.lock().unwrap().iter().find(|e| e.id == id).map(|e| e.enabled).unwrap_or(true);
        if !enabled {
            self.update(&id, None, Some(true))?;
        }
        // Single-use token: keep it in memory only (not in the credentials file).
        let client = self.client(&id).ok_or("gateway went away")?;
        client.set_credential_override(Some(Credential::Bootstrap(token)));
        Ok(id)
    }

    /// Add a Hermes install or an ACP agent.
    pub fn add_bot(
        &self,
        kind: RuntimeKind,
        name: Option<String>,
        acp: Option<AcpConfig>,
        hermes: Option<HermesConfig>,
        credential: Option<Credential>,
    ) -> Result<GatewayEntry, String> {
        let url = match kind {
            RuntimeKind::Openclaw => return Err("use add() for OpenClaw gateways".into()),
            RuntimeKind::Hermes => hermes.as_ref().and_then(|h| h.url.clone()),
            RuntimeKind::Acp => acp.as_ref().and_then(|a| a.url.clone()),
        };
        if kind == RuntimeKind::Acp {
            let cfg = acp.as_ref().ok_or("missing ACP agent settings")?;
            if cfg.command.as_deref().unwrap_or("").trim().is_empty() && cfg.url.as_deref().unwrap_or("").trim().is_empty() {
                return Err("Tell littleguys how to start the agent (a command) or where it runs (a URL).".into());
            }
        }
        if let Some(url) = &url {
            let dup = self.entries.lock().unwrap().iter().any(|e| {
                // Hermes: one server, many profiles — each profile is its own bot.
                if let (RuntimeKind::Hermes, Some(new), Some(old)) = (kind, hermes.as_ref(), e.hermes.as_ref()) {
                    return e.kind == kind && gateways::same_hermes_target(old, new);
                }
                e.kind == kind
                    && (e.url.as_deref().is_some_and(|u| gateways::same_endpoint(u, url))
                        || e.hermes.as_ref().and_then(|h| h.url.as_deref()).is_some_and(|u| gateways::same_endpoint(u, url))
                        || e.acp.as_ref().and_then(|a| a.url.as_deref()).is_some_and(|u| gateways::same_endpoint(u, url)))
            });
            if dup {
                return Err("That bot is already added.".into());
            }
        }
        let entry = GatewayEntry {
            id: format!("{}-{}", match kind { RuntimeKind::Hermes => "hermes", _ => "acp" }, &uuid::Uuid::new_v4().simple().to_string()[..10]),
            name: name.map(|n| n.trim().to_string()).unwrap_or_default(),
            url: None,
            enabled: true,
            added_at_ms: now_ms(),
            kind,
            acp,
            hermes,
        };
        if credential.is_some() {
            self.store.set_credential(&entry.id, credential.as_ref()).map_err(|e| e.to_string())?;
        }
        {
            let mut entries = self.entries.lock().unwrap();
            entries.push(entry.clone());
            self.store.save(&entries).map_err(|e| e.to_string())?;
        }
        self.spawn(&entry);
        Ok(entry)
    }

    pub fn update(&self, id: &str, name: Option<String>, enabled: Option<bool>) -> Result<(), String> {
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            let entry = entries.iter_mut().find(|e| e.id == id).ok_or("unknown gateway")?;
            if let Some(name) = &name {
                entry.name = name.trim().to_string();
            }
            if let Some(enabled) = enabled {
                entry.enabled = enabled;
            }
            let entry = entry.clone();
            self.store.save(&entries).map_err(|e| e.to_string())?;
            entry
        };
        let display = self.display_name(&entry);
        if let Some(client) = self.client(id) {
            if name.is_some() {
                client.set_name(display.clone());
            }
            match enabled {
                Some(false) => client.stop(),
                Some(true) => {
                    // A stopped client can't be restarted; replace it.
                    client.stop();
                    self.clients.lock().unwrap().retain(|c| c.id() != id);
                    self.activity.detach(id);
                    self.spawn(&entry);
                }
                None => {}
            }
        }
        self.activity.set_gateway_meta(id, name.map(|_| display), enabled);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        if id == OPENCLAW_GATEWAY_ID {
            return Err("Your OpenClaw config's gateway can be turned off, but not removed.".into());
        }
        if let Some(client) = self.client(id) {
            client.stop();
        }
        self.clients.lock().unwrap().retain(|c| c.id() != id);
        {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|e| e.id != id);
            self.store.save(&entries).map_err(|e| e.to_string())?;
        }
        let _ = self.store.set_credential(id, None);
        self.activity.detach(id);
        Ok(())
    }

    pub async fn discover(&self) -> Vec<DiscoveredGateway> {
        gateways::discover()
            .await
            .into_iter()
            .map(|gateway| DiscoveredGateway { added_as: self.find_by_url(&gateway.url), gateway })
            .collect()
    }
}
