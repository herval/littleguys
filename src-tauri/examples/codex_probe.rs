//! Drives littleguys' Codex runtime and prints every event.
//!
//! `LITTLEGUYS_CODEX_ONLY=<a test folder> cargo run --example codex_probe`
//!
//! Set `LITTLEGUYS_CODEX_ONLY`: it limits everything, sending included, to sessions in
//! that folder. `PROBE_SEND` and `PROBE_NEW` refuse to run without it.
//!
//! - `PROBE_DATA`      littleguys data dir (default `$TMPDIR/littleguys-codex-probe`)
//! - `PROBE_DECISION`  how to answer prompts: allow-once (default) | allow-always | deny | wait
//! - `PROBE_THREAD`    a session id: print its history
//! - `PROBE_SEND`      send this to `PROBE_THREAD` (after `PROBE_SEND_AFTER` seconds, default 0)
//! - `PROBE_SEND2`     send this too, `PROBE_SEND2_AFTER` seconds later (default 10)
//! - `PROBE_ABORT_AFTER` stop `PROBE_THREAD`'s turn after this many seconds
//! - `PROBE_PATCH`     JSON to `sessions.patch` onto `PROBE_THREAD`, one patch per array item
//! - `PROBE_DELETE=1`  then `sessions.delete` it
//! - `PROBE_OPEN=<n>`  with `PROBE_STATS`: open the oldest n past sessions and count them too
//! - `PROBE_NEW`       start a new thread in the folder `PROBE_NEW_CWD` with this message
//! - `PROBE_CRON`      run a one-off routine with this message, in the folder `PROBE_NEW_CWD`
//! - `PROBE_STATS=1`   counts only (no content): threads, past sessions, messages by role
//! - `PROBE_LINGER`    seconds to keep printing events (default 5)
//! - `PROBE_EVENT_CHARS` how much of each event to print (default 300)
use std::collections::BTreeMap;
use std::time::Duration;

use littleguys_lib::gateway::client::{Hooks, Phase};
use littleguys_lib::gateways::CodexConfig;
use littleguys_lib::runtime::codex::CodexClient;
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

