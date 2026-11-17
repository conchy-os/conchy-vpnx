//! User settings from `vpnx.kdl`.
//!
//! ```kdl
//! mode "auto"          // auto | tun | proxy
//! mixed-port 7890
//! mihomo "/usr/bin/mihomo"
//! autoconnect true     // bring the VPN up when the daemon starts
//! hwid "..."           // device id sent to the subscription server
//! subscription-update-hours 12   // default: what the server asks for, else 24; 0 = never
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kdl::{KdlDocument, KdlNode};

use crate::Mode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub mixed_port: u16,
    /// Explicit mihomo binary; looked up in `PATH` when unset.
    pub mihomo: Option<PathBuf>,
    /// Bring the VPN up as soon as the daemon starts.
    pub autoconnect: bool,
    /// Device id for the subscription server; derived from machine-id when unset.
    pub hwid: Option<String>,
    /// Auto-update interval; `None` follows the server, 0 turns it off.
    pub subscription_update_hours: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            mixed_port: 7890,
            mihomo: None,
            autoconnect: false,
            hwid: None,
            subscription_update_hours: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let doc: KdlDocument = text.parse()?;
        let mut s = Self::default();
        if let Some(v) = doc.get_arg("mode").and_then(|v| v.as_string()) {
            s.mode = v.parse()?;
        }
        if let Some(v) = doc.get_arg("mixed-port").and_then(|v| v.as_i64()) {
            s.mixed_port = u16::try_from(v).context("mixed-port out of range")?;
        }
        if let Some(v) = doc.get_arg("mihomo").and_then(|v| v.as_string()) {
            s.mihomo = Some(v.into());
        }
        if let Some(v) = doc.get_arg("autoconnect").and_then(|v| v.as_bool()) {
            s.autoconnect = v;
        }
        if let Some(v) = doc
            .get_arg("subscription-update-hours")
            .and_then(|v| v.as_i64())
        {
            s.subscription_update_hours =
                Some(u32::try_from(v).context("subscription-update-hours out of range")?);
        }
        if let Some(v) = doc.get_arg("hwid").and_then(|v| v.as_string()) {
            s.hwid = Some(v.to_owned()).filter(|v| !v.trim().is_empty());
        }
        Ok(s)
    }

    /// Change `mode` in the file, keeping the rest of it (and its comments) intact.
    pub fn save_mode(path: &Path, mode: Mode) -> Result<()> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: KdlDocument = text.parse()?;
        let value = mode.as_str();
        match doc.get_mut("mode") {
            Some(node) => {
                node.entries_mut().clear();
                node.push(value);
            }
            None => {
                let mut node = KdlNode::new("mode");
                node.push(value);
                doc.nodes_mut().push(node);
            }
        }
        crate::paths::write_atomic(path, doc.to_string().as_bytes())?;
        Ok(())
    }

    /// The mihomo binary to run.
    pub fn mihomo_bin(&self) -> Result<PathBuf> {
        if let Some(p) = &self.mihomo {
            return Ok(p.clone());
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("mihomo"))
            .find(|p| p.is_file())
            .context("mihomo not found in PATH; install it or set `mihomo` in vpnx.kdl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults() {
        let s = Settings::parse("mode \"tun\"\nmixed-port 1080\nautoconnect true").unwrap();
        assert_eq!(s.mode, Mode::Tun);
        assert!(s.autoconnect);
        assert_eq!(s.mixed_port, 1080);
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
        assert!(Settings::parse("mode \"wat\"").is_err());
    }

    #[test]
    fn save_mode_keeps_other_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vpnx.kdl");
        std::fs::write(&path, "// mine\nmixed-port 1080\nmode \"auto\"\n").unwrap();
        Settings::save_mode(&path, Mode::Proxy).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("// mine"));
        let s = Settings::parse(&text).unwrap();
        assert_eq!((s.mode, s.mixed_port), (Mode::Proxy, 1080));
    }
}
