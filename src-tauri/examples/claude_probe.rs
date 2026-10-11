//! Drives littleguys' Claude Code runtime and prints every event.
//!
//! `LITTLEGUYS_CLAUDE_PLUGIN=skip cargo run --example claude_probe`
//!
//! - `PROBE_DATA`      littleguys data dir: the hook socket and the plugin go here
//!   (default `$TMPDIR/littleguys-claude-probe`). Start a session that reports here with
//!   `claude --plugin-dir <PROBE_DATA>/claude-plugin/plugin`.
//! - `PROBE_APPROVALS=1` answer background sessions' permission prompts
//! - `PROBE_DECISION`  how: allow-once (default) | allow-always | deny | wait (leave them pending)
//! - `PROBE_KEY`       a thread (`agent:claude:dashboard:<sessionId>`): print its history
//! - `PROBE_SEND`      send this to `PROBE_KEY` (after `PROBE_SEND_AFTER` seconds, default 0)
//! - `PROBE_LINGER`    seconds to keep printing events (default 5)
//! - `PROBE_EVENT_CHARS` how much of each event to print (default 300)
use std::time::Duration;

use littleguys_lib::gateway::client::{Hooks, Phase};
use littleguys_lib::gateways::ClaudeCodeConfig;
use littleguys_lib::runtime::claudecode::ClaudeCodeClient;
use serde_json::{json, Value};

fn short(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s
    }
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=info")).init();
    let max: usize = std::env::var("PROBE_EVENT_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let data = std::env::var("PROBE_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("littleguys-claude-probe"));
    std::fs::create_dir_all(&data)?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let hooks = Hooks {
        status: Box::new(|s| eprintln!("[status] {:?} {} {}", s.phase, s.server_version.clone().unwrap_or_default(), s.message.clone().unwrap_or_default())),
        event: Box::new(move |name, payload, _| {
            eprintln!("[event] {name} {}", short(payload, max));
            let _ = tx.send((name.to_string(), payload.clone()));
        }),
        connected: Box::new(|| eprintln!("[connected]")),
    };
    let config = ClaudeCodeConfig { approvals: std::env::var("PROBE_APPROVALS").is_ok_and(|v| v == "1") };
    let client = ClaudeCodeClient::new("claude-probe".into(), "Claude Code".into(), config, data.clone(), hooks);
    client.start();

    for _ in 0..100 {
        if client.status().phase == Phase::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if client.status().phase != Phase::Connected {
        anyhow::bail!("didn't connect: {:?} {:?}", client.status().phase, client.status().message);
    }
    // Let the first `claude agents` poll and transcript look land.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let list = client.call("sessions.list", json!({ "limit": 20 })).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows = list["sessions"].as_array().cloned().unwrap_or_default();
    println!("== {} threads (newest first):", list["count"]);
    for row in rows.iter().take(12) {
        println!(
            "  {:<8} {:<44} {:<40} {}",
            row["status"].as_str().unwrap_or(""),
            row["key"].as_str().unwrap_or(""),
            short(&row["derivedTitle"], 38),
            short(&row["lastMessagePreview"], 60)
        );
    }

    if let Ok(key) = std::env::var("PROBE_KEY") {
        let history = client.call("chat.history", json!({ "sessionKey": key, "limit": 400 })).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let messages = history["messages"].as_array().cloned().unwrap_or_default();
        println!("== {key}: {} messages (hasMore {})", messages.len(), history["hasMore"]);
        for m in messages.iter().rev().take(6).rev() {
            println!("  {:<10} {}", m["role"].as_str().unwrap_or(""), short(&m["content"], 140));
        }
        if let Ok(text) = std::env::var("PROBE_SEND") {
            tokio::time::sleep(Duration::from_secs(env_secs("PROBE_SEND_AFTER", 0))).await;
            let sent = client.call("chat.send", json!({ "sessionKey": key, "message": text, "idempotencyKey": "probe" })).await;
            println!("== chat.send → {sent:?}");
        }
        // A second message, sent while the events below are being watched (e.g. mid-turn).
        if let Ok(text) = std::env::var("PROBE_SEND2") {
            let (client, key) = (client.clone(), key.clone());
            let after = env_secs("PROBE_SEND2_AFTER", 10);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(after)).await;
                let sent = client.call("chat.send", json!({ "sessionKey": key, "message": text, "idempotencyKey": "probe2" })).await;
                println!("== chat.send (2nd) → {sent:?}");
            });
        }
    }

    let decision = std::env::var("PROBE_DECISION").unwrap_or_else(|_| "allow-once".into());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(env_secs("PROBE_LINGER", 5));
    loop {
        let next = tokio::time::timeout_at(deadline, rx.recv()).await;
        let Ok(Some((name, payload))) = next else { break };
        if name == "exec.approval.requested" && decision != "wait" {
            let id = payload["id"].as_str().unwrap_or_default().to_string();
            println!("== approving {id} with {decision}: {}", payload["presentation"]["commandText"]);
            let resolved = client.call("exec.approval.resolve", json!({ "id": id, "decision": decision })).await;
            println!("== exec.approval.resolve → {resolved:?}");
        }
    }
    client.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}
