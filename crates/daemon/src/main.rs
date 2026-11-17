mod engine;
mod proxy_env;

use std::sync::Arc;

use anyhow::Result;
use engine::{Engine, Status};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Mutex};
use tracing_subscriber::EnvFilter;
use vpnx_core::paths::Paths;
use vpnx_core::Mode;
use zbus::{connection, fdo, interface};

const DELAY_URL: &str = "https://www.gstatic.com/generate_204";

struct Vpnx {
    engine: Arc<Mutex<Engine>>,
    status: watch::Receiver<Status>,
}

fn failed(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

#[interface(name = "org.conchy.Vpnx")]
impl Vpnx {
    async fn up(&self) -> fdo::Result<()> {
        self.engine.lock().await.up().await.map_err(failed)
    }

    async fn down(&self) -> fdo::Result<()> {
        self.engine.lock().await.down().await.map_err(failed)
    }

    async fn reload(&self) -> fdo::Result<()> {
        self.engine.lock().await.reload().await.map_err(failed)
    }

    async fn set_mode(&self, mode: &str) -> fdo::Result<()> {
        let mode: Mode = mode.parse().map_err(failed)?;
        self.engine
            .lock()
            .await
            .set_mode(mode)
            .await
            .map_err(failed)
    }

    async fn set_subscription(&self, url: &str) -> fdo::Result<()> {
        self.engine
            .lock()
            .await
            .set_subscription(url)
            .await
            .map_err(failed)
    }

    async fn update_subscription(&self) -> fdo::Result<()> {
        self.engine
            .lock()
            .await
            .update_subscription()
            .await
            .map_err(failed)
    }

    async fn groups(&self) -> fdo::Result<String> {
        let groups = self.engine.lock().await.groups().await.map_err(failed)?;
        Ok(groups.to_string())
    }

    async fn select_proxy(&self, group: &str, name: &str) -> fdo::Result<()> {
        self.engine
            .lock()
            .await
            .select(group, name)
            .await
            .map_err(failed)
    }

    async fn delay(&self, proxy: &str) -> fdo::Result<u32> {
        // Don't hold the engine lock across a slow network probe.
        let api = self.engine.lock().await.api().map_err(failed)?;
        api.delay(proxy, DELAY_URL, 5000).await.map_err(failed)
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.status.borrow().state.as_str().into()
    }

    #[zbus(property)]
    async fn mode(&self) -> String {
        self.status.borrow().mode.as_str().into()
    }

    #[zbus(property)]
    async fn effective_mode(&self) -> String {
        self.status
            .borrow()
            .effective
            .map(|m| m.as_str().into())
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.status.borrow().error.clone()
    }

    #[zbus(property)]
    async fn warnings(&self) -> Vec<String> {
        self.status.borrow().warnings.clone()
    }

    #[zbus(property)]
    async fn can_tun(&self) -> bool {
        self.status.borrow().can_tun
    }

    #[zbus(property)]
    async fn mihomo_path(&self) -> String {
        self.status.borrow().mihomo.clone()
    }

    #[zbus(property)]
    async fn subscription(&self) -> String {
        self.status.borrow().subscription.clone()
    }
}

/// How often to check whether the subscription is due.
const SUBSCRIPTION_CHECK: std::time::Duration = std::time::Duration::from_secs(10 * 60);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let paths = Paths::from_env();
    let bus = zbus::Connection::session().await?;
    let engine = Engine::new(paths, bus)?;
    engine.spawn_proc_watcher();
    let mut status = engine.status();
    let engine = Arc::new(Mutex::new(engine));

    let conn = connection::Builder::session()?
        .name(vpnx_client::BUS_NAME)?
        .serve_at(
            vpnx_client::OBJECT_PATH,
            Vpnx {
                engine: engine.clone(),
                status: status.clone(),
            },
        )?
        .build()
        .await?;
    tracing::info!("conchy-vpnxd running as {}", vpnx_client::BUS_NAME);

    // Turn status changes into PropertiesChanged signals.
    let signals = conn.clone();
    tokio::spawn(async move {
        let iface = match signals
            .object_server()
            .interface::<_, Vpnx>(vpnx_client::OBJECT_PATH)
            .await
        {
            Ok(i) => i,
            Err(e) => return tracing::error!("no interface for signals: {e}"),
        };
        let mut last = status.borrow_and_update().clone();
        while status.changed().await.is_ok() {
            let now = status.borrow_and_update().clone();
            let ctx = iface.signal_emitter();
            let i = iface.get().await;
            if now.state != last.state {
                let _ = i.state_changed(ctx).await;
            }
            if now.mode != last.mode {
                let _ = i.mode_changed(ctx).await;
            }
            if now.effective != last.effective {
                let _ = i.effective_mode_changed(ctx).await;
            }
            if now.error != last.error {
                let _ = i.last_error_changed(ctx).await;
            }
            if now.warnings != last.warnings {
                let _ = i.warnings_changed(ctx).await;
            }
            if now.can_tun != last.can_tun {
                let _ = i.can_tun_changed(ctx).await;
            }
            if now.mihomo != last.mihomo {
                let _ = i.mihomo_path_changed(ctx).await;
            }
            if now.subscription != last.subscription {
                let _ = i.subscription_changed(ctx).await;
            }
            last = now;
        }
    });

    if engine.lock().await.autoconnect() {
        let engine = engine.clone();
        tokio::spawn(async move {
            tracing::info!("autoconnect is on, bringing the VPN up");
            if let Err(e) = engine.lock().await.up().await {
                tracing::warn!("autoconnect failed: {e:#}");
            }
        });
    }

    {
        let engine = engine.clone();
        tokio::spawn(async move {
            // Give the network a moment after login before the first check.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                engine.lock().await.auto_update_subscription().await;
                tokio::time::sleep(SUBSCRIPTION_CHECK).await;
            }
        });
    }

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    // Never leave a dead proxy in the session environment.
    let _ = engine.lock().await.down().await;
    Ok(())
}
