//! The gateways littleguys talks to. The one described by `~/.openclaw` is
//! always first (id "openclaw"); any others are added in the app and stored in
//! littleguys' own data dir — OpenClaw has no notion of "my other gateways".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::openclaw::config::Credential;
use crate::runtime::RuntimeKind;

pub const OPENCLAW_GATEWAY_ID: &str = "openclaw";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayEntry {
    pub id: String,
    /// User-facing name. Empty = derive from the URL.
    #[serde(default)]
    pub name: String,
    /// Fixed endpoint. `None` for the `~/.openclaw` gateway, which is
    /// re-resolved from the config on every connect.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub added_at_ms: u64,
    /// What kind of bot runtime lives here (default: an OpenClaw gateway).
    #[serde(default)]
    pub kind: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hermes: Option<HermesConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AcpTransport {
    /// Spawn the agent and speak JSON-RPC over its stdin/stdout.
    #[default]
    Stdio,
    /// Connect to an agent serving ACP at a WebSocket endpoint (e.g. `goose serve`).
    Websocket,
}

/// An ACP agent (Claude Code, Codex, Gemini CLI, Goose, …). One entry = one guy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AcpConfig {
    #[serde(default)]
    pub transport: AcpTransport,
    /// stdio: program to run (resolved with the login-shell PATH), e.g. "npx".
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// websocket: `ws(s)://host:port/acp`.
    #[serde(default)]
    pub url: Option<String>,
    /// Working folder the agent operates in (its "workspace").
    #[serde(default)]
    pub cwd: Option<String>,
    /// The guy's name and face.
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub emoji: Option<String>,
    /// ACP registry id this came from (e.g. "claude-acp"), if any.
    #[serde(default)]
    pub registry_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HermesMode {
    /// The machine's `hermes serve` backend (JSON-RPC over WebSocket) — full features.
    #[default]
    Local,
    /// Hermes' OpenAI-compatible API server (`API_SERVER_KEY` bearer) — for remote hosts.
    Api,
}

/// A Hermes Agent install. Each Hermes profile shows up as a guy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HermesConfig {
    #[serde(default)]
    pub mode: HermesMode,
    /// local: backend base URL (default: discover via ~/.hermes/spawn-ledger.json, else
    /// http://127.0.0.1:9119). api: API server base URL, e.g. http://host:8642.
    #[serde(default)]
    pub url: Option<String>,
    /// Only show this profile (None = all profiles).
    #[serde(default)]
    pub profile: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
enum StoredCredential {
    Token(String),
    Password(String),
}

pub struct GatewayStore {
    dir: PathBuf,
}

impl GatewayStore {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    /// Saved gateways, with the `~/.openclaw` entry always present and first.
    pub fn load(&self) -> Vec<GatewayEntry> {
        let mut entries: Vec<GatewayEntry> = std::fs::read_to_string(self.dir.join("gateways.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        if let Some(pos) = entries.iter().position(|e| e.id == OPENCLAW_GATEWAY_ID) {
            let entry = entries.remove(pos);
            entries.insert(0, GatewayEntry { url: None, kind: RuntimeKind::Openclaw, ..entry });
        } else {
            entries.insert(
                0,
                GatewayEntry {
                    id: OPENCLAW_GATEWAY_ID.into(),
                    name: String::new(),
                    url: None,
                    enabled: true,
                    added_at_ms: 0,
                    kind: RuntimeKind::Openclaw,
                    acp: None,
                    hermes: None,
                },
            );
        }
        entries
    }

    pub fn save(&self, entries: &[GatewayEntry]) -> anyhow::Result<()> {
        crate::gateway::identity::write_private(&self.dir.join("gateways.json"), &serde_json::to_vec_pretty(entries)?)
    }

    fn read_credentials(&self) -> HashMap<String, StoredCredential> {
        std::fs::read_to_string(self.dir.join("gateway-credentials.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn credential(&self, id: &str) -> Option<Credential> {
        self.read_credentials().remove(id).map(|c| match c {
            StoredCredential::Token(t) => Credential::Token(t),
            StoredCredential::Password(p) => Credential::Password(p),
        })
    }

    pub fn set_credential(&self, id: &str, credential: Option<&Credential>) -> anyhow::Result<()> {
        let mut all = self.read_credentials();
        match credential {
            Some(Credential::Token(t)) => all.insert(id.into(), StoredCredential::Token(t.clone())),
            Some(Credential::Password(p)) => all.insert(id.into(), StoredCredential::Password(p.clone())),
            _ => all.remove(id),
        };
        crate::gateway::identity::write_private(
            &self.dir.join("gateway-credentials.json"),
            &serde_json::to_vec_pretty(&all)?,
        )
    }
}

/// Accept what people paste — `attic.tail1234.ts.net`, `https://attic…`,
/// `192.168.1.20:18789`, `ws://…` — and turn it into a gateway WebSocket URL.
pub fn normalize_url(input: &str) -> Result<String, String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Enter a gateway address.".into());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some(("ws", r)) => ("ws", r),
        Some(("wss", r)) => ("wss", r),
        Some(("http", r)) => ("ws", r),
        Some(("https", r)) => ("wss", r),
        Some((other, _)) => return Err(format!("{other}:// isn't a gateway address.")),
        None => {
            let (host, port) = split_host_port(raw);
            let private = host.ends_with(".local") || host == "localhost" || host.parse::<std::net::IpAddr>().is_ok();
            return Ok(match (port, private) {
                (Some(443), _) => format!("wss://{host}"),
                (Some(p), _) => format!("ws://{host}:{p}"),
                (None, true) => format!("ws://{host}:{}", crate::openclaw::config::DEFAULT_GATEWAY_PORT),
                // MagicDNS / public names: Tailscale Serve or TLS on 443.
                (None, false) => format!("wss://{host}"),
            });
        }
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("That doesn't look like a gateway address.".into());
    }
    Ok(format!("{scheme}://{rest}"))
}

fn split_host_port(s: &str) -> (&str, Option<u16>) {
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h, Some(port)),
            Err(_) => (s, None),
        },
        _ => (s, None),
    }
}

