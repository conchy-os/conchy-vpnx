use anyhow::Result;
use tracing_subscriber::EnvFilter;
use zbus::{connection, interface};

struct Vpnx {
    state: String,
}

#[interface(name = "org.conchy.Vpnx")]
impl Vpnx {
    async fn up(&mut self) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("not implemented yet".into()))
    }

    async fn down(&mut self) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("not implemented yet".into()))
    }

    #[zbus(property)]
    async fn state(&self) -> &str {
        &self.state
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let _conn = connection::Builder::session()?
        .name(vpnx_client::BUS_NAME)?
        .serve_at(
            vpnx_client::OBJECT_PATH,
            Vpnx {
                state: "down".into(),
            },
        )?
        .build()
        .await?;

    tracing::info!("conchy-vpnxd running as {}", vpnx_client::BUS_NAME);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
