//! Device identity for subscription servers.
//!
//! Panels such as Remnawave bind a subscription to devices: without an
//! `x-hwid` header they answer with a stub profile ("Please turn on HWID in
//! your app") instead of the real servers, and they count devices by it.

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable per machine, so re-downloading never counts as a new device.
    pub hwid: String,
    pub os: String,
    pub os_version: String,
    pub model: String,
}

impl Device {
    /// `hwid` overrides the derived id (e.g. to reuse the one another client
    /// on this machine already registered).
    pub fn detect(hwid: Option<&str>) -> Self {
        let hwid = match hwid {
            Some(h) => h.to_owned(),
            None => derive_hwid(&machine_id()),
        };
        Self {
            hwid,
            os: "Linux".to_owned(),
            os_version: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_default(),
            model: model(),
        }
    }

    /// Headers understood by Remnawave and compatible panels.
    pub fn headers(&self) -> [(&'static str, &str); 4] {
        [
            ("x-hwid", &self.hwid),
            ("x-device-os", &self.os),
            ("x-ver-os", &self.os_version),
            ("x-device-model", &self.model),
        ]
    }
}

fn machine_id() -> String {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|p| read_trimmed(p))
        // No machine-id at all: fall back to something stable-ish.
        .unwrap_or_else(|| read_trimmed("/proc/sys/kernel/hostname").unwrap_or_default())
}

/// Never send the machine-id itself: systemd asks applications to use an
/// app-specific hash of it.
fn derive_hwid(machine_id: &str) -> String {
    let digest = Sha256::digest(format!("conchy-vpnx:{machine_id}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Shown in the provider's device list.
fn model() -> String {
    let dmi = read_trimmed("/sys/class/dmi/id/product_name")
        .filter(|m| !m.to_ascii_lowercase().contains("to be filled"));
    dmi.or_else(os_name)
        .unwrap_or_else(|| "Linux PC".to_owned())
}

fn os_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| {
            l.strip_prefix("PRETTY_NAME=")
                .or_else(|| l.strip_prefix("NAME="))
        })
        .map(|v| v.trim_matches('"').to_owned())
}

fn read_trimmed(path: &str) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hwid_is_stable_and_hides_machine_id() {
        let a = derive_hwid("0123456789abcdef0123456789abcdef");
        assert_eq!(a, derive_hwid("0123456789abcdef0123456789abcdef"));
        assert_ne!(a, derive_hwid("ffffffffffffffffffffffffffffffff"));
        assert_eq!(a.len(), 32);
        assert!(!a.contains("0123456789abcdef"));
    }

    #[test]
    fn override_wins() {
        assert_eq!(Device::detect(Some("my-id")).hwid, "my-id");
    }
}
