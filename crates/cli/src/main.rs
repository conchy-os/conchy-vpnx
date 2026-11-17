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
    /// Remote overlays: merge configs downloaded from the network
    #[command(subcommand)]
    Overlay(OverlayCmd),
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
    /// Title, traffic, expiry and update times
    Show {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum OverlayCmd {
    /// Remote overlays in merge order (the local overlay.yaml comes last)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add an overlay by URL (https:// or file://); it is downloaded first
    Add { url: String },
    /// Remove an overlay by URL or by its number in `list`
    Remove { which: String },
    /// Download all overlays again and apply them
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`conchy-vpnx status | head -1`) instead
    // of panicking in println!.
    // SAFETY: restoring the default disposition before any other thread uses
    // signals.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
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
        Command::Subscription(Sub::Show { json }) => {
            let text = vpnx.subscription().await?;
            if json {
                println!("{text}");
            } else {
                for line in subscription_lines(&serde_json::from_str(&text)?) {
                    println!("{line}");
                }
            }
        }
        Command::Groups { json } => groups(&vpnx, json).await?,
        Command::Select { group, proxy } => vpnx.select_proxy(&group, &proxy).await?,
        Command::Delay { proxy } => println!("{} ms", vpnx.delay(&proxy).await?),
        Command::Overlay(cmd) => overlay(&vpnx, cmd).await?,
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
    let sub: Value = serde_json::from_str(&vpnx.subscription().await?).unwrap_or_default();
    if let Some(line) = subscription_lines(&sub).into_iter().next() {
        println!("sub:   {}", line.trim_start_matches("subscription: "));
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

async fn overlay(vpnx: &VpnxProxy<'_>, cmd: OverlayCmd) -> Result<()> {
    let list = || async {
        let text = vpnx.overlays().await?;
        anyhow::Ok(serde_json::from_str::<Vec<Value>>(&text)?)
    };
    match cmd {
        OverlayCmd::List { json } => {
            if json {
                println!("{}", vpnx.overlays().await?);
                return Ok(());
            }
            let items = list().await?;
            if items.is_empty() {
                println!("no remote overlays; add one with `conchy-vpnx overlay add <url>`");
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            for (i, o) in items.iter().enumerate() {
                let status = match (o["error"].as_str(), o["updated_at"].as_u64()) {
                    (Some(e), _) => format!("error: {e}"),
                    (None, Some(at)) => format!("updated {} ago", span(now.saturating_sub(at))),
                    (None, None) => "not downloaded".into(),
                };
                println!(
                    "{}. {}\n   {status}",
                    i + 1,
                    o["url"].as_str().unwrap_or("?")
                );
            }
        }
        OverlayCmd::Add { url } => vpnx.add_overlay(&url).await?,
        OverlayCmd::Remove { which } => {
            let url = match which.parse::<usize>() {
                Ok(n) => list()
                    .await?
                    .get(n.wrapping_sub(1))
                    .and_then(|o| o["url"].as_str().map(str::to_owned))
                    .with_context(|| format!("no overlay number {n}"))?,
                Err(_) => which,
            };
            vpnx.remove_overlay(&url).await?;
        }
        OverlayCmd::Update => vpnx.update_overlays().await?,
    }
    Ok(())
}

/// Human-readable lines; the first one is a summary.
fn subscription_lines(sub: &Value) -> Vec<String> {
    if sub["configured"] != Value::Bool(true) {
        return vec!["subscription: none, use `conchy-vpnx subscription set <url>`".into()];
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let name = sub["title"]
        .as_str()
        .or(sub["host"].as_str())
        .unwrap_or("subscription");
    let mut summary = vec![name.to_owned()];
    let used = sub["upload"].as_u64().unwrap_or(0) + sub["download"].as_u64().unwrap_or(0);
    match sub["total"].as_u64().filter(|&t| t > 0) {
        Some(total) => summary.push(format!("{} of {} used", size(used), size(total))),
        None if used > 0 => summary.push(format!("{} used", size(used))),
        None => {}
    }
    if let Some(expire) = sub["expire"].as_u64().filter(|&e| e > 0) {
        summary.push(if expire <= now {
            "expired".into()
        } else {
            format!("{} left", span(expire - now))
        });
    }
    let mut lines = vec![format!("subscription: {}", summary.join(" · "))];
    if let Some(host) = sub["host"].as_str() {
        lines.push(format!("server:       {host}"));
    }
    if let Some(at) = sub["updated_at"].as_u64() {
        lines.push(format!(
            "updated:      {} ago",
            span(now.saturating_sub(at))
        ));
    }
    match sub["next_update"].as_u64() {
        Some(next) if next > now => lines.push(format!("next update:  in {}", span(next - now))),
        Some(_) => lines.push("next update:  due".into()),
        None => lines.push("next update:  off".into()),
    }
    if let Some(err) = sub["error"].as_str() {
        lines.push(format!("last error:   {err}"));
    }
    lines
}

fn size(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gb = bytes as f64 / GB;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 0.1 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

fn span(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 2 * 86400 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86400),
    }
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
