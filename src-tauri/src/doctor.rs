//! Doctor: figures out why there's no working gateway and fixes it — using the
//! local OpenClaw when present, installing it (official installer) when not.
//! Every fix is user-initiated from the UI; nothing here runs on its own.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use std::sync::Arc;

use crate::gateway::client::{GatewayClient, Phase};
use crate::hub::Hub;
use crate::openclaw::config::{self, GatewayMode};
use crate::openclaw::runtime;

/// OpenClaw's official installers: the CLI and its own Node in ~/.openclaw
/// (macOS, Linux), or the CLI via npm, bringing Node.js if needed (Windows).
#[cfg(not(windows))]
const INSTALLER_URL: &str = "https://openclaw.ai/install-cli.sh";
#[cfg(windows)]
const INSTALLER_URL: &str = "https://openclaw.ai/install.ps1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub id: &'static str,
    pub title: String,
    pub detail: String,
    /// One of: install, update, start, onboard, pairing, credential.
    pub fix: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub mode: Option<GatewayMode>,
    pub config_path: String,
    pub config_exists: bool,
    pub config_version: Option<String>,
    pub cli_path: Option<String>,
    pub cli_version: Option<String>,
    pub cli_problem: Option<String>,
    pub gateway_url: Option<String>,
    pub gateway_healthy: Option<bool>,
    pub service_installed: bool,
    pub service: runtime::ServiceState,
    pub connected: bool,
    pub issues: Vec<Issue>,
    pub log_tail: Option<String>,
}

#[tauri::command]
pub async fn doctor_check(hub: State<'_, Arc<Hub>>) -> Result<DoctorReport, String> {
    let client = openclaw(&hub)?;
    let paths = client.paths().clone();
    let status = client.status();
    let cfg = config::load_config(&paths).map_err(|e| e.to_string())?;
    let endpoint = config::resolve_endpoint(cfg.as_ref(), &paths);
    let mut report = DoctorReport {
        mode: Some(endpoint.mode),
        config_path: paths.config_path.display().to_string(),
        config_exists: cfg.is_some(),
        config_version: endpoint.config_version.clone(),
        gateway_url: Some(endpoint.url.clone()),
        connected: status.phase == Phase::Connected,
        ..Default::default()
    };

    // The CLI matters for local gateways; for remote ones it's informational.
    if let Some(cli) = runtime::find_cli(&paths.state_dir) {
        report.cli_path = Some(cli.display().to_string());
        match runtime::run(&cli, &["--version"], Duration::from_secs(20)).await {
            Ok(out) => {
                report.cli_version = runtime::parse_version(&out.stdout).or_else(|| runtime::parse_version(&out.stderr));
                if report.cli_version.is_none() {
                    report.cli_problem = Some(first_meaningful_line(&out.stderr, &out.stdout));
                }
            }
            Err(e) => report.cli_problem = Some(e.to_string()),
        }
    }

    if endpoint.mode == GatewayMode::Local {
        let port = endpoint.port.unwrap_or(config::DEFAULT_GATEWAY_PORT);
        report.gateway_healthy = Some(runtime::health_ok(port).await);
        let service = runtime::gateway_service(report.cli_path.as_deref().map(std::path::Path::new)).await;
        report.service_installed = service.installed;
        report.service = service.state;
        if report.gateway_healthy == Some(false) {
            report.log_tail = runtime::gateway_log_path().and_then(|p| runtime::tail_file(&p, 30));
        }
    }

    report.issues = diagnose(&report, &status, endpoint.credential_issue.as_deref());
    Ok(report)
}

