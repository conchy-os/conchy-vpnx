//! The daemon's state machine: build the config, run mihomo, set the proxy.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use tokio::sync::watch;
use vpnx_core::api::Api;
use vpnx_core::device::Device;
use vpnx_core::merge::{self, Forced};
use vpnx_core::mihomo::{self, Launch, ProcState, Supervisor};
use vpnx_core::paths::{write_atomic, Paths};
use vpnx_core::settings::Settings;
use vpnx_core::{caps, conflict, subscription, Mode};

use crate::proxy_env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Down,
    Starting,
    Up,
    Restarting,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Starting => "starting",
            Self::Up => "up",
            Self::Restarting => "restarting",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub mode: Mode,
    pub effective: Option<Mode>,
    pub error: String,
    pub warnings: Vec<String>,
    /// mihomo may create a TUN device (root or file capabilities).
    pub can_tun: bool,
    /// The mihomo binary in use, empty when none was found.
    pub mihomo: String,
}

pub struct Engine {
    paths: Paths,
    bus: zbus::Connection,
    status: Arc<watch::Sender<Status>>,
    proc: Arc<watch::Sender<ProcState>>,
    supervisor: Option<Supervisor>,
    /// Mode and port the running mihomo was started with.
    running: Option<(Mode, u16)>,
    /// Why `auto` fell back to proxy, shown with the merge warnings.
    notice: Option<String>,
    /// The user's GNOME proxy settings, restored when proxy mode stops.
    saved_proxy: Option<proxy_env::Saved>,
}

impl Engine {
    pub fn new(paths: Paths, bus: zbus::Connection) -> Result<Self> {
        let settings = Settings::load(&paths.settings)?;
        let status = Status {
            state: State::Down,
            mode: settings.mode,
            effective: None,
            error: String::new(),
            warnings: Vec::new(),
            can_tun: false,
            mihomo: String::new(),
        };
        let engine = Self {
            paths,
            bus,
            status: Arc::new(watch::channel(status).0),
            proc: Arc::new(watch::channel(ProcState::Stopped).0),
            supervisor: None,
            running: None,
            notice: None,
            saved_proxy: None,
        };
        engine.refresh_binary(&settings);
        Ok(engine)
    }

    pub fn autoconnect(&self) -> bool {
        Settings::load(&self.paths.settings)
            .map(|s| s.autoconnect)
            .unwrap_or(false)
    }

