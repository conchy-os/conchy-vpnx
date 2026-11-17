//! Where conchy-vpnx keeps its files.
//!
//! ```text
//! $XDG_CONFIG_HOME/conchy/vpnx.kdl              settings
//! $XDG_CONFIG_HOME/conchy/vpnx/subscription.url
//! $XDG_CONFIG_HOME/conchy/vpnx/overlay.yaml     user rules
//! $XDG_DATA_HOME/conchy/vpnx/subscription.yaml  last downloaded profile
//! $XDG_DATA_HOME/conchy/vpnx/mihomo/            mihomo home (config.yaml, geodata)
//! $XDG_RUNTIME_DIR/conchy-vpnx/mihomo.sock      mihomo controller
//! ```

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub settings: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let xdg = |var: &str, fallback: &str| {
            env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        let config = xdg("XDG_CONFIG_HOME", ".config").join("conchy");
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        Self {
            settings: config.join("vpnx.kdl"),
            config_dir: config.join("vpnx"),
            data_dir: xdg("XDG_DATA_HOME", ".local/share").join("conchy/vpnx"),
            runtime_dir: runtime.join("conchy-vpnx"),
        }
    }

    pub fn subscription_url(&self) -> PathBuf {
        self.config_dir.join("subscription.url")
    }
    pub fn overlay(&self) -> PathBuf {
        self.config_dir.join("overlay.yaml")
    }
    pub fn subscription(&self) -> PathBuf {
        self.data_dir.join("subscription.yaml")
    }
    /// Title, traffic and expiry from the last download.
    pub fn subscription_meta(&self) -> PathBuf {
        self.data_dir.join("subscription.meta.json")
    }
    pub fn mihomo_home(&self) -> PathBuf {
        self.data_dir.join("mihomo")
    }
    pub fn mihomo_config(&self) -> PathBuf {
        self.mihomo_home().join("config.yaml")
    }
    pub fn mihomo_config_prev(&self) -> PathBuf {
        self.mihomo_home().join("config.prev.yaml")
    }
    pub fn controller_socket(&self) -> PathBuf {
        self.runtime_dir.join("mihomo.sock")
    }
}

/// Write via a temp file + rename so readers never see a half-written file.
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, path)
}
