//! Core logic of conchy-vpnx: everything that does not depend on D-Bus or a UI.

pub mod api;
pub mod caps;
pub mod conflict;
pub mod device;
pub mod merge;
pub mod mihomo;
pub mod mode;
pub mod overlay;
pub mod paths;
pub mod settings;
pub mod subscription;

pub use mode::Mode;
