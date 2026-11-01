//! Finding and driving the local `openclaw` CLI. Commands run with
//! [`login_path`] (where nvm, brew, npm and the official installer put node
//! and openclaw).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub use crate::platform::login_path;

/// Look for the CLI: the official installer's prefix first, then PATH.
pub fn find_cli(state_dir: &Path) -> Option<PathBuf> {
    crate::platform::find_in(std::iter::once(state_dir.join("bin")).chain(crate::platform::path_dirs()), "openclaw")
}

pub fn command(program: &Path) -> Command {
    let mut cmd = crate::platform::command(program);
    cmd.env("PATH", login_path())
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

pub struct CliOutput {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run(program: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<CliOutput> {
    let child = command(program).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("`openclaw {}` timed out", args.join(" ")))??;
    Ok(CliOutput {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run a command, handing each output line (stdout and stderr) to `on_line`.
pub async fn run_streaming(
    mut cmd: Command,
    timeout: Duration,
    on_line: impl Fn(&str) + Send + Sync + 'static,
) -> anyhow::Result<CliOutput> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let on_line = std::sync::Arc::new(on_line);
    let (out_cb, err_cb) = (on_line.clone(), on_line.clone());
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut all = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            out_cb(&line);
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut all = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            err_cb(&line);
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("timed out after {}s", timeout.as_secs());
        }
    };
    Ok(CliOutput {
        ok: status.success(),
        stdout: out_task.await.unwrap_or_default(),
        stderr: err_task.await.unwrap_or_default(),
    })
}

/// "OpenClaw 2026.9.4 (3a9d69d)" → "2026.9.4"
pub fn parse_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("OpenClaw ")?;
        let version = rest.split_whitespace().next()?;
        version.chars().next()?.is_ascii_digit().then(|| version.to_string())
    })
}

/// Compare OpenClaw's calendar versions (2026.9.4, 2026.9.4-beta.1).
pub fn version_lt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let (core, pre) = match v.split_once('-') {
            Some((core, _)) => (core, true),
            None => (v, false),
        };
        (core.split('.').map(|p| p.parse().unwrap_or(0)).collect(), pre)
    }
    let (pa, pre_a) = parts(a.trim());
    let (pb, pre_b) = parts(b.trim());
    match pa.cmp(&pb) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => pre_a && !pre_b,
    }
}

#[cfg(target_os = "macos")]
pub fn service_label() -> String {
    match std::env::var("OPENCLAW_PROFILE") {
        Ok(p) if !p.trim().is_empty() => format!("ai.openclaw.{}", p.trim().to_lowercase()),
        _ => "ai.openclaw.gateway".into(),
    }
}

#[cfg(target_os = "macos")]
fn service_plist() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{}.plist", service_label())))
}

/// Where the gateway service writes its log. macOS: the LaunchAgent's log.
/// Elsewhere: the file `openclaw gateway status` last named.
pub fn gateway_log_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let name = match std::env::var("OPENCLAW_PROFILE") {
            Ok(p) if !p.trim().is_empty() => format!("gateway-{}.log", p.trim()),
            _ => "gateway.log".into(),
        };
        dirs::home_dir().map(|h| h.join("Library/Logs/openclaw").join(name))
    }
    #[cfg(not(target_os = "macos"))]
    LOG_FILE.lock().unwrap().clone()
}

#[cfg(not(target_os = "macos"))]
static LOG_FILE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub fn tail_file(path: &Path, lines: usize) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let start = raw.len().saturating_sub(64 * 1024);
    let text = String::from_utf8_lossy(&raw[start..]);
    let all: Vec<&str> = text.lines().collect();
    Some(all[all.len().saturating_sub(lines)..].join("\n"))
}

#[derive(Debug, Clone, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceState {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u64>,
    pub last_exit_code: Option<i64>,
}

/// The gateway service as the OS runs it: a LaunchAgent on macOS, a Scheduled
/// Task on Windows, a systemd user unit on Linux.
#[derive(Debug, Clone, Default)]
pub struct GatewayService {
    pub installed: bool,
    pub state: ServiceState,
}

