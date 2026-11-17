//! Detect another VPN that already sends all traffic into its own TUN device
//! (FlClash, sing-box, Clash Verge, OpenVPN with redirect-gateway, ...).
//!
//! mihomo-based clients use the same routing table (2022) and `ip rule`s, so
//! a second TUN breaks both: routes get overwritten, and whichever stops first
//! removes the other's rules.

use std::path::Path;

use serde_json::Value;
use tokio::process::Command;

/// Destinations that together mean "everything".
const CATCH_ALL: &[&str] = &[
    "default",
    "0.0.0.0/0",
    "0.0.0.0/1",
    "128.0.0.0/1",
    "::/0",
    "::/1",
    "8000::/1",
];

/// Name of a TUN device that currently carries all traffic, if any.
///
/// Call it only while our own TUN is not up. Returns `None` when `ip` is
/// unavailable: better to try than to refuse on a guess.
pub async fn foreign_tunnel() -> Option<String> {
    for family in ["-4", "-6"] {
        let out = Command::new("ip")
            .args([family, "-j", "route", "show", "table", "all"])
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(dev) = catch_all_device(&text, is_tun) {
            return Some(dev);
        }
    }
    None
}

fn is_tun(dev: &str) -> bool {
    // Only TUN/TAP devices have this attribute (not ethernet, wireguard, ...).
    Path::new("/sys/class/net")
        .join(dev)
        .join("tun_flags")
        .exists()
}

/// First device in `ip -j route` output that has a catch-all route and is a TUN.
fn catch_all_device(routes_json: &str, is_tun: impl Fn(&str) -> bool) -> Option<String> {
    let routes: Value = serde_json::from_str(routes_json).ok()?;
    routes
        .as_array()?
        .iter()
        .filter(|r| {
            r["dst"]
                .as_str()
                .is_some_and(|dst| CATCH_ALL.contains(&dst))
        })
        // Unreachable/blackhole routes have no device.
        .filter_map(|r| r["dev"].as_str())
        .find(|dev| is_tun(dev))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tun(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |dev| names.contains(&dev)
    }

    #[test]
    fn flclash_policy_table() {
        let routes = r#"[
            {"dst":"default","gateway":"198.18.0.2","dev":"FlClashX","table":"2022","flags":[]},
            {"dst":"default","gateway":"192.168.1.1","dev":"enp37s0","protocol":"dhcp","metric":100,"flags":[]},
            {"type":"local","dst":"127.0.0.0/8","dev":"lo","table":"local","flags":[]}
        ]"#;
        assert_eq!(
            catch_all_device(routes, tun(&["FlClashX"])).as_deref(),
            Some("FlClashX")
        );
    }

    #[test]
    fn openvpn_split_default() {
        let routes = r#"[
            {"dst":"0.0.0.0/1","gateway":"10.8.0.1","dev":"tun0","flags":[]},
            {"dst":"128.0.0.0/1","gateway":"10.8.0.1","dev":"tun0","flags":[]},
            {"dst":"default","gateway":"192.168.1.1","dev":"wlan0","flags":[]}
        ]"#;
        assert_eq!(
            catch_all_device(routes, tun(&["tun0"])).as_deref(),
            Some("tun0")
        );
    }

    #[test]
    fn tailscale_without_exit_node_is_fine() {
        let routes = r#"[
            {"dst":"100.64.0.0/10","dev":"tailscale0","table":"52","flags":[]},
            {"dst":"default","gateway":"192.168.1.1","dev":"wlan0","flags":[]},
            {"type":"unreachable","dst":"default","table":"99","flags":[]}
        ]"#;
        assert_eq!(catch_all_device(routes, tun(&["tailscale0"])), None);
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(catch_all_device("not json", tun(&[])), None);
        assert_eq!(catch_all_device("{}", tun(&[])), None);
    }
}