/// An OpenClaw setup code (`openclaw qr --setup-code-only`, or the Control UI's
/// pairing dialog): base64url JSON `{url, bootstrapToken, …}`, optionally
/// prefixed with `oc-pair://`.
pub fn decode_setup_code(input: &str) -> Result<(String, String), String> {
    use base64::Engine;
    let trimmed = input.trim();
    let code = if trimmed.len() >= 10 && trimmed[..10].eq_ignore_ascii_case("oc-pair://") { &trimmed[10..] } else { trimmed };
    let invalid = || "That doesn't look like an OpenClaw setup code.".to_string();
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(invalid());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(code.trim_end_matches('='))
        .map_err(|_| invalid())?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let url = payload.get("url").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty()).ok_or_else(invalid)?;
    let token = payload
        .get("bootstrapToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(invalid)?;
    Ok((normalize_url(url)?, token.to_string()))
}

/// "wss://attic.tail1234.ts.net" → "attic"; loopback → "This Mac".
pub fn display_name_for(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    let (host, _) = split_host_port(host);
    if host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]" {
        return "This Mac".into();
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    host.split('.').next().unwrap_or(host).to_string()
}

pub fn same_endpoint(a: &str, b: &str) -> bool {
    fn key(u: &str) -> String {
        u.trim().trim_end_matches('/').to_lowercase().replace("wss://", "").replace("ws://", "").replace(":443", "")
    }
    key(a) == key(b)
}

