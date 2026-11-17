//! D-Bus client for `conchy-vpnxd`. Used by the CLI, conchy-shell and any other frontend.

pub const BUS_NAME: &str = "org.conchy.Vpnx";
pub const OBJECT_PATH: &str = "/org/conchy/Vpnx";

#[zbus::proxy(
    interface = "org.conchy.Vpnx",
    default_service = "org.conchy.Vpnx",
    default_path = "/org/conchy/Vpnx"
)]
pub trait Vpnx {
    fn up(&self) -> zbus::Result<()>;
    fn down(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}
