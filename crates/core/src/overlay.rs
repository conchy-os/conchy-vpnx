//! Overlays fetched from the network (a GitHub raw URL, a gist, any web
//! server) or a local file, cached on disk so the VPN starts offline.
//!
//! A remote overlay can change anything in the config except the settings the
//! daemon forces, so only add sources you trust.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use sha2::{Digest, Sha256};

const USER_AGENT: &str = concat!("conchy-vpnx/", env!("CARGO_PKG_VERSION"));

/// Where the copy of `url` is kept inside `dir`.
pub fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{id}.yaml"))
}

/// Short name for messages: host and file name, never the query string
/// (private links carry tokens there).
pub fn label(url: &str) -> String {
    if let Some(path) = url.strip_prefix("file://") {
        return path.to_owned();
    }
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let file = u
                .path_segments()
                .and_then(|mut s| s.next_back())
                .filter(|f| !f.is_empty())
                .unwrap_or("");
            format!("{}/…/{file}", u.host_str().unwrap_or("?"))
        }
        Err(_) => "overlay".to_owned(),
    }
}

/// Download `url` (or read `file://`), check it is an overlay and save it.
pub async fn fetch(url: &str, dest: &Path) -> Result<()> {
    let text = if let Some(path) = url.strip_prefix("file://") {
        tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {path}"))?
    } else {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        // Private links carry tokens: keep the URL out of errors and logs.
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("downloading")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("server answered {status}");
        }
        resp.text().await.map_err(reqwest::Error::without_url)?
    };
    parse(&text)?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::paths::write_atomic(dest, text.as_bytes())
        .with_context(|| format!("saving {}", dest.display()))
}

/// An overlay is a YAML mapping (or empty).
pub fn parse(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text).context("not valid YAML")?;
    match value {
        Value::Mapping(_) | Value::Null => Ok(value),
        // An HTML error page or a link list instead of the file.
        _ => bail!("not an overlay (expected a YAML mapping)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_hides_tokens() {
        assert_eq!(
            label("https://raw.githubusercontent.com/me/rules/main/overlay.yaml?token=SECRET"),
            "raw.githubusercontent.com/…/overlay.yaml"
        );
        assert_eq!(label("file:///home/me/o.yaml"), "/home/me/o.yaml");
    }

    #[test]
    fn cache_is_per_url() {
        let dir = Path::new("/c");
        assert_eq!(cache_path(dir, "a"), cache_path(dir, "a"));
        assert_ne!(cache_path(dir, "a"), cache_path(dir, "b"));
    }

    #[test]
    fn only_mappings_are_overlays() {
        assert!(parse("prepend-rules: []").is_ok());
        assert!(parse("").is_ok());
        assert!(parse("<html>404</html>").is_err());
        assert!(parse("- a\n- b").is_err());
    }
}
