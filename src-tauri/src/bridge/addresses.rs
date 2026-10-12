//! Where a phone can reach this Mac: private LAN addresses, then its tailnet
//! name and address (from the Tailscale CLI).

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressKind {
    Lan,
    Tailnet,
    Loopback,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    pub host: String,
    pub port: u16,
    pub kind: AddressKind,
}

impl Address {
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// In the order phones should try them.
pub async fn discover(port: u16, loopback_first: bool) -> Vec<Address> {
    let mut out = Vec::new();
    if loopback_first {
        out.push(Address { host: "127.0.0.1".into(), port, kind: AddressKind::Loopback });
    }
    for ip in lan_ipv4() {
        out.push(Address { host: ip.to_string(), port, kind: AddressKind::Lan });
    }
    if let Some((name, ip)) = tailnet_self().await {
        if let Some(name) = name {
            out.push(Address { host: name, port, kind: AddressKind::Tailnet });
        }
        if let Some(ip) = ip {
            out.push(Address { host: ip.to_string(), port, kind: AddressKind::Tailnet });
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(a.host.clone()));
    out
}

/// Private IPv4 addresses of interfaces that are up, Wi-Fi/Ethernet (`en*`) first.
pub fn lan_ipv4() -> Vec<Ipv4Addr> {
    let Ok(mut ifaces) = if_addrs::get_if_addrs() else { return Vec::new() };
    ifaces.sort_by_key(|i| (!i.name.starts_with("en"), i.name.clone()));
    let mut out = Vec::new();
    for iface in ifaces {
        if let IpAddr::V4(ip) = iface.ip() {
            if iface.is_oper_up() && ip.is_private() && !is_tailnet(IpAddr::V4(ip)) && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// Tailscale's CGNAT range (100.64.0.0/10) or its IPv6 prefix (fd7a:115c:a1e0::/48).
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
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

/// This Mac's MagicDNS name and tailnet IPv4 address, when Tailscale is running.
async fn tailnet_self() -> Option<(Option<String>, Option<Ipv4Addr>)> {
    let cli = tailscale_cli()?;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(cli).args(["status", "--json"]).output(),
    )
    .await
    .ok()?
    .ok()?;
    let status: Value = serde_json::from_slice(&output.stdout).ok()?;
    if status.get("BackendState").and_then(Value::as_str).is_some_and(|s| s != "Running") {
        return None;
    }
    let me = status.get("Self")?;
    let name = me
        .get("DNSName")
        .and_then(Value::as_str)
        .map(|n| n.trim_end_matches('.').to_string())
        .filter(|n| !n.is_empty());
    let ip = me
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|s| s.parse::<Ipv4Addr>().ok());
    (name.is_some() || ip.is_some()).then_some((name, ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_ranges() {
        assert!(is_tailnet("100.101.102.103".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet("192.168.1.20".parse().unwrap()));
        assert!(is_tailnet("fd7a:115c:a1e0::1".parse().unwrap()));
    }
}