fn diagnose(r: &DoctorReport, status: &crate::gateway::client::ConnStatus, credential_issue: Option<&str>) -> Vec<Issue> {
    let mut issues = Vec::new();
    let local = r.mode == Some(GatewayMode::Local);
    let outdated = match (&r.cli_version, &r.config_version) {
        (Some(cli), Some(cfg)) => runtime::version_lt(cli, cfg),
        _ => false,
    };
    let log_says_newer = r.log_tail.as_deref().is_some_and(|t| t.contains("is older than the config") || t.contains("newer schema"));

    if local && r.cli_path.is_none() && !r.connected {
        issues.push(Issue {
            id: "no-cli",
            title: "OpenClaw isn't installed".into(),
            detail: if cfg!(windows) {
                "littleguys runs on OpenClaw. Install it (a few minutes); it brings Node.js along if you don't have it.".into()
            } else {
                "littleguys runs on OpenClaw. Install it (about 2 minutes) — it goes in ~/.openclaw and doesn't need admin rights.".into()
            },
            fix: Some("install"),
        });
    } else if (outdated || log_says_newer) && local && !r.connected {
        issues.push(Issue {
            id: "cli-outdated",
            title: "OpenClaw needs an update".into(),
            detail: format!(
                "Your OpenClaw data was last written by {} but the installed CLI is {}, so the gateway refuses to start.",
                r.config_version.as_deref().unwrap_or("a newer version"),
                r.cli_version.as_deref().unwrap_or("older"),
            ),
            fix: Some("update"),
        });
    }

    if local && !r.config_exists && r.cli_path.is_some() {
        issues.push(Issue {
            id: "no-config",
            title: "OpenClaw hasn't been set up yet".into(),
            detail: "Pick an AI provider and littleguys will set up a local gateway for you.".into(),
            fix: Some("onboard"),
        });
    } else if local && r.config_exists && r.gateway_healthy == Some(false) && !outdated && r.cli_path.is_some() {
        let detail = match (r.service_installed, r.service.last_exit_code) {
            (false, _) => "The gateway service isn't installed.".to_string(),
            (true, Some(code)) if code != 0 => format!("The gateway service keeps exiting (code {code}). Check the log below."),
            _ => "The gateway isn't running.".to_string(),
        };
        issues.push(Issue { id: "gateway-down", title: "Gateway is not running".into(), detail, fix: Some("start") });
    }

    match status.phase {
        Phase::Pairing => issues.push(Issue {
            id: "pairing",
            title: "Approve littleguys on your gateway".into(),
            detail: match &status.request_id {
                Some(id) => format!("On the gateway host run: openclaw devices approve {id}"),
                None => "On the gateway host run: openclaw devices list, then approve littleguys.".into(),
            },
            fix: Some("pairing"),
        }),
        Phase::AuthFailed => issues.push(Issue {
            id: "auth",
            title: "The gateway rejected littleguys's credentials".into(),
            detail: credential_issue
                .map(|i| format!("littleguys couldn't read the gateway token ({i}). Paste it to continue."))
                .or_else(|| status.message.clone())
                .unwrap_or_else(|| "Paste the gateway token to continue.".into()),
            fix: Some("credential"),
        }),
        _ => {}
    }
    issues
}

fn openclaw(hub: &Hub) -> Result<GatewayClient, String> {
    hub.openclaw_client().ok_or_else(|| "the OpenClaw gateway entry is missing".to_string())
}

fn first_meaningful_line(stderr: &str, stdout: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.contains("ExperimentalWarning") && !l.starts_with("(Use `node"))
        .unwrap_or("openclaw did not respond")
        .trim_start_matches("[openclaw] ")
        .to_string()
}

fn progress(app: &AppHandle, step: &str, line: &str) {
    let _ = app.emit("lg:doctor-progress", serde_json::json!({ "step": step, "line": line }));
}

/// Install OpenClaw with the official installer (see [`INSTALLER_URL`]).
#[tauri::command]
pub async fn doctor_install_openclaw(app: AppHandle, hub: State<'_, Arc<Hub>>, version: Option<String>) -> Result<(), String> {
    let client = openclaw(&hub)?;
    progress(&app, "install", "Downloading the OpenClaw installer…");
    let script = reqwest::get(INSTALLER_URL)
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("couldn't download the installer: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let tmp = std::env::temp_dir().join(format!("littleguys-install-{}.{ext}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, script).map_err(|e| e.to_string())?;

    let version = version.as_deref().map(str::trim).filter(|v| !v.is_empty());
    #[cfg(not(windows))]
    let cmd = {
        let mut cmd = runtime::command(std::path::Path::new("/bin/bash"));
        cmd.arg(&tmp).arg("--json");
        if let Some(v) = version {
            cmd.args(["--version", v]);
        }
        cmd
    };
    // Progress comes as plain lines here (no --json); littleguys onboards on its own.
    #[cfg(windows)]
    let cmd = {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let powershell = std::path::PathBuf::from(root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let mut cmd = runtime::command(&powershell);
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(&tmp).arg("-NoOnboard");
        if let Some(v) = version {
            cmd.args(["-Tag", v]);
        }
        cmd
    };
    let app2 = app.clone();
    let result = runtime::run_streaming(cmd, Duration::from_secs(1800), move |line| {
        let label = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| {
                let event = v.get("event")?.as_str()?.to_string();
                match event.as_str() {
                    "step" => Some(format!(
                        "{} — {}",
                        v.get("name").and_then(Value::as_str).unwrap_or("step"),
                        v.get("status").and_then(Value::as_str).unwrap_or("")
                    )),
                    "error" => Some(format!("error: {}", v.get("message").and_then(Value::as_str).unwrap_or(""))),
                    "done" => Some("done".into()),
                    _ => None,
                }
            })
            .unwrap_or_else(|| line.to_string());
        progress(&app2, "install", &label);
    })
    .await;
    let _ = std::fs::remove_file(&tmp);
    let out = result.map_err(|e| e.to_string())?;
    if !out.ok {
        return Err(first_meaningful_line(&out.stderr, &out.stdout));
    }
    // The installer may have put node and openclaw somewhere new on PATH.
    crate::platform::refresh_login_path();
    client.reconnect_now();
    Ok(())
}

