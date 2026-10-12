//! A bridge with pretend bots, for trying littleguys for iPhone (or
//! `bridge_probe`) without the app: an OpenClaw gateway with Jefe and Fox (who
//! streams a canned reply; Jefe waits on an approval), Hermes and Claude Code.
//! Opens a pairing on start and prints the link.
//!
//! `cargo run --example bridge_demo -- [--data <dir>] [--port <n>] [--no-bonjour] [--relay <url>] [--push-every <secs>]`
//! (data defaults to /tmp/littleguys-bridge-demo, so pairings survive restarts)
//!
//! Push: type `push` (a reply from Fox), `approval` or `resolved` and Enter to
//! send one to every registered phone that isn't in the foreground, or pass
//! `--push-every <secs>`. The relay's answer is printed for each phone.
use std::sync::Arc;
use std::time::Duration;

use littleguys_lib::bridge::demo::{Demo, DemoHost};
use littleguys_lib::bridge::{Bridge, BridgeConfig, Notice, NoticeKind};
use tokio::io::AsyncBufReadExt;

fn notice(what: &str) -> Option<Notice> {
    let reply = Notice {
        kind: NoticeKind::Reply,
        title: "🦊 Fox".into(),
        body: "Push test from the bridge demo".into(),
        gateway: "openclaw".into(),
        session_key: Some("agent:fox:main".into()),
        approval_id: None,
    };
    match what {
        "push" | "p" | "reply" => Some(reply),
        "approval" | "a" => Some(Notice {
            kind: NoticeKind::Approval,
            title: "🤠 Jefe needs your OK".into(),
            body: "Wants to run: rm -rf ~/Downloads/old-builds".into(),
            session_key: Some("agent:main:main".into()),
            approval_id: Some("appr-1".into()),
            ..reply
        }),
        "resolved" | "r" => Some(Notice {
            kind: NoticeKind::ApprovalResolved,
            title: String::new(),
            body: String::new(),
            session_key: None,
            approval_id: Some("appr-1".into()),
            ..reply
        }),
        _ => None,
    }
}

async fn push(bridge: &Bridge, notice: &Notice) {
    let results = bridge.push(notice).await;
    if results.is_empty() {
        println!("[push] no registered phone outside the foreground");
    }
    for (phone, result) in results {
        match result {
            Ok(answer) => println!("[push] {phone}: {answer}"),
            Err(e) => println!("[push] {phone}: failed: {e}"),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("littleguys_lib=info")).init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let data = flag("--data").unwrap_or_else(|| "/tmp/littleguys-bridge-demo".into());

    let mut config = BridgeConfig::new(data.clone().into(), "Bridge Demo".into(), env!("CARGO_PKG_VERSION").into());
    config.loopback_first = true;
    config.bonjour = !args.iter().any(|a| a == "--no-bonjour");
    if let Some(port) = flag("--port") {
        config.port = port.parse()?;
    }
    if let Some(relay) = flag("--relay") {
        config.relay_url = Some(relay).filter(|r| !r.is_empty());
    }
    let bridge = Bridge::new(config, Arc::new(Demo(DemoHost::new().leak())), tokio::runtime::Handle::current())?;
    bridge.on_state(|state| {
        let phones: Vec<String> = state
            .phones
            .iter()
            .map(|p| {
                format!("{}{}{}", p.name, if p.connected { " (connected)" } else { "" }, if p.push { " (push)" } else { "" })
            })
            .collect();
        eprintln!("[state] listening={} port={:?} phones={phones:?}", state.listening, state.port);
    });
    bridge.start();
    let pairing = bridge.pair_start().await?;
    println!("\nPair with this link (valid 10 minutes, once):\n\n{}\n", pairing.uri);
    println!(
        "Data dir: {data}  ·  macId {}  ·  relay {}",
        bridge.mac_id(),
        bridge.state().relay_url.unwrap_or_else(|| "off".into())
    );
    println!("Type push, approval or resolved and Enter to send a test push. Ctrl-C to stop.");

    if let Some(secs) = flag("--push-every") {
        let every = Duration::from_secs(secs.parse()?);
        let b = bridge.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                push(&b, &notice("push").unwrap()).await;
            }
        });
    }
    let b = bridge.clone();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match notice(line.trim()) {
                Some(n) => push(&b, &n).await,
                None if line.trim().is_empty() => {}
                None => println!("[push] try: push, approval, resolved"),
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    bridge.shutdown();
    Ok(())
}
