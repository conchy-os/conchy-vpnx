use anyhow::Result;
use clap::{Parser, Subcommand};
use vpnx_client::VpnxProxy;

#[derive(Parser)]
#[command(
    name = "conchy-vpnx",
    version,
    about = "Control the conchy-vpnx daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the VPN
    Up,
    /// Stop the VPN
    Down,
    /// Show the current state
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = zbus::Connection::session().await?;
    let vpnx = VpnxProxy::new(&conn).await?;
    match cli.command {
        Command::Up => vpnx.up().await?,
        Command::Down => vpnx.down().await?,
        Command::Status => println!("{}", vpnx.state().await?),
    }
    Ok(())
}
