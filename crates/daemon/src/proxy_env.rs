//! System proxy for `proxy` mode: environment of newly started programs
//! (D-Bus activation + systemd user manager) and GNOME/GTK proxy settings.
//!
//! Programs that are already running keep their old environment.

use std::collections::HashMap;

use tokio::process::Command;
use zbus::Connection;

const ENV_VARS: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "no_proxy",
    "NO_PROXY",
];

fn values(port: u16) -> Vec<(&'static str, String)> {
    let http = format!("http://127.0.0.1:{port}");
    let socks = format!("socks5://127.0.0.1:{port}");
    let no = "localhost,127.0.0.1,::1".to_owned();
    ENV_VARS
        .iter()
        .map(|&k| {
            let v = match k {
                "all_proxy" | "ALL_PROXY" => socks.clone(),
                "no_proxy" | "NO_PROXY" => no.clone(),
                _ => http.clone(),
            };
            (k, v)
        })
        .collect()
}

pub async fn set(conn: &Connection, port: u16) {
    let vars = values(port);
    update_dbus(conn, vars.iter().map(|(k, v)| (*k, v.as_str())).collect()).await;
    let assignments: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
    systemd(conn, "SetEnvironment", assignments).await;

    let port = port.to_string();
    let mut cmds: Vec<Vec<&str>> = vec![vec!["org.gnome.system.proxy", "mode", "manual"]];
    for schema in [
        "org.gnome.system.proxy.http",
        "org.gnome.system.proxy.https",
        "org.gnome.system.proxy.socks",
    ] {
        cmds.push(vec![schema, "host", "127.0.0.1"]);
        cmds.push(vec![schema, "port", &port]);
    }
    for args in cmds {
        gsettings(&args).await;
    }
}

pub async fn clear(conn: &Connection) {
    // dbus-daemon cannot unset activation variables; empty means "no proxy".
    update_dbus(conn, ENV_VARS.iter().map(|&k| (k, "")).collect()).await;
    systemd(
        conn,
        "UnsetEnvironment",
        ENV_VARS.iter().map(|k| k.to_string()).collect(),
    )
    .await;
    gsettings(&["org.gnome.system.proxy", "mode", "none"]).await;
}

async fn update_dbus(conn: &Connection, vars: HashMap<&str, &str>) {
    let r = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "UpdateActivationEnvironment",
            &(vars,),
        )
        .await;
    if let Err(e) = r {
        tracing::debug!("UpdateActivationEnvironment failed: {e}");
    }
}

/// No-op without a systemd user manager (e.g. on Void with runit).
async fn systemd(conn: &Connection, method: &str, arg: Vec<String>) {
    let r = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            method,
            &(arg,),
        )
        .await;
    if let Err(e) = r {
        tracing::debug!("systemd {method} skipped: {e}");
    }
}

async fn gsettings(args: &[&str]) {
    // Capture stderr: without dconf, gsettings warns on every call.
    match Command::new("gsettings")
        .arg("set")
        .args(args)
        .output()
        .await
    {
        Ok(out) if !out.status.success() || !out.stderr.is_empty() => tracing::debug!(
            "gsettings {args:?}: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::debug!("gsettings unavailable: {e}"),
        _ => {}
    }
}
