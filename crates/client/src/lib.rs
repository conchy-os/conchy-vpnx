//! D-Bus client for `conchy-vpnxd`. Used by the CLI, conchy-shell and any other frontend.

pub const BUS_NAME: &str = "org.conchy.Vpnx";
pub const OBJECT_PATH: &str = "/org/conchy/Vpnx";

/// `org.conchy.Vpnx` on the session bus.
///
/// `State` is one of `down`, `starting`, `up`, `restarting`, `error`.
/// `Mode` is the configured mode (`auto`, `tun`, `proxy`); `EffectiveMode` is
/// what is actually running (`tun` or `proxy`, empty while down).
#[zbus::proxy(
    interface = "org.conchy.Vpnx",
    default_service = "org.conchy.Vpnx",
    default_path = "/org/conchy/Vpnx"
)]
pub trait Vpnx {
    fn up(&self) -> zbus::Result<()>;
    fn down(&self) -> zbus::Result<()>;
    /// Re-merge subscription + overlay and apply it.
    fn reload(&self) -> zbus::Result<()>;
    fn set_mode(&self, mode: &str) -> zbus::Result<()>;
    fn set_subscription(&self, url: &str) -> zbus::Result<()>;
    fn update_subscription(&self) -> zbus::Result<()>;
    /// JSON array of groups: `[{"name", "type", "now", "all": [..]}]`.
    fn groups(&self) -> zbus::Result<String>;
    fn select_proxy(&self, group: &str, name: &str) -> zbus::Result<()>;
    /// Latency in milliseconds.
    fn delay(&self, proxy: &str) -> zbus::Result<u32>;
    /// Add a remote overlay (merged before the local `overlay.yaml`).
    fn add_overlay(&self, url: &str) -> zbus::Result<()>;
    fn remove_overlay(&self, url: &str) -> zbus::Result<()>;
    /// Download the remote overlays again and apply them.
    fn update_overlays(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn effective_mode(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn warnings(&self) -> zbus::Result<Vec<String>>;
    /// mihomo may create a TUN device.
    #[zbus(property)]
    fn can_tun(&self) -> zbus::Result<bool>;
    /// The mihomo binary in use; empty when it is not installed.
    #[zbus(property)]
    fn mihomo_path(&self) -> zbus::Result<String>;
    /// JSON: `{"configured", "host", "title", "upload", "download", "total",
    /// "expire", "updated_at", "next_update", "error"}`. Sizes in bytes, times
    /// in Unix seconds; unknown values are `null`, `total`/`expire` of 0 mean
    /// unlimited.
    #[zbus(property)]
    fn subscription(&self) -> zbus::Result<String>;
    /// JSON list: `[{"url", "label", "name", "source", "updated_at", "error"}]`,
    /// in merge order. `name` is the file name, `source` the host or
    /// "local file"; neither contains tokens from the URL.
    #[zbus(property)]
    fn overlays(&self) -> zbus::Result<String>;
    /// The local `overlay.yaml`, merged last; it may not exist yet.
    #[zbus(property)]
    fn overlay_path(&self) -> zbus::Result<String>;
}

/// Root helper installed next to the binaries (`packaging/conchy-vpnx-tun`).
pub const TUN_HELPER: &str = "conchy-vpnx-tun";

const TUN_CAPS: &str = "cap_net_admin,cap_net_bind_service+ep";

/// `pkexec` command that grants (or revokes) TUN access for `mihomo`.
///
/// Run it from the user's session, where polkit can ask for the password,
/// not from the daemon. Afterwards call [`VpnxProxy::reload`] so the daemon
/// picks up the change.
pub fn tun_access_command(enable: bool, mihomo: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("pkexec");
    match find_in_path(TUN_HELPER) {
        Some(helper) => {
            cmd.arg(helper)
                .arg(if enable { "enable" } else { "disable" })
                .arg(mihomo);
        }
        // Development build without the helper: works, but is not restored
        // after mihomo upgrades.
        None if enable => {
            cmd.args(["setcap", TUN_CAPS, mihomo]);
        }
        None => {
            cmd.args(["setcap", "-r", mihomo]);
        }
    }
    cmd
}

fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}