    /// Re-check which mihomo is used and whether it may run TUN. Capabilities
    /// change behind our back (`setcap`, or lost on a package upgrade).
    fn refresh_binary(&self, settings: &Settings) {
        let bin = settings.mihomo_bin().ok();
        let can_tun = bin.as_deref().is_some_and(caps::can_tun);
        let mihomo = bin
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status.send_if_modified(|s| {
            let changed = s.can_tun != can_tun || s.mihomo != mihomo;
            s.can_tun = can_tun;
            s.mihomo = mihomo;
            changed
        });
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    /// Follow crashes and restarts of mihomo.
    pub fn spawn_proc_watcher(&self) {
        let mut rx = self.proc.subscribe();
        let status = self.status.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let proc = rx.borrow_and_update().clone();
                status.send_if_modified(|s| {
                    let prev = s.clone();
                    match proc {
                        ProcState::Running if s.state == State::Restarting => s.state = State::Up,
                        ProcState::Restarting if s.state == State::Up => {
                            s.state = State::Restarting
                        }
                        ProcState::Failed(ref e) => {
                            s.state = State::Error;
                            s.error = e.clone();
                        }
                        _ => {}
                    }
                    *s != prev
                });
            }
        });
    }

    fn is_running(&self) -> bool {
        self.supervisor.is_some()
            && matches!(
                *self.proc.borrow(),
                ProcState::Running | ProcState::Restarting
            )
    }

    pub async fn up(&mut self) -> Result<()> {
        if self.is_running() {
            return self.reload().await;
        }
        self.fresh_start().await
    }

    async fn fresh_start(&mut self) -> Result<()> {
        self.set_state(State::Starting, None);
        let result = self.start().await;
        if let Err(e) = &result {
            self.stop_process().await;
            self.status.send_modify(|s| {
                s.state = State::Error;
                s.effective = None;
                s.error = format!("{e:#}");
            });
        }
        result
    }

    async fn start(&mut self) -> Result<()> {
        if let Some(old) = self.supervisor.take() {
            old.stop().await;
        }
        let settings = Settings::load(&self.paths.settings)?;
        self.refresh_binary(&settings);
        let bin = settings.mihomo_bin()?;
        let effective = self.effective_mode(&settings, &bin).await?;
        self.build_config(&settings, &bin, effective).await?;

        let launch = Launch {
            bin,
            home: self.paths.mihomo_home(),
            config: self.paths.mihomo_config(),
            socket: self.paths.controller_socket(),
        };
        self.supervisor = Some(Supervisor::start(launch, self.proc.clone()).await?);
        self.running = Some((effective, settings.mixed_port));
        if effective == Mode::Proxy {
            self.saved_proxy = Some(proxy_env::set(&self.bus, settings.mixed_port).await);
        }
        self.status.send_modify(|s| {
            s.state = State::Up;
            s.mode = settings.mode;
            s.effective = Some(effective);
            s.error.clear();
        });
        Ok(())
    }

    pub async fn down(&mut self) -> Result<()> {
        self.stop_process().await;
        self.set_state(State::Down, None);
        Ok(())
    }

    async fn stop_process(&mut self) {
        if let Some(sup) = self.supervisor.take() {
            sup.stop().await;
        }
        if let Some((Mode::Proxy, _)) = self.running.take() {
            proxy_env::clear(&self.bus, self.saved_proxy.take()).await;
        }
    }

    /// Rebuild the config and apply it; restarts when mode or port changed.
    pub async fn reload(&mut self) -> Result<()> {
        let settings = Settings::load(&self.paths.settings)?;
        self.status.send_modify(|s| s.mode = settings.mode);
        self.refresh_binary(&settings);
        let bin = settings.mihomo_bin()?;
        let effective = self.effective_mode(&settings, &bin).await?;

        if !self.is_running() {
            // Validate only, so errors show up before the user hits "up".
            return self.build_config(&settings, &bin, effective).await;
        }
        if self.running != Some((effective, settings.mixed_port)) {
            self.stop_process().await;
            return self.fresh_start().await;
        }
        self.build_config(&settings, &bin, effective).await?;
        Api::new(self.paths.controller_socket())
            .reload(&self.paths.mihomo_config())
            .await
    }

    pub async fn set_mode(&mut self, mode: Mode) -> Result<()> {
        Settings::save_mode(&self.paths.settings, mode)?;
        self.status.send_modify(|s| s.mode = mode);
        if self.is_running() {
            self.reload().await?;
        }
        Ok(())
    }

    pub async fn set_subscription(&mut self, url: &str) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            bail!("empty subscription URL");
        }
        subscription::update(url, &self.paths.subscription(), &self.device()?).await?;
        write_atomic(&self.paths.subscription_url(), url.as_bytes())?;
        self.after_subscription_change().await
    }

    pub async fn update_subscription(&mut self) -> Result<()> {
        let url = std::fs::read_to_string(self.paths.subscription_url())
            .context("no subscription set; use `conchy-vpnx subscription set <url>`")?;
        subscription::update(url.trim(), &self.paths.subscription(), &self.device()?).await?;
        self.after_subscription_change().await
    }

    fn device(&self) -> Result<Device> {
        let settings = Settings::load(&self.paths.settings)?;
        Ok(Device::detect(settings.hwid.as_deref()))
    }

    async fn after_subscription_change(&mut self) -> Result<()> {
        if self.is_running() {
            self.reload().await
        } else {
            Ok(())
        }
    }

    pub async fn groups(&self) -> Result<Json> {
        let proxies = self.api()?.proxies().await?;
        let all = &proxies["proxies"];
        // GLOBAL lists groups in config order; the map itself is unordered.
        let order: Vec<&str> = all["GLOBAL"]["all"]
            .as_array()
            .map(|a| a.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default();
        let groups = order
            .into_iter()
            .chain(std::iter::once("GLOBAL"))
            .filter_map(|name| {
                let p = &all[name];
                p["all"].as_array()?;
                Some(json!({
                    "name": name,
                    "type": p["type"],
                    "now": p["now"],
                    "all": p["all"],
                }))
            })
            .collect();
        Ok(Json::Array(groups))
    }

    pub async fn select(&self, group: &str, name: &str) -> Result<()> {
        self.api()?.select(group, name).await
    }

    /// API handle for calls that should not hold the engine lock.
    pub fn api(&self) -> Result<Api> {
        if !self.is_running() {
            bail!("VPN is down");
        }
        Ok(Api::new(self.paths.controller_socket()))
    }

    /// Merge, validate with `mihomo -t`, then swap the config in, keeping the
    /// previous working one.
    async fn build_config(
        &mut self,
        settings: &Settings,
        bin: &std::path::Path,
        effective: Mode,
    ) -> Result<()> {
        let sub_path = self.paths.subscription();
        let sub_text = std::fs::read_to_string(&sub_path)
            .context("no subscription yet; use `conchy-vpnx subscription set <url>`")?;
        let sub = subscription::parse(&sub_text)?;
        let overlay = match std::fs::read_to_string(self.paths.overlay()) {
            Ok(t) => Some(serde_yaml::from_str(&t).context("overlay.yaml is not valid YAML")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading overlay.yaml"),
        };
        let forced = Forced {
            controller_socket: self.paths.controller_socket(),
            mixed_port: settings.mixed_port,
            tun: effective == Mode::Tun,
        };
        let merged = merge::merge(sub, overlay, &forced)?;
        let warnings: Vec<String> = self
            .notice
            .iter()
            .cloned()
            .chain(merged.warnings.iter().map(ToString::to_string))
            .collect();
        for w in &warnings {
            tracing::warn!("merge: {w}");
        }

        let home = self.paths.mihomo_home();
        let config = self.paths.mihomo_config();
        let candidate = home.join("config.new.yaml");
        write_atomic(
            &candidate,
            serde_yaml::to_string(&merged.config)?.as_bytes(),
        )?;
        if let Err(e) = mihomo::validate(bin, &home, &candidate).await {
            let _ = std::fs::remove_file(&candidate);
            return Err(e);
        }
        if config.exists() {
            std::fs::copy(&config, self.paths.mihomo_config_prev())?;
        }
        std::fs::rename(&candidate, &config)?;
        self.status.send_modify(|s| s.warnings = warnings);
        Ok(())
    }

    fn set_state(&self, state: State, effective: Option<Mode>) {
        self.status.send_modify(|s| {
            s.state = state;
            s.effective = effective;
            if state != State::Error {
                s.error.clear();
            }
        });
    }

    /// Resolve `auto`, and refuse TUN while another VPN holds all traffic.
    async fn effective_mode(&mut self, settings: &Settings, bin: &std::path::Path) -> Result<Mode> {
        self.notice = None;
        let can_tun = caps::can_tun(bin);
        let wants_tun = match settings.mode {
            Mode::Proxy => return Ok(Mode::Proxy),
            Mode::Tun if !can_tun => bail!(
                "TUN mode needs CAP_NET_ADMIN for mihomo; run `conchy-vpnx tun enable` or: {}",
                caps::grant_hint(bin)
            ),
            Mode::Tun => true,
            Mode::Auto => can_tun,
        };
        if !wants_tun {
            tracing::info!(
                "no CAP_NET_ADMIN, using proxy mode; run `conchy-vpnx tun enable` for TUN"
            );
            return Ok(Mode::Proxy);
        }
        // Our own TUN is up: its routes would look like a foreign one.
        if matches!(self.running, Some((Mode::Tun, _))) && self.is_running() {
            return Ok(Mode::Tun);
        }
        let Some(dev) = conflict::foreign_tunnel().await else {
            return Ok(Mode::Tun);
        };
        if settings.mode == Mode::Tun {
            bail!(
                "another VPN already sends all traffic through `{dev}`; \
                 turn off its TUN mode or use proxy mode"
            );
        }
        let notice = format!(
            "another VPN already sends all traffic through `{dev}`, using proxy mode instead of TUN"
        );
        tracing::warn!("{notice}");
        self.notice = Some(notice);
        Ok(Mode::Proxy)
    }
}
