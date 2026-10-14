//! IPC surface for the webviews.

use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, State};

use crate::activity::{Activity, NotifyPrefs, Roster};
use crate::gateway::client::{ConnStatus, RpcError};
use crate::gateways::{AcpConfig, ClaudeCodeConfig, GatewayEntry, HermesConfig};
use crate::runtime::RuntimeKind;
use crate::updater::{UpdateState, Updates};
use crate::hub::{DiscoveredGateway, Hub};
use crate::openclaw::config::Credential;
use crate::windows::{self, Navigate};

fn credential(kind: Option<String>, value: Option<String>) -> Option<Credential> {
    let value = value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())?;
    Some(match kind.as_deref() {
        Some("password") => Credential::Password(value),
        _ => Credential::Token(value),
    })
}

/// Generic gateway RPC. Typed wrappers live in src/lib/gateway.ts.
#[tauri::command]
pub async fn gw_call(
    hub: State<'_, Arc<Hub>>,
    gateway: String,
    method: String,
    params: Option<Value>,
    timeout_ms: Option<u64>,
) -> Result<Value, RpcError> {
    let client = hub
        .client(&gateway)
        .ok_or_else(|| RpcError { code: "UNKNOWN_GATEWAY".into(), message: format!("no gateway {gateway}"), details: None })?;
    let params = params.unwrap_or_else(|| Value::Object(Default::default()));
    match timeout_ms {
        Some(ms) => client.call_with_timeout(&method, params, std::time::Duration::from_millis(ms)).await,
        None => client.call(&method, params).await,
    }
}

#[tauri::command]
pub fn gw_statuses(hub: State<'_, Arc<Hub>>) -> Vec<ConnStatus> {
    hub.statuses()
}

#[tauri::command]
pub fn gw_reconnect(hub: State<'_, Arc<Hub>>, gateway: Option<String>) {
    hub.reconnect(gateway.as_deref());
}

