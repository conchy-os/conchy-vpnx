use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How traffic is routed through mihomo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// TUN if mihomo has CAP_NET_ADMIN, otherwise system proxy.
    #[default]
    Auto,
    Tun,
    Proxy,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Tun => "tun",
            Self::Proxy => "proxy",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "tun" => Ok(Self::Tun),
            "proxy" => Ok(Self::Proxy),
            other => anyhow::bail!("unknown mode `{other}` (expected auto, tun or proxy)"),
        }
    }
}
