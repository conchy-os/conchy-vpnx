//! Fetching the subscription profile.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::device::Device;

/// Providers serve Clash/mihomo YAML (instead of base64 link lists) to this UA.
const USER_AGENT: &str = concat!("clash.meta conchy-vpnx/", env!("CARGO_PKG_VERSION"));

/// What the subscription server says about the subscription, from the
/// headers most panels send (`subscription-userinfo`, `profile-title`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub title: Option<String>,
    /// Bytes.
    pub upload: Option<u64>,
    pub download: Option<u64>,
    /// Traffic limit in bytes; `None` or 0 means unlimited.
    pub total: Option<u64>,
    /// Unix time the subscription ends; `None` or 0 means never.
    pub expire: Option<u64>,
    /// How often the server wants to be polled.
    pub interval_hours: Option<u32>,
    /// Unix time of the download.
    pub updated_at: u64,
}

impl Meta {
    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::paths::write_atomic(path, &serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("saving {}", path.display()))
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Host of the subscription URL, to show which provider it is without the
/// secret token in the path.
pub fn display_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url.trim()).ok()?;
    match url.scheme() {
        "file" => Some("local file".to_owned()),
        _ => url.host_str().map(str::to_owned),
    }
}

/// Download `url` (or read a `file://` URL), check it is a mihomo profile and
/// save it to `dest`.
pub async fn update(url: &str, dest: &Path, device: &Device) -> Result<Meta> {
    let (text, mut meta) = fetch(url, device).await?;
    let profile = parse(&text)?;
    if let Some(message) = stub_message(&profile) {
        bail!("the subscription server sent no usable servers: {message}");
    }
    crate::paths::write_atomic(dest, text.as_bytes())
        .with_context(|| format!("saving {}", dest.display()))?;
    meta.updated_at = now();
    Ok(meta)
}

async fn fetch(url: &str, device: &Device) -> Result<(String, Meta)> {
    if let Some(path) = url.strip_prefix("file://") {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {path}"))?;
        return Ok((text, Meta::default()));
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut req = client.get(url);
    for (name, value) in device.headers() {
        req = req.header(name, value);
    }
    // The URL carries the account's token: keep it out of errors and logs.
    let resp = req
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("downloading subscription")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("subscription server answered {status}");
    }
    let meta = meta_from_headers(resp.headers());
    let text = resp.text().await.map_err(reqwest::Error::without_url)?;
    Ok((text, meta))
}

fn meta_from_headers(headers: &HeaderMap) -> Meta {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut meta = Meta {
        title: get("profile-title")
            .and_then(decode_title)
            .or_else(|| get("content-disposition").and_then(disposition_name)),
        interval_hours: get("profile-update-interval").and_then(|v| v.trim().parse().ok()),
        ..Meta::default()
    };
    if let Some(info) = get("subscription-userinfo") {
        // "upload=1; download=2; total=3; expire=4"
        for part in info.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            // Some panels send floats or empty values.
            let value = value.trim().parse::<f64>().ok().map(|v| v as u64);
            match key.trim() {
                "upload" => meta.upload = value,
                "download" => meta.download = value,
                "total" => meta.total = value,
                "expire" => meta.expire = value,
                _ => {}
            }
        }
    }
    meta
}

/// Plain text, or `base64:<text>` (Remnawave, Marzban).
fn decode_title(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let title = match raw.strip_prefix("base64:") {
        Some(b64) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()?;
            String::from_utf8(bytes).ok()?
        }
        None => raw.to_owned(),
    };
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// `attachment; filename="name.yaml"` or `filename*=UTF-8''name`.
fn disposition_name(raw: &str) -> Option<String> {
    let name = raw.split(';').find_map(|p| {
        let p = p.trim();
        p.strip_prefix("filename*=UTF-8''")
            .map(|n| {
                percent_encoding::percent_decode_str(n)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .or_else(|| {
                p.strip_prefix("filename=")
                    .map(|n| n.trim_matches('"').to_owned())
            })
    })?;
    let name = name
        .trim_end_matches(".yaml")
        .trim_end_matches(".yml")
        .trim();
    (!name.is_empty()).then(|| name.to_owned())
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
    // Not 127.0.0.1: chaining to a local proxy (Tor, xray) is legitimate.
    let is_stub = |p: &&Value| {
        p["server"].as_str() == Some("0.0.0.0") || matches!(p["port"].as_u64(), Some(0 | 1))
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
    fn reads_provider_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            "subscription-userinfo",
            "upload=1024; download=2048; total=107374182400; expire=1767225600"
                .parse()
                .unwrap(),
        );
        h.insert("profile-update-interval", "12".parse().unwrap());
        // "Мой VPN"
        h.insert("profile-title", "base64:0JzQvtC5IFZQTg==".parse().unwrap());
        let m = meta_from_headers(&h);
        assert_eq!(m.title.as_deref(), Some("Мой VPN"));
        assert_eq!(
            (m.upload, m.download, m.total, m.expire),
            (Some(1024), Some(2048), Some(107374182400), Some(1767225600))
        );
        assert_eq!(m.interval_hours, Some(12));

        let mut h = HeaderMap::new();
        h.insert(
            "content-disposition",
            "attachment; filename=\"user_1.yaml\"".parse().unwrap(),
        );
        h.insert(
            "subscription-userinfo",
            "upload=; download=5.0".parse().unwrap(),
        );
        let m = meta_from_headers(&h);
        assert_eq!(m.title.as_deref(), Some("user_1"));
        assert_eq!((m.upload, m.download), (None, Some(5)));
    }

    #[test]
    fn host_hides_the_token() {
        assert_eq!(
            display_host("https://sub.example.com/api/sub/SECRET").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(
            display_host("file:///tmp/x.yaml").as_deref(),
            Some("local file")
        );
        assert_eq!(display_host("not a url"), None);
    }

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
