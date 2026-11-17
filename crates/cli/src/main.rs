use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
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
    /// Start the VPN (or apply the current config if it is running)
    Up,
    /// Stop the VPN
    Down,
    /// Show state, mode and merge warnings
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Set the mode: auto, tun or proxy
    Mode { mode: String },
    /// Re-merge subscription + overlay.yaml and apply
    Reload,
    /// Manage the subscription
    #[command(subcommand)]
    Subscription(Sub),
    /// List proxy groups and the selected proxy
    Groups {
        #[arg(long)]
        json: bool,
    },
    /// Select a proxy in a group
    Select { group: String, proxy: String },
    /// Measure latency of a proxy
    Delay { proxy: String },
    /// Allow or forbid TUN mode (asks for the administrator password)
    Tun {
        #[arg(value_parser = ["enable", "disable"])]
        action: String,
    },
}

#[derive(Subcommand)]
enum Sub {
    /// Set the subscription URL and download it
    Set { url: String },
    /// Download the subscription again
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = zbus::Connection::session().await?;
    let vpnx = VpnxProxy::new(&conn).await?;
    let ping = vpnx.state().await;
    ping.context("conchy-vpnxd is not running")?;

    match cli.command {
        Command::Up => vpnx.up().await?,
        Command::Down => vpnx.down().await?,
        Command::Status { json } => status(&vpnx, json).await?,
        Command::Mode { mode } => vpnx.set_mode(&mode).await?,
        Command::Reload => vpnx.reload().await?,
        Command::Subscription(Sub::Set { url }) => vpnx.set_subscription(&url).await?,
        Command::Subscription(Sub::Update) => vpnx.update_subscription().await?,
        Command::Groups { json } => groups(&vpnx, json).await?,
        Command::Select { group, proxy } => vpnx.select_proxy(&group, &proxy).await?,
        Command::Delay { proxy } => println!("{} ms", vpnx.delay(&proxy).await?),
        Command::Tun { action } => tun(&vpnx, action == "enable").await?,
    }
    Ok(())
}

async fn status(vpnx: &VpnxProxy<'_>, as_json: bool) -> Result<()> {
    let (state, mode, effective, error, warnings, can_tun, mihomo) = (
        vpnx.state().await?,
        vpnx.mode().await?,
        vpnx.effective_mode().await?,
        vpnx.last_error().await?,
        vpnx.warnings().await?,
        vpnx.can_tun().await?,
        vpnx.mihomo_path().await?,
    );
    if as_json {
        let v = json!({
            "state": state, "mode": mode, "effective_mode": effective,
            "error": error, "warnings": warnings,
            "can_tun": can_tun, "mihomo": mihomo,
        });
        println!("{v}");
        return Ok(());
    }
    let running = if effective.is_empty() {
        String::new()
    } else {
        format!(" ({effective})")
    };
    println!("state: {state}{running}\nmode:  {mode}");
    match (mihomo.is_empty(), can_tun) {
        (true, _) => println!("tun:   mihomo not found"),
        (false, true) => println!("tun:   available"),
        (false, false) => {
            println!("tun:   not allowed, run `conchy-vpnx tun enable`")
        }
    }
    if !error.is_empty() {
        println!("error: {error}");
    }
    for w in warnings {
        println!("warning: {w}");
    }
    Ok(())
}

async fn tun(vpnx: &VpnxProxy<'_>, enable: bool) -> Result<()> {
    let mihomo = vpnx.mihomo_path().await?;
    if mihomo.is_empty() {
        bail!("mihomo is not installed");
    }
    let cmd = vpnx_client::tun_access_command(enable, &mihomo);
    let status = tokio::process::Command::from(cmd)
        .status()
        .await
        .context("running pkexec")?;
    if !status.success() {
        bail!("pkexec failed or was cancelled ({status})");
    }
    // Re-checks the capability; without a subscription there is nothing
    // else to apply, which is fine while the VPN is down.
    if let Err(e) = vpnx.reload().await {
        if vpnx.state().await? != "down" {
            return Err(e.into());
        }
    }
    println!(
        "TUN {}",
        if vpnx.can_tun().await? {
            "available"
        } else {
            "not available"
        }
    );
    Ok(())
}

async fn groups(vpnx: &VpnxProxy<'_>, as_json: bool) -> Result<()> {
    let text = vpnx.groups().await?;
    if as_json {
        println!("{text}");
        return Ok(());
    }
    let groups: Value = serde_json::from_str(&text)?;
    for g in groups.as_array().into_iter().flatten() {
        println!(
            "{} [{}] -> {}",
            g["name"].as_str().unwrap_or("?"),
            g["type"].as_str().unwrap_or("?"),
            g["now"].as_str().unwrap_or("-")
        );
        for p in g["all"].as_array().into_iter().flatten() {
            let p = p.as_str().unwrap_or("?");
            let mark = if Some(p) == g["now"].as_str() {
                "*"
            } else {
                " "
            };
            println!("  {mark} {p}");
        }
    }
    Ok(())
}
