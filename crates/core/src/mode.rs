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
