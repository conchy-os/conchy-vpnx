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

/// GNOME proxy keys the daemon changes, restored on [`clear`].
const GSETTINGS_KEYS: &[(&str, &str)] = &[
    ("org.gnome.system.proxy", "mode"),
    ("org.gnome.system.proxy.http", "host"),
    ("org.gnome.system.proxy.http", "port"),
    ("org.gnome.system.proxy.https", "host"),
    ("org.gnome.system.proxy.https", "port"),
    ("org.gnome.system.proxy.socks", "host"),
    ("org.gnome.system.proxy.socks", "port"),
];

/// The user's own GNOME proxy settings from before [`set`].
#[derive(Debug, Default)]
pub struct Saved(Vec<(&'static str, &'static str, String)>);

pub async fn set(conn: &Connection, port: u16) -> Saved {
    let vars = values(port);
    update_dbus(conn, vars.iter().map(|(k, v)| (*k, v.as_str())).collect()).await;
    let assignments: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
    systemd(conn, "SetEnvironment", assignments).await;

    let saved = save_gsettings(port).await;
    let port = port.to_string();
    for (schema, key) in GSETTINGS_KEYS {
        let value = match *key {
            "mode" => "manual",
            "host" => "127.0.0.1",
            _ => &port,
        };
        gsettings(&["set", schema, key, value]).await;
    }
    saved
}

pub async fn clear(conn: &Connection, saved: Option<Saved>) {
    // dbus-daemon cannot unset activation variables; empty means "no proxy".
    update_dbus(conn, ENV_VARS.iter().map(|&k| (k, "")).collect()).await;
    systemd(
        conn,
        "UnsetEnvironment",
        ENV_VARS.iter().map(|k| k.to_string()).collect(),
    )
    .await;
    match saved {
        Some(Saved(values)) if !values.is_empty() => {
            // Mode last, so the proxy never points at a half-restored address.
            for (schema, key, value) in values.iter().rev() {
                gsettings(&["set", schema, key, value]).await;
            }
        }
        // Unknown (the daemon restarted while up): just switch it off.
        _ => {
            gsettings(&["set", "org.gnome.system.proxy", "mode", "none"]).await;
        }
    }
}

async fn save_gsettings(port: u16) -> Saved {
    let mut values = Vec::new();
    for &(schema, key) in GSETTINGS_KEYS {
        match gsettings(&["get", schema, key]).await {
            Some(v) => values.push((schema, key, v)),
            None => return Saved::default(),
        }
    }
    // Left over from a crash of ours: that is not the user's setting.
    let ours = |schema: &str, key: &str, v: &str| {
        values
            .iter()
            .any(|(s, k, x)| *s == schema && *k == key && x == v)
    };
    if ours("org.gnome.system.proxy", "mode", "'manual'")
        && ours("org.gnome.system.proxy.http", "host", "'127.0.0.1'")
        && ours("org.gnome.system.proxy.http", "port", &port.to_string())
    {
        values[0].2 = "'none'".to_owned();
    }
    Saved(values)
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

/// Run `gsettings <args>`; returns stdout on success. Values are GVariant
/// text (`'none'`, `7890`), which `gsettings set` accepts back as is.
async fn gsettings(args: &[&str]) -> Option<String> {
    // Capture stderr: without dconf, gsettings warns on every call.
    match Command::new("gsettings").args(args).output().await {
        Ok(out) if out.status.success() && out.stderr.is_empty() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        }
        Ok(out) => {
            tracing::debug!(
                "gsettings {args:?}: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            None
        }
        Err(e) => {
            tracing::debug!("gsettings unavailable: {e}");
            None
        }
    }
}
