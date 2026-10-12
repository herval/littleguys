//! A pretend phone for the bridge (docs/BRIDGE-PROTOCOL.md). It keeps its key
//! and the Mac it paired with in a file, so `pair` once, then call away.
//!
//! ```text
//! cargo run --example bridge_probe -- pair '<littleguys://pair?… link>'
//! cargo run --example bridge_probe -- bots
//! cargo run --example bridge_probe -- call <gateway> <method> ['<json params>']
//! cargo run --example bridge_probe -- events
//! cargo run --example bridge_probe -- media <gateway> <source>
//! cargo run --example bridge_probe -- forget
//! cargo run --example bridge_probe -- raw <bridge method> ['<json params>']   # e.g. push.register
//! ```
//! PROBE_FILE=<path> picks the file (default /tmp/littleguys-bridge-probe.json);
//! PROBE_ADDR=<host:port> overrides the address from the link.
use littleguys_lib::bridge::phone::{connect, Invite, PhoneKeys, PhoneSession};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize)]
struct Saved {
    keys: PhoneKeys,
    /// The pairing link (its secret is spent once paired).
    link: String,
}

fn file() -> String {
    std::env::var("PROBE_FILE").unwrap_or_else(|_| "/tmp/littleguys-bridge-probe.json".into())
}

fn client() -> Value {
    json!({ "name": "bridge_probe", "model": "probe", "version": env!("CARGO_PKG_VERSION"), "platform": "probe" })
}

/// Try the link's addresses in order (or PROBE_ADDR).
async fn open(invite: &Invite, keys: &PhoneKeys, pair: bool) -> anyhow::Result<PhoneSession> {
    let addrs = match std::env::var("PROBE_ADDR") {
        Ok(a) => vec![a],
        Err(_) => invite.addrs.clone(),
    };
    let mut last = None;
    for addr in &addrs {
        match connect(addr, invite, keys, client(), pair).await {
            Ok(session) => {
                eprintln!("[connected to {addr}]");
                return Ok(session);
            }
            Err(e) => {
                eprintln!("[{addr}: {e}]");
                last = Some(e.to_string());
            }
        }
    }
    anyhow::bail!(last.unwrap_or_else(|| "the link has no addresses".into()))
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();

    if arg(0) == "pair" {
        let link = arg(1);
        let invite = Invite::parse(&link).map_err(anyhow::Error::msg)?;
        let keys = PhoneKeys::generate();
        let session = open(&invite, &keys, true).await?;
        std::fs::write(file(), serde_json::to_vec_pretty(&Saved { keys, link })?)?;
        eprintln!("[paired with {}; saved to {}]", session.hello["mac"]["name"], file());
        print(&session.hello);
        return Ok(());
    }

    let saved: Saved = serde_json::from_slice(&std::fs::read(file()).map_err(|_| anyhow::anyhow!("pair first: no {}", file()))?)?;
    let invite = Invite::parse(&saved.link).map_err(anyhow::Error::msg)?;
    let mut session = open(&invite, &saved.keys, false).await?;
    let result = match arg(0).as_str() {
        "bots" | "" => session.request("bots.list", json!({})).await,
        "call" => {
            let params: Value = if args.len() > 3 { serde_json::from_str(&arg(3))? } else { json!({}) };
            session.request("bot.call", json!({ "gateway": arg(1), "method": arg(2), "params": params })).await
        }
        "media" => match session.media(&arg(1), &arg(2)).await {
            Ok((answer, bytes)) => {
                let dir = std::path::Path::new(&file()).parent().map(|d| d.to_path_buf()).unwrap_or_default();
                let path = dir.join(format!("bridge-probe-media-{}", arg(2).rsplit('/').next().unwrap_or("file")));
                if answer["available"] == true {
                    std::fs::write(&path, &bytes)?;
                    eprintln!("[{} bytes → {}]", bytes.len(), path.display());
                }
                Ok(answer)
            }
            Err(e) => Err(e),
        },
        "forget" => session.request("phone.forget", json!({})).await,
        "raw" => {
            let params: Value = if args.len() > 2 { serde_json::from_str(&arg(2))? } else { json!({}) };
            session.request(&arg(1), params).await
        }
        "events" => {
            while let Some(frame) = session.next_event().await {
                let s = frame.to_string();
                println!("{}", &s[..s.len().min(400)]);
            }
            eprintln!("[closed: {:?}]", session.close_code());
            return Ok(());
        }
        other => anyhow::bail!("unknown command {other}"),
    };
    match result {
        Ok(v) => print(&v),
        Err(e) => {
            print(&e);
            std::process::exit(1);
        }
    }
    Ok(())
}