fn fail(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=info")).init();
    let stats = std::env::var("PROBE_STATS").is_ok_and(|v| v == "1");
    let scoped = std::env::var("LITTLEGUYS_CODEX_ONLY").is_ok_and(|v| !v.is_empty());
    if !scoped && (std::env::var("PROBE_SEND").is_ok() || std::env::var("PROBE_NEW").is_ok() || std::env::var("PROBE_CRON").is_ok()) {
        anyhow::bail!("set LITTLEGUYS_CODEX_ONLY to a test folder before sending anything");
    }
    let max: usize = std::env::var("PROBE_EVENT_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let data = std::env::var("PROBE_DATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("littleguys-codex-probe"));
    std::fs::create_dir_all(&data)?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let hooks = Hooks {
        status: Box::new(|s| eprintln!("[status] {:?} {} {}", s.phase, s.server_version.clone().unwrap_or_default(), s.message.clone().unwrap_or_default())),
        event: Box::new(move |name, payload, _| {
            if !stats {
                eprintln!("[event] {name} {}", short(payload, max));
            }
            let _ = tx.send((name.to_string(), payload.clone()));
        }),
        connected: Box::new(|| eprintln!("[connected]")),
    };
    let client = CodexClient::new("codex-probe".into(), "Codex".into(), CodexConfig::default(), data.clone(), hooks);
    client.start();

    for _ in 0..300 {
        if client.status().phase == Phase::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if client.status().phase != Phase::Connected {
        anyhow::bail!("didn't connect: {:?} {:?}", client.status().phase, client.status().message);
    }
    // Let the first scan and reads land.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let list = client.call("sessions.list", json!({ "limit": 500, "archived": "all" })).await.map_err(fail)?;
    let rows = list["sessions"].as_array().cloned().unwrap_or_default();
    println!("== {} threads ({} archived, {} running):", rows.len(), rows.iter().filter(|r| r["archived"] == true).count(), rows.iter().filter(|r| r["status"] == "running").count());
    if stats {
        let past = client.call("sessions.past", json!({})).await.map_err(fail)?;
        let past = past["sessions"].as_array().cloned().unwrap_or_default();
        println!("== {} past sessions, {} with a title", past.len(), past.iter().filter(|p| p["title"].is_string()).count());
        let open: usize = std::env::var("PROBE_OPEN").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let oldest: Vec<Value> = past.iter().rev().take(open).cloned().collect();
        for row in &oldest {
            let params = json!({ "sessionId": row["sessionId"], "cwd": row["cwd"], "updatedAt": row["updatedAt"] });
            client.call("sessions.open", params).await.map_err(fail)?;
        }
        if open > 0 {
            tokio::time::sleep(Duration::from_secs(7)).await;
            let list = client.call("sessions.list", json!({ "limit": 500, "archived": "all" })).await.map_err(fail)?;
            let shown = list["sessions"].as_array().cloned().unwrap_or_default();
            let opened = oldest.iter().filter(|p| shown.iter().any(|r| r["sessionId"] == p["sessionId"] || r["key"] == p["key"])).count();
            let mut read = 0;
            let mut messages = 0;
            for row in &oldest {
                let history = client.call("chat.history", json!({ "sessionKey": row["key"], "limit": 1000 })).await.map_err(fail)?;
                let n = history["messages"].as_array().map_or(0, Vec::len);
                read += usize::from(n > 0);
                messages += n;
            }
            println!("== opened {open} oldest past sessions: {opened} listed, {read} with history, {messages} messages");
        }
        let mut roles: BTreeMap<String, usize> = BTreeMap::new();
        let mut empty = 0;
        for row in rows.iter().chain(past.iter().take(60)) {
            let history = client.call("chat.history", json!({ "sessionKey": row["key"], "limit": 1000 })).await.map_err(fail)?;
            let messages = history["messages"].as_array().cloned().unwrap_or_default();
            empty += usize::from(messages.is_empty());
            for m in &messages {
                let block = m["content"][0]["type"].as_str().unwrap_or("?");
                *roles.entry(format!("{}/{block}", m["role"].as_str().unwrap_or("?"))).or_default() += 1;
            }
        }
        println!("== messages: {roles:?}; {empty} threads with none");
        client.stop();
        return Ok(());
    }
    for row in rows.iter().take(12) {
        println!(
            "  {:<8} {:<54} {:<30} {}",
            row["status"].as_str().unwrap_or(""),
            row["key"].as_str().unwrap_or(""),
            short(&row["derivedTitle"], 28),
            short(&row["lastMessagePreview"], 50)
        );
    }

    if let Ok(message) = std::env::var("PROBE_CRON") {
        let cwd = std::env::var("PROBE_NEW_CWD").unwrap_or_default();
        let job = client
            .call(
                "cron.add",
                json!({
                    "name": "Probe routine",
                    "agentId": "codex",
                    "schedule": { "kind": "every", "everyMs": 86_400_000 },
                    "payload": { "kind": "agentTurn", "message": message, "cwd": cwd },
                    "sessionTarget": "isolated",
                }),
            )
            .await
            .map_err(fail)?;
        let id = job["id"].as_str().unwrap_or_default().to_string();
        println!("== cron.add {}", short(&job, max));
        println!("== cron.run {:?}", client.call("cron.run", json!({ "id": id })).await);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while let Ok(Some((name, payload))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if name == "cron" && payload["action"] == "finished" {
                println!("== routine finished: {}", short(&payload, max));
                break;
            }
        }
        println!("== cron.runs {:?}", client.call("cron.runs", json!({ "id": id })).await.map(|v| short(&v, max)));
        println!("== cron.remove {:?}", client.call("cron.remove", json!({ "id": id })).await);
    }

    if let Ok(new) = std::env::var("PROBE_NEW") {
        let created = client.call("sessions.create", json!({})).await.map_err(fail)?;
        let key = created["key"].as_str().unwrap_or_default().to_string();
        let cwd = std::env::var("PROBE_NEW_CWD").unwrap_or_default();
        let sent = client.call("chat.send", json!({ "sessionKey": key, "message": new, "cwd": cwd, "idempotencyKey": "probe" })).await;
        println!("== new thread {key}: chat.send → {sent:?}");
    }

    if let Ok(id) = std::env::var("PROBE_THREAD") {
        let key = format!("agent:codex:dashboard:{id}");
        let history = client.call("chat.history", json!({ "sessionKey": key, "limit": 400 })).await.map_err(fail)?;
        let messages = history["messages"].as_array().cloned().unwrap_or_default();
        println!("== {key}: {} messages (hasMore {})", messages.len(), history["hasMore"]);
        for m in messages.iter().rev().take(8).rev() {
            println!("  {:<10} {}", m["role"].as_str().unwrap_or(""), short(&m["content"], 140));
        }
        if let Ok(patches) = std::env::var("PROBE_PATCH") {
            let patches: Vec<Value> = serde_json::from_str(&patches)?;
            for patch in patches {
                let mut params = patch.clone();
                params["key"] = json!(key);
                let row = client.call("sessions.patch", params).await.map_err(fail)?;
                println!("== patch {patch} → {} archived={} pinned={}", row["label"], row["archived"], row["pinned"]);
                let list = client.call("sessions.list", json!({ "limit": 500 })).await.map_err(fail)?;
                let listed = list["sessions"].as_array().is_some_and(|r| r.iter().any(|r| r["sessionId"] == json!(id)));
                println!("   in the default list: {listed}");
            }
        }
        if std::env::var("PROBE_DELETE").is_ok_and(|v| v == "1") {
            let gone = client.call("sessions.delete", json!({ "key": key })).await.map_err(fail)?;
            let list = client.call("sessions.list", json!({ "limit": 500, "archived": "all" })).await.map_err(fail)?;
            let listed = list["sessions"].as_array().is_some_and(|r| r.iter().any(|r| r["sessionId"] == json!(id)));
            println!("== delete → {gone}; still listed: {listed}");
        }
        if let Ok(text) = std::env::var("PROBE_SEND") {
            tokio::time::sleep(Duration::from_secs(env_secs("PROBE_SEND_AFTER", 0))).await;
            let sent = client.call("chat.send", json!({ "sessionKey": key, "message": text, "idempotencyKey": "probe" })).await;
            println!("== chat.send → {sent:?}");
        }
        if let Ok(text) = std::env::var("PROBE_SEND2") {
            let (client, key) = (client.clone(), key.clone());
            let after = env_secs("PROBE_SEND2_AFTER", 10);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(after)).await;
                let sent = client.call("chat.send", json!({ "sessionKey": key, "message": text, "idempotencyKey": "probe2" })).await;
                println!("== chat.send (2nd) → {sent:?}");
            });
        }
        if std::env::var("PROBE_ABORT_AFTER").is_ok() {
            let (client, key) = (client.clone(), key.clone());
            let after = env_secs("PROBE_ABORT_AFTER", 5);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(after)).await;
                println!("== chat.abort → {:?}", client.call("chat.abort", json!({ "sessionKey": key })).await);
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
            println!("== answering {id} with {decision}: {}", payload["presentation"]["commandText"]);
            let resolved = client.call("exec.approval.resolve", json!({ "id": id, "decision": decision })).await;
            println!("== exec.approval.resolve → {resolved:?}");
        }
    }
    let list = client.call("sessions.list", json!({ "limit": 20, "archived": "all" })).await.map_err(fail)?;
    println!("== at the end:");
    for row in list["sessions"].as_array().cloned().unwrap_or_default().iter().take(8) {
        println!(
            "  {:<8} archived={:<5} kept-key={:<56} {:<28} {}",
            row["status"].as_str().unwrap_or(""),
            row["archived"],
            row["key"].as_str().unwrap_or(""),
            short(&row["derivedTitle"], 26),
            short(&row["lastMessagePreview"], 50)
        );
    }
    client.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}