/// macOS asks launchd directly (quick); elsewhere `cli` reports it (`openclaw
/// gateway status --json`, a few seconds), which also says where the log is.
pub async fn gateway_service(cli: Option<&Path>) -> GatewayService {
    #[cfg(target_os = "macos")]
    {
        let _ = cli;
        GatewayService { installed: service_plist().is_some_and(|p| p.exists()), state: launchd_state().await }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let Some(cli) = cli else { return GatewayService::default() };
        let Ok(out) = run(cli, &["gateway", "status", "--json"], Duration::from_secs(45)).await else {
            return GatewayService::default();
        };
        let Some(status) = out.stdout.find('{').and_then(|i| serde_json::from_str::<serde_json::Value>(&out.stdout[i..]).ok()) else {
            return GatewayService::default();
        };
        let (service, log) = parse_gateway_status(&status);
        if log.is_some() {
            *LOG_FILE.lock().unwrap() = log;
        }
        service
    }
}

/// What `openclaw gateway status --json` says about the service, and its log file.
#[cfg(any(not(target_os = "macos"), test))]
fn parse_gateway_status(status: &serde_json::Value) -> (GatewayService, Option<PathBuf>) {
    use serde_json::Value;
    let service = status.get("service");
    let installed = service.and_then(|s| s.get("loaded")).and_then(Value::as_bool).unwrap_or(false);
    let runtime = service.and_then(|s| s.get("runtime"));
    let pid = runtime.and_then(|r| r.get("pid")).and_then(Value::as_u64);
    let running = runtime.and_then(|r| r.get("status")).and_then(Value::as_str) == Some("running") || pid.is_some();
    let last_exit_code = runtime.and_then(|r| ["lastExitStatus", "lastExitCode", "lastRunResult"].iter().find_map(|k| r.get(*k)?.as_i64()));
    let log = status.get("logFile").and_then(Value::as_str).filter(|p| !p.is_empty()).map(PathBuf::from);
    (GatewayService { installed, state: ServiceState { loaded: installed, running, pid, last_exit_code } }, log)
}

/// `launchctl print gui/<uid>/<label>`
#[cfg(target_os = "macos")]
async fn launchd_state() -> ServiceState {
    let uid = unsafe_getuid();
    let target = format!("gui/{uid}/{}", service_label());
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("/bin/launchctl").args(["print", &target]).output(),
    )
    .await
    else {
        return ServiceState::default();
    };
    if !out.status.success() {
        return ServiceState::default();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines().find_map(|l| l.trim().strip_prefix(name).and_then(|r| r.trim().strip_prefix('=')).map(|v| v.trim().to_string()))
    };
    let pid = field("pid").and_then(|p| p.parse().ok());
    ServiceState {
        loaded: true,
        running: field("state").as_deref() == Some("running") || pid.is_some(),
        pid,
        last_exit_code: field("last exit code").and_then(|c| c.parse().ok()),
    }
}

#[cfg(target_os = "macos")]
fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

pub async fn health_ok(port: u16) -> bool {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build();
    let Ok(client) = client else { return false };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/health")).send().await,
        Ok(resp) if resp.status().is_success()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_version() {
        assert_eq!(parse_version("OpenClaw 2026.9.4 (3a9d69d)").as_deref(), Some("2026.9.4"));
        assert_eq!(parse_version("(node:1) warning\nOpenClaw 2026.10.1-beta.2 (abc)").as_deref(), Some("2026.10.1-beta.2"));
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn reads_gateway_status() {
        let status = serde_json::json!({
            "logFile": "C:\\Users\\pat\\AppData\\Local\\Temp\\openclaw\\openclaw-2026-11-01.log",
            "service": { "label": "Scheduled Task", "loaded": true, "runtime": { "status": "stopped", "state": "active" } }
        });
        let (service, log) = parse_gateway_status(&status);
        assert!(service.installed && service.state.loaded && !service.state.running);
        assert!(log.is_some_and(|p| p.to_string_lossy().ends_with("openclaw-2026-11-01.log")));
        let (service, log) = parse_gateway_status(&serde_json::json!({ "service": { "loaded": true, "runtime": { "status": "running", "pid": 42 } } }));
        assert!(service.state.running);
        assert_eq!(service.state.pid, Some(42));
        assert_eq!(log, None);
        assert!(!parse_gateway_status(&serde_json::json!({})).0.installed);
    }

    #[test]
    fn compares_calendar_versions() {
        assert!(version_lt("2026.9.4", "2026.9.6"));
        assert!(version_lt("2026.9.6", "2026.10.0"));
        assert!(!version_lt("2026.9.6", "2026.9.6"));
        assert!(version_lt("2026.9.6-beta.1", "2026.9.6"));
        assert!(!version_lt("2026.10.0", "2026.9.30"));
    }
}