/// Use a pasted gateway token/password (kind = "token" | "password"), or clear
/// it (value = null) to go back to the config's credential.
#[tauri::command]
pub fn gw_set_credential(hub: State<'_, Arc<Hub>>, gateway: String, kind: String, value: Option<String>) -> Result<(), String> {
    hub.set_credential(&gateway, credential(Some(kind), value)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn gateway_add(
    hub: State<'_, Arc<Hub>>,
    url: String,
    name: Option<String>,
    kind: Option<String>,
    secret: Option<String>,
) -> Result<GatewayEntry, String> {
    hub.add(&url, name, credential(kind, secret))
}

/// Add a Hermes install, an ACP agent or Claude Code (kind = "hermes" | "acp" | "claudecode").
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn bot_add(
    hub: State<'_, Arc<Hub>>,
    kind: RuntimeKind,
    name: Option<String>,
    acp: Option<AcpConfig>,
    hermes: Option<HermesConfig>,
    claude: Option<ClaudeCodeConfig>,
    secret_kind: Option<String>,
    secret: Option<String>,
) -> Result<GatewayEntry, String> {
    hub.add_bot(kind, name, acp, hermes, claude, credential(secret_kind, secret))
}

/// Claude Code on this Mac: `{installed, version, plugin: {enabled, folder} | null}`.
#[tauri::command]
pub async fn claude_code_detect() -> Result<Value, ()> {
    Ok(crate::runtime::claudecode::detect().await)
}

/// Install (or repair) the littleguys plugin in Claude Code.
#[tauri::command]
pub async fn claude_code_install_plugin(hub: State<'_, Arc<Hub>>) -> Result<(), String> {
    hub.install_claude_plugin().await
}

#[tauri::command]
pub fn claude_code_configure(hub: State<'_, Arc<Hub>>, id: String, claude: ClaudeCodeConfig) -> Result<(), String> {
    hub.configure_claude(&id, claude)
}

/// Launchable agents from the ACP registry (Claude Code, Codex, Gemini CLI, …).
#[tauri::command]
pub async fn acp_registry() -> Result<Vec<crate::runtime::acp::RegistryAgent>, String> {
    crate::runtime::acp::fetch_registry().await
}

/// Hermes on this Mac (running backend) and on the tailnet (API servers).
#[tauri::command]
pub async fn hermes_detect() -> Result<Vec<crate::runtime::hermes::HermesProbe>, ()> {
    use crate::runtime::hermes;
    let mut candidates: Vec<String> = Vec::new();
    // local_backend_url() does short blocking connection checks.
    if let Ok(Some(url)) = tokio::task::spawn_blocking(hermes::local_backend_url).await {
        candidates.push(url);
    }
    candidates.push("http://127.0.0.1:9119".into());
    candidates.push("http://127.0.0.1:8642".into());
    for host in crate::gateways::tailnet_hosts().await {
        candidates.push(format!("http://{host}:8642"));
    }
    candidates.dedup();
    let probes = candidates.iter().map(|u| hermes::probe(u));
    let mut found: Vec<hermes::HermesProbe> = futures_util::future::join_all(probes).await.into_iter().flatten().collect();
    found.dedup_by(|a, b| a.url == b.url);
    Ok(found)
}

/// Connect using an OpenClaw setup code; returns the gateway's id.
#[tauri::command]
pub fn gateway_setup_code(hub: State<'_, Arc<Hub>>, code: String) -> Result<String, String> {
    hub.use_setup_code(&code)
}

#[tauri::command]
pub fn gateway_update(hub: State<'_, Arc<Hub>>, id: String, name: Option<String>, enabled: Option<bool>) -> Result<(), String> {
    hub.update(&id, name, enabled)
}

#[tauri::command]
pub fn gateway_remove(hub: State<'_, Arc<Hub>>, id: String) -> Result<(), String> {
    hub.remove(&id)
}

#[tauri::command]
pub async fn gateways_discover(hub: State<'_, Arc<Hub>>) -> Result<Vec<DiscoveredGateway>, ()> {
    Ok(hub.discover().await)
}

#[tauri::command]
pub fn get_roster(activity: State<'_, Arc<Activity>>) -> Roster {
    activity.roster()
}

#[tauri::command]
pub async fn refresh_roster(activity: State<'_, Arc<Activity>>, gateway: Option<String>) -> Result<Roster, ()> {
    match gateway {
        Some(id) => activity.refresh(&id).await,
        None => activity.refresh_all().await,
    }
    Ok(activity.roster())
}

/// Show or hide a guy (`gatewayId/agentId`) everywhere in littleguys.
#[tauri::command]
pub fn set_guy_hidden(activity: State<'_, Arc<Activity>>, uid: String, hidden: bool) -> Result<(), String> {
    activity.set_guy_hidden(&uid, hidden).map_err(|e| e.to_string())
}

/// What notifies you about a guy (`gatewayId/agentId`), or with no `uid` about everyone.
#[tauri::command]
pub fn set_notifications(activity: State<'_, Arc<Activity>>, uid: Option<String>, prefs: NotifyPrefs) -> Result<(), String> {
    activity.set_notifications(uid.as_deref(), prefs).map_err(|e| e.to_string())
}

/// Give a guy (`gatewayId/agentId`) a photo for their face, or `None` to go back
/// to the bot's; kept in littleguys only.
#[tauri::command]
pub fn set_guy_face(activity: State<'_, Arc<Activity>>, uid: String, face: Option<String>) -> Result<(), String> {
    activity.set_guy_face(&uid, face).map_err(|e| e.to_string())
}

/// Save an OpenClaw guy's picture into their workspace (gateways on this Mac only)
/// and return the path for `agents.update {avatar}`; `None` deletes it.
#[tauri::command]
pub fn set_workspace_avatar(
    activity: State<'_, Arc<Activity>>,
    gateway: String,
    agent_id: String,
    face: Option<String>,
) -> Result<Option<String>, String> {
    let workspace = activity.local_workspace(&gateway, &agent_id).ok_or("Their workspace isn't on this Mac.")?;
    match face {
        Some(face) if crate::activity::is_face(&face) => crate::avatar_file::write(&workspace, &face).map(Some),
        Some(_) => Err(anyhow::anyhow!("That picture can't be used.")),
        None => crate::avatar_file::remove(&workspace).map(|()| None),
    }
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_focused_session(activity: State<'_, Arc<Activity>>, gateway: Option<String>, key: Option<String>) {
    activity.set_focused_session(gateway, key);
}

#[tauri::command]
pub fn open_main(
    app: AppHandle,
    gateway: Option<String>,
    agent_id: Option<String>,
    session_key: Option<String>,
    view: Option<String>,
) {
    let nav = (gateway.is_some() || agent_id.is_some() || session_key.is_some() || view.is_some())
        .then_some(Navigate { gateway_id: gateway, agent_id, session_key, view });
    windows::open_main(&app, nav);
}

#[tauri::command]
pub fn hide_popover(app: AppHandle) {
    windows::hide_popover(&app);
}

#[tauri::command]
pub fn fit_popover(app: AppHandle, height: f64) {
    windows::fit_popover(&app, height);
}

/// Open a folder (an agent workspace) in Finder. Only meaningful when the
/// gateway runs on this Mac.
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).ok_or("no home dir")?,
        None => std::path::PathBuf::from(&path),
    };
    if !expanded.exists() {
        return Err(format!("{} doesn't exist on this Mac", expanded.display()));
    }
    std::process::Command::new("/usr/bin/open")
        .arg(&expanded)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn update_state(updates: State<'_, Arc<Updates>>) -> UpdateState {
    updates.state()
}

/// Check for an update now (and install it if there is one).
#[tauri::command]
pub async fn update_check(app: AppHandle, updates: State<'_, Arc<Updates>>) -> Result<UpdateState, ()> {
    Ok(updates.check(&app).await)
}

/// Relaunch into an installed update.
#[tauri::command]
pub fn update_restart(app: AppHandle) {
    app.restart();
}
