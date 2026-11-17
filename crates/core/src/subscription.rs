//! Fetching the subscription profile.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::Value;

use crate::device::Device;

/// Providers serve Clash/mihomo YAML (instead of base64 link lists) to this UA.
const USER_AGENT: &str = concat!("clash.meta conchy-vpnx/", env!("CARGO_PKG_VERSION"));

/// Download `url` (or read a `file://` URL), check it is a mihomo profile and
/// save it to `dest`.
pub async fn update(url: &str, dest: &Path, device: &Device) -> Result<Value> {
    let text = fetch(url, device).await?;
    let profile = parse(&text)?;
    if let Some(message) = stub_message(&profile) {
        bail!("the subscription server sent no usable servers: {message}");
    }
    crate::paths::write_atomic(dest, text.as_bytes())
        .with_context(|| format!("saving {}", dest.display()))?;
    Ok(profile)
}

async fn fetch(url: &str, device: &Device) -> Result<String> {
    if let Some(path) = url.strip_prefix("file://") {
        return tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {path}"));
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut req = client.get(url);
    for (name, value) in device.headers() {
        req = req.header(name, value);
    }
    let resp = req.send().await.context("downloading subscription")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("subscription server answered {status}");
    }
    Ok(resp.text().await?)
}

pub fn parse(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text)
        .context("subscription is not YAML (base64 link lists are not supported yet)")?;
    let Value::Mapping(m) = &value else {
        bail!(
            "subscription is not a mihomo/Clash profile (base64 link lists are not supported yet)"
        );
    };
    if !m.contains_key("proxies") && !m.contains_key("proxy-providers") {
        bail!("subscription has neither `proxies` nor `proxy-providers`");
    }
    Ok(value)
}

/// Panels answer with placeholder proxies (`0.0.0.0:1`) whose names carry
/// the reason: HWID missing, device limit reached, subscription expired, ...
fn stub_message(profile: &Value) -> Option<String> {
    let proxies = profile.get("proxies")?.as_sequence()?;
    if profile.get("proxy-providers").is_some() {
        return None;
    }
    let real: Vec<&Value> = proxies
        .iter()
        .filter(|p| !matches!(p["type"].as_str(), Some("direct" | "dns" | "reject")))
        .collect();
    let is_stub = |p: &&Value| {
        matches!(
            p["server"].as_str(),
            Some("0.0.0.0" | "127.0.0.1" | "localhost")
        ) || matches!(p["port"].as_u64(), Some(0 | 1))
    };
    if real.is_empty() || !real.iter().all(is_stub) {
        return None;
    }
    let names: Vec<&str> = real.iter().filter_map(|p| p["name"].as_str()).collect();
    Some(if names.is_empty() {
        "placeholder servers only".to_owned()
    } else {
        names.join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stub_profiles() {
        let stub: Value = serde_yaml::from_str(
            "proxies:\n  - {name: DIRECT, type: direct}\n  - {name: 'Please turn on HWID in your app', type: vless, server: 0.0.0.0, port: 1, uuid: x}",
        )
        .unwrap();
        assert_eq!(
            stub_message(&stub).as_deref(),
            Some("Please turn on HWID in your app")
        );
        let real: Value = serde_yaml::from_str(
            "proxies:\n  - {name: nl, type: vless, server: 203.0.113.5, port: 443, uuid: x}\n  - {name: info, type: vless, server: 0.0.0.0, port: 1, uuid: x}",
        )
        .unwrap();
        assert_eq!(stub_message(&real), None);
    }

    #[test]
    fn rejects_non_profiles() {
        assert!(parse("dm1lc3M6Ly9hYmM=").is_err());
        assert!(parse("rules: []").is_err());
        assert!(parse("proxies: []").is_ok());
    }
}
