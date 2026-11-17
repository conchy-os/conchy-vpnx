//! Fetching the subscription profile.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::Value;

/// Providers serve Clash/mihomo YAML (instead of base64 link lists) to this UA.
const USER_AGENT: &str = concat!("clash.meta conchy-vpnx/", env!("CARGO_PKG_VERSION"));

/// Download `url` (or read a `file://` URL), check it is a mihomo profile and
/// save it to `dest`.
pub async fn update(url: &str, dest: &Path) -> Result<Value> {
    let text = fetch(url).await?;
    let profile = parse(&text)?;
    crate::paths::write_atomic(dest, text.as_bytes())
        .with_context(|| format!("saving {}", dest.display()))?;
    Ok(profile)
}

async fn fetch(url: &str) -> Result<String> {
    if let Some(path) = url.strip_prefix("file://") {
        return tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {path}"));
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .context("downloading subscription")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_profiles() {
        assert!(parse("dm1lc3M6Ly9hYmM=").is_err());
        assert!(parse("rules: []").is_err());
        assert!(parse("proxies: []").is_ok());
    }
}