/// Whether two Hermes configs show the same guys: same server and same profile. One server
/// can host several profiles (api mode reaches them at `/p/<profile>`), each its own entry.
pub fn same_hermes_target(a: &HermesConfig, b: &HermesConfig) -> bool {
    /// (server, profile): the profile comes from the Profile field, else a `/p/<profile>` URL;
    /// with neither, api mode is the server's default profile and local mode shows them all.
    fn target(c: &HermesConfig) -> (String, Option<String>) {
        let url = c.url.as_deref().unwrap_or("").trim().trim_end_matches('/');
        let (server, in_url) = match url.find("/p/") {
            Some(i) => (&url[..i], url[i + 3..].split('/').next()),
            None => (url, None),
        };
        let profile = c.profile.as_deref().map(str::trim).filter(|p| !p.is_empty()).or(in_url.filter(|p| !p.is_empty()));
        let profile = match (profile, c.mode) {
            (None, HermesMode::Api) => Some("default"),
            (p, _) => p,
        };
        (server.to_string(), profile.map(str::to_string))
    }
    let (a_server, a_profile) = target(a);
    let (b_server, b_profile) = target(b);
    same_endpoint(&a_server, &b_server) && a_profile == b_profile
}

// ---- discovery ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    pub name: String,
    pub url: String,
    /// "tailscale" | "bonjour"
    pub via: String,
    pub host: String,
}

/// Find OpenClaw gateways on the tailnet (Tailscale peers answering
/// `/health`) and on the LAN (Bonjour `_openclaw-gw._tcp`).
pub async fn discover() -> Vec<Discovered> {
    let (tail, lan) = tokio::join!(discover_tailscale(), discover_bonjour());
    let mut out: Vec<Discovered> = Vec::new();
    for d in tail.into_iter().chain(lan) {
        if !out.iter().any(|o| same_endpoint(&o.url, &d.url) || (o.host == d.host && !d.host.is_empty())) {
            out.push(d);
        }
    }
    out
}