/// Update the existing OpenClaw install in place (`openclaw update`).
#[tauri::command]
pub async fn doctor_update_openclaw(app: AppHandle, hub: State<'_, Arc<Hub>>) -> Result<(), String> {
    let client = openclaw(&hub)?;
    let cli = runtime::find_cli(&client.paths().state_dir).ok_or("OpenClaw CLI not found")?;
    let mut cmd = runtime::command(&cli);
    cmd.args(["update", "--yes"]);
    let app2 = app.clone();
    let out = runtime::run_streaming(cmd, Duration::from_secs(1800), move |line| progress(&app2, "update", line))
        .await
        .map_err(|e| e.to_string())?;
    if !out.ok {
        return Err(first_meaningful_line(&out.stderr, &out.stdout));
    }
    client.reconnect_now();
    Ok(())
}

/// Install (if needed) and start the gateway service (a LaunchAgent, Scheduled
/// Task or systemd unit), then wait for /health.
#[tauri::command]
pub async fn doctor_start_gateway(app: AppHandle, hub: State<'_, Arc<Hub>>) -> Result<(), String> {
    let client = openclaw(&hub)?;
    let paths = client.paths().clone();
    let cli = runtime::find_cli(&paths.state_dir).ok_or("OpenClaw CLI not found")?;
    let cfg = config::load_config(&paths).map_err(|e| e.to_string())?;
    let port = config::resolve_endpoint(cfg.as_ref(), &paths).port.unwrap_or(config::DEFAULT_GATEWAY_PORT);

    let installed = runtime::gateway_service(Some(&cli)).await.installed;
    let steps: Vec<Vec<&str>> = if installed {
        vec![vec!["gateway", "restart"]]
    } else {
        vec![vec!["gateway", "install"], vec!["gateway", "start"]]
    };
    for args in steps {
        progress(&app, "start", &format!("openclaw {}", args.join(" ")));
        let mut cmd = runtime::command(&cli);
        cmd.args(&args);
        let app2 = app.clone();
        let out = runtime::run_streaming(cmd, Duration::from_secs(120), move |line| progress(&app2, "start", line))
            .await
            .map_err(|e| e.to_string())?;
        if !out.ok {
            return Err(first_meaningful_line(&out.stderr, &out.stdout));
        }
    }
    for _ in 0..40 {
        if runtime::health_ok(port).await {
            client.reconnect_now();
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("The gateway didn't come up. Check the log for details.".into())
}

/// First-run setup: `openclaw onboard --non-interactive` with the chosen
/// provider, a loopback gateway with token auth, and the gateway service.
#[tauri::command]
pub async fn doctor_onboard(
    app: AppHandle,
    hub: State<'_, Arc<Hub>>,
    auth_choice: String,
    api_key: Option<String>,
    agent_name: Option<String>,
) -> Result<(), String> {
    let client = openclaw(&hub)?;
    let cli = runtime::find_cli(&client.paths().state_dir).ok_or("OpenClaw CLI not found")?;
    let key_flag = match auth_choice.as_str() {
        "apiKey" => Some("--anthropic-api-key"),
        "openai-api-key" => Some("--openai-api-key"),
        "openrouter-api-key" => Some("--openrouter-api-key"),
        "gemini-api-key" => Some("--gemini-api-key"),
        "anthropic-cli" | "ollama" => None,
        other => return Err(format!("unsupported provider choice {other}")),
    };
    let mut args: Vec<String> = [
        "onboard",
        "--non-interactive",
        "--accept-risk",
        "--mode",
        "local",
        "--auth-choice",
        &auth_choice,
        "--secret-input-mode",
        "plaintext",
        "--gateway-bind",
        "loopback",
        "--gateway-auth",
        "token",
        "--skip-channels",
        "--skip-ui",
        "--install-daemon",
        "--json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let (Some(flag), Some(key)) = (key_flag, api_key.as_deref().map(str::trim).filter(|k| !k.is_empty())) {
        args.push(flag.into());
        args.push(key.into());
    } else if key_flag.is_some() {
        return Err("An API key is required for this provider.".into());
    }
    if let Some(name) = agent_name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        args.push("--agent-name".into());
        args.push(name);
    }
    let mut cmd = runtime::command(&cli);
    cmd.args(&args);
    let app2 = app.clone();
    let out = runtime::run_streaming(cmd, Duration::from_secs(900), move |line| {
        // Never echo a line containing the key back to the UI.
        if !line.contains("api-key") {
            progress(&app2, "onboard", line);
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    if !out.ok {
        return Err(first_meaningful_line(&out.stderr, &out.stdout));
    }
    client.reconnect_now();
    Ok(())
}

#[tauri::command]
pub fn doctor_gateway_log(lines: Option<usize>) -> Option<String> {
    runtime::gateway_log_path().and_then(|p| runtime::tail_file(&p, lines.unwrap_or(80)))
}
