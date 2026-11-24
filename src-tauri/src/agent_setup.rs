//! Claude Code and Codex for people who don't have them yet. "Add a bot" installs
//! them with the vendor's official installer (into your home folder, no admin rights),
//! then signs in with the CLI's own browser sign-in. Every step starts from a click.
//!
//! Progress goes out as `lg:agent-setup` events, `{agent, step: "install" | "signin", line}`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::doctor::{first_meaningful_line, run_installer};
use crate::openclaw::runtime as cli;
use crate::platform::COMPUTER;
use crate::runtime::{claudecode, codex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn key(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    fn find(self) -> Option<PathBuf> {
        match self {
            Agent::Claude => claudecode::find_claude(),
            Agent::Codex => codex::find_codex(),
        }
    }

    /// The official installer, and the shell it's written for (PowerShell on Windows).
    /// Both put the CLI in ~/.local/bin (Codex on Windows: %LOCALAPPDATA%\Programs\OpenAI\Codex\bin
    /// and its own folder under ~/.codex), where `find` looks.
    fn installer(self) -> (&'static str, &'static str) {
        match self {
            #[cfg(not(windows))]
            Agent::Claude => ("https://claude.ai/install.sh", "/bin/bash"),
            #[cfg(windows)]
            Agent::Claude => ("https://claude.ai/install.ps1", ""),
            #[cfg(not(windows))]
            Agent::Codex => ("https://chatgpt.com/codex/install.sh", "/bin/sh"),
            #[cfg(windows)]
            Agent::Codex => ("https://chatgpt.com/codex/install.ps1", ""),
        }
    }

    /// The CLI's sign-in: it opens the browser and returns once you're signed in.
    fn sign_in_args(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["auth", "login"],
            Agent::Codex => &["login"],
        }
    }
}

fn progress(app: &AppHandle, agent: Agent, step: &str, line: &str) {
    let _ = app.emit("lg:agent-setup", serde_json::json!({ "agent": agent.key(), "step": step, "line": line }));
}

/// Install Claude Code or Codex with the vendor's installer.
#[tauri::command]
pub async fn agent_install(app: AppHandle, agent: Agent) -> Result<(), String> {
    let (url, shell) = agent.installer();
    progress(&app, agent, "install", &format!("Downloading the {} installer…", agent.name()));
    // Codex's installer offers to start Codex when it's done; littleguys only installs it.
    let envs: &[(&str, &str)] = match agent {
        Agent::Codex => &[("CODEX_NON_INTERACTIVE", "1")],
        Agent::Claude => &[],
    };
    let app2 = app.clone();
    run_installer(url, shell, &[], envs, move |line| progress(&app2, agent, "install", line)).await?;
    if agent.find().is_none() {
        return Err(format!("The installer finished, but littleguys can't find {} yet. Restart littleguys and try again.", agent.name()));
    }
    Ok(())
}

/// Sign-ins under way, one per agent: starting another, or cancelling, stops it (Codex's
/// waits for the browser on a fixed local port, so two can't run at once).
static SIGN_INS: LazyLock<Mutex<HashMap<Agent, tokio::task::AbortHandle>>> = LazyLock::new(Default::default);

/// Sign in to Claude Code or Codex: the CLI opens the browser, and this returns once
/// you've signed in there (or after 15 minutes).
#[tauri::command]
pub async fn agent_sign_in(app: AppHandle, agent: Agent) -> Result<(), String> {
    let path = agent.find().ok_or_else(|| format!("{} isn't installed on this {COMPUTER}.", agent.name()))?;
    let mut cmd = cli::command(&path);
    cmd.args(agent.sign_in_args());
    let app2 = app.clone();
    let task = tokio::spawn(cli::run_streaming(cmd, Duration::from_secs(900), move |line| progress(&app2, agent, "signin", line)));
    let id = task.id();
    if let Some(earlier) = SIGN_INS.lock().unwrap().insert(agent, task.abort_handle()) {
        earlier.abort();
    }
    let result = task.await;
    {
        let mut running = SIGN_INS.lock().unwrap();
        if running.get(&agent).is_some_and(|h| h.id() == id) {
            running.remove(&agent);
        }
    }
    match result {
        Err(e) if e.is_cancelled() => Err("Sign-in cancelled.".into()),
        Err(e) => Err(e.to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if !out.ok => Err(first_meaningful_line(&out.stderr, &out.stdout)),
        Ok(Ok(_)) => Ok(()),
    }
}

/// Stop a sign-in that's waiting on the browser (the dialog closed, or you gave up).
#[tauri::command]
pub fn agent_sign_in_cancel(agent: Agent) {
    if let Some(task) = SIGN_INS.lock().unwrap().remove(&agent) {
        task.abort();
    }
}