fn tailscale_cli() -> Option<PathBuf> {
    [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// MagicDNS names of online tailnet machines (this Mac included).
pub async fn tailnet_hosts() -> Vec<String> {
    let Some(cli) = tailscale_cli() else { return vec![] };
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(cli).args(["status", "--json"]).output(),
    )
    .await;
    let Ok(Ok(output)) = output else { return vec![] };
    let Ok(status) = serde_json::from_slice::<Value>(&output.stdout) else { return vec![] };
    let mut hosts: Vec<String> = Vec::new();
    let mut consider = |node: &Value| {
        let online = node.get("Online").and_then(Value::as_bool).unwrap_or(true);
        let os = node.get("OS").and_then(Value::as_str).unwrap_or_default().to_lowercase();
        let dns = node.get("DNSName").and_then(Value::as_str).unwrap_or_default().trim_end_matches('.');
        if online && !dns.is_empty() && !matches!(os.as_str(), "ios" | "android" | "tvos") {
            hosts.push(dns.to_string());
        }
    };
    if let Some(me) = status.get("Self") {
        consider(me);
    }
    if let Some(peers) = status.get("Peer").and_then(Value::as_object) {
        peers.values().for_each(&mut consider);
    }
    hosts
}

async fn discover_tailscale() -> Vec<Discovered> {
    let hosts = tailnet_hosts().await;
    if hosts.is_empty() {
        return vec![];
    }
    let client = match reqwest::Client::builder().timeout(Duration::from_millis(2500)).build() {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let probes = hosts.into_iter().map(|host| {
        let client = client.clone();
        async move {
            // Tailscale Serve (HTTPS on 443) first, then a direct tailnet bind.
            for (health, url) in [
                (format!("https://{host}/health"), format!("wss://{host}")),
                (format!("http://{host}:18789/health"), format!("ws://{host}:18789")),
            ] {
                if is_gateway_health(&client, &health).await {
                    return Some(Discovered { name: display_name_for(&url), url, via: "tailscale".into(), host: host.clone() });
                }
            }
            None
        }
    });
    futures_util::future::join_all(probes).await.into_iter().flatten().collect()
}

async fn is_gateway_health(client: &reqwest::Client, url: &str) -> bool {
    let Ok(resp) = client.get(url).send().await else { return false };
    if !resp.status().is_success() {
        return false;
    }
    resp.json::<Value>()
        .await
        .map(|v| v.get("ok").and_then(Value::as_bool) == Some(true) && v.get("status").is_some())
        .unwrap_or(false)
}

async fn discover_bonjour() -> Vec<Discovered> {
    tokio::task::spawn_blocking(|| {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return vec![] };
        let Ok(rx) = daemon.browse("_openclaw-gw._tcp.local.") else { return vec![] };
        let deadline = std::time::Instant::now() + Duration::from_millis(2500);
        let mut found = Vec::new();
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(svc)) => {
                    let host = svc.get_hostname().trim_end_matches('.').to_string();
                    let port = svc.get_port();
                    let tls = svc.get_property_val_str("gatewayTls") == Some("1");
                    let scheme = if tls { "wss" } else { "ws" };
                    let url = format!("{scheme}://{host}:{port}");
                    let name = svc
                        .get_property_val_str("displayName")
                        .map(str::to_string)
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| display_name_for(&url));
                    found.push(Discovered { name, url, via: "bonjour".into(), host });
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
        found
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pasted_addresses() {
        assert_eq!(normalize_url("attic.tail1234.ts.net").unwrap(), "wss://attic.tail1234.ts.net");
        assert_eq!(normalize_url("https://attic.tail1234.ts.net/").unwrap(), "wss://attic.tail1234.ts.net");
        assert_eq!(normalize_url("192.168.1.20").unwrap(), "ws://192.168.1.20:18789");
        assert_eq!(normalize_url("studio.local:19000").unwrap(), "ws://studio.local:19000");
        assert_eq!(normalize_url("ws://10.0.0.2:18789").unwrap(), "ws://10.0.0.2:18789");
        assert!(normalize_url("ftp://nope").is_err());
        assert!(normalize_url("  ").is_err());
    }

    #[test]
    fn decodes_setup_codes() {
        use base64::Engine;
        let code = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"url":"wss://attic.tail1234.ts.net","bootstrapToken":"boot-123"}"#);
        let expected = ("wss://attic.tail1234.ts.net".to_string(), "boot-123".to_string());
        assert_eq!(decode_setup_code(&code).unwrap(), expected);
        assert_eq!(decode_setup_code(&format!("  oc-pair://{code}\n")).unwrap(), expected);
        assert!(decode_setup_code("not a code!").is_err());
    }

    #[test]
    fn names_gateways_after_their_host() {
        assert_eq!(display_name_for("wss://attic.tail1234.ts.net"), "attic");
        assert_eq!(display_name_for("ws://127.0.0.1:18789"), "This Mac");
        assert_eq!(display_name_for("ws://192.168.1.20:18789"), "192.168.1.20");
    }

    #[test]
    fn matches_equivalent_endpoints() {
        assert!(same_endpoint("wss://attic.ts.net", "wss://attic.ts.net:443/"));
        assert!(!same_endpoint("wss://attic.ts.net", "wss://garage.ts.net"));
    }

    #[test]
    fn hermes_profiles_on_one_server_are_different_bots() {
        let api = |url: &str, profile: &str| HermesConfig {
            mode: HermesMode::Api,
            url: Some(url.into()),
            profile: Some(profile.into()).filter(|p: &String| !p.is_empty()),
        };
        let server = "http://10.2.0.22:8642";
        assert!(!same_hermes_target(&api(server, ""), &api(server, "infra")));
        assert!(!same_hermes_target(&api(server, "radar"), &api(server, "infra")));
        assert!(same_hermes_target(&api(server, "infra"), &api(server, "infra")));
        assert!(same_hermes_target(&api(server, ""), &api(server, "default")));
        assert!(same_hermes_target(&api(server, ""), &api("http://10.2.0.22:8642/", " ")));
        assert!(same_hermes_target(&api(server, "infra"), &api("http://10.2.0.22:8642/p/infra", "")));
        assert!(same_hermes_target(&api(server, "infra"), &api("http://10.2.0.22:8642/p/infra/", "infra")));
        assert!(!same_hermes_target(&api(server, "infra"), &api("http://10.2.0.23:8642", "infra")));
        let local = |profile: Option<&str>| HermesConfig { mode: HermesMode::Local, url: None, profile: profile.map(str::to_string) };
        assert!(same_hermes_target(&local(None), &local(None)));
        assert!(!same_hermes_target(&local(None), &local(Some("infra"))));
    }
}
