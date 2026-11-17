//! The daemon's state machine: build the config, run mihomo, set the proxy.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use tokio::sync::watch;
use vpnx_core::api::Api;
use vpnx_core::device::Device;
use vpnx_core::merge::{self, Forced, Layer};
use vpnx_core::mihomo::{self, Launch, ProcState, Supervisor};
use vpnx_core::overlay;
use vpnx_core::paths::{write_atomic, Paths};
use vpnx_core::settings::Settings;
use vpnx_core::subscription::Meta;
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
    /// JSON with the subscription's title, traffic, expiry and update times.
    pub subscription: String,
    /// JSON list of remote overlays: url, label, updated_at, error.
    pub overlays: String,
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
    /// Why the last subscription download failed.
    sub_error: Option<String>,
    /// Unix time of the last automatic download attempt.
    last_auto_update: Option<u64>,
    /// Why the last download of a remote overlay failed, by URL.
    overlay_errors: HashMap<String, String>,
}

/// When neither the user nor the server says how often to update.
const DEFAULT_UPDATE_HOURS: u32 = 24;
/// Don't hammer a server that is down or unreachable.
const RETRY_AFTER_FAILURE_SECS: u64 = 30 * 60;

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
            subscription: String::new(),
            overlays: String::new(),
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
            sub_error: None,
            last_auto_update: None,
            overlay_errors: HashMap::new(),
        };
        engine.refresh_binary(&settings);
        engine.publish_subscription();
        engine.publish_overlays();
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
        let result = self.download(url).await;
        if result.is_ok() {
            write_atomic(&self.paths.subscription_url(), url.as_bytes())?;
        }
        self.publish_subscription();
        result?;
        self.fetch_overlays().await;
        self.after_subscription_change().await
    }

    pub async fn update_subscription(&mut self) -> Result<()> {
        let url = self
            .subscription_url()
            .context("no subscription set; use `conchy-vpnx subscription set <url>`")?;
        let result = self.download(&url).await;
        self.publish_subscription();
        result?;
        self.fetch_overlays().await;
        self.after_subscription_change().await
    }

    /// Download and save the profile and what the server says about it.
    async fn download(&mut self, url: &str) -> Result<()> {
        let result = async {
            let meta =
                subscription::update(url, &self.paths.subscription(), &self.device()?).await?;
            meta.save(&self.paths.subscription_meta())
        }
        .await;
        self.sub_error = result.as_ref().err().map(|e| format!("{e:#}"));
        result
    }

    /// Update the subscription when its interval is up. Called periodically.
    pub async fn auto_update_subscription(&mut self) {
        if !self.subscription_due() {
            return;
        }
        self.last_auto_update = Some(subscription::now());
        match self.update_subscription().await {
            Ok(()) => tracing::info!("subscription updated"),
            Err(e) => tracing::warn!("subscription update failed, keeping the old one: {e:#}"),
        }
    }

    fn subscription_due(&self) -> bool {
        if self.subscription_url().is_none() {
            return false;
        }
        let now = subscription::now();
        if self
            .last_auto_update
            .is_some_and(|t| now.saturating_sub(t) < RETRY_AFTER_FAILURE_SECS)
        {
            return false;
        }
        self.next_update(Meta::load(&self.paths.subscription_meta()).as_ref())
            .is_some_and(|next| now >= next)
    }

    /// `None` when auto-update is off.
    fn next_update(&self, meta: Option<&Meta>) -> Option<u64> {
        let hours = Settings::load(&self.paths.settings)
            .ok()
            .and_then(|s| s.subscription_update_hours)
            .or_else(|| meta.and_then(|m| m.interval_hours))
            .unwrap_or(DEFAULT_UPDATE_HOURS);
        (hours > 0).then(|| meta.map_or(0, |m| m.updated_at) + u64::from(hours) * 3600)
    }

    /// Add a remote overlay; it is downloaded first so a bad link is
    /// refused instead of saved.
    pub async fn add_overlay(&mut self, url: &str) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            bail!("empty overlay URL");
        }
        overlay::fetch(url, &overlay::cache_path(&self.paths.overlay_cache(), url))
            .await
            .with_context(|| format!("overlay {}", overlay::label(url)))?;
        self.overlay_errors.remove(url);
        if !Settings::add_overlay(&self.paths.settings, url)? {
            bail!("this overlay is already added");
        }
        self.publish_overlays();
        self.reload().await
    }

    pub async fn remove_overlay(&mut self, url: &str) -> Result<()> {
        if !Settings::remove_overlay(&self.paths.settings, url)? {
            bail!("no such overlay; see `conchy-vpnx overlay list`");
        }
        let _ = std::fs::remove_file(overlay::cache_path(&self.paths.overlay_cache(), url));
        self.overlay_errors.remove(url);
        self.publish_overlays();
        self.reload().await
    }

    /// Download every remote overlay again and apply them.
    pub async fn update_overlays(&mut self) -> Result<()> {
        self.fetch_overlays().await;
        let failed: Vec<String> = self
            .overlay_errors
            .iter()
            .map(|(url, e)| format!("{}: {e}", overlay::label(url)))
            .collect();
        self.reload().await?;
        if !failed.is_empty() {
            bail!("kept the previous copy of {}", failed.join("; "));
        }
        Ok(())
    }

    /// Refresh all cached overlays; failures keep the old copy.
    async fn fetch_overlays(&mut self) {
        let urls = Settings::load(&self.paths.settings)
            .map(|s| s.overlays)
            .unwrap_or_default();
        for url in urls {
            self.fetch_overlay(&url).await;
        }
        self.publish_overlays();
    }

    async fn fetch_overlay(&mut self, url: &str) {
        let dest = overlay::cache_path(&self.paths.overlay_cache(), url);
        match overlay::fetch(url, &dest).await {
            Ok(()) => {
                self.overlay_errors.remove(url);
            }
            Err(e) => {
                tracing::warn!("overlay {}: {e:#}", overlay::label(url));
                self.overlay_errors.insert(url.to_owned(), format!("{e:#}"));
            }
        }
    }

    fn publish_overlays(&self) {
        let urls = Settings::load(&self.paths.settings)
            .map(|s| s.overlays)
            .unwrap_or_default();
        let list: Vec<Json> = urls
            .iter()
            .map(|url| {
                let cache = overlay::cache_path(&self.paths.overlay_cache(), url);
                let updated_at = std::fs::metadata(&cache)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                json!({
                    "url": url,
                    "label": overlay::label(url),
                    "updated_at": updated_at,
                    "error": self.overlay_errors.get(url),
                })
            })
            .collect();
        let json = Json::Array(list).to_string();
        self.status.send_if_modified(|s| {
            let changed = s.overlays != json;
            s.overlays = json;
            changed
        });
    }

    fn subscription_url(&self) -> Option<String> {
        let url = std::fs::read_to_string(self.paths.subscription_url()).ok()?;
        let url = url.trim();
        (!url.is_empty()).then(|| url.to_owned())
    }

    fn publish_subscription(&self) {
        let url = self.subscription_url();
        let meta = Meta::load(&self.paths.subscription_meta());
        let next = url.as_ref().and(self.next_update(meta.as_ref()));
        let meta = meta.unwrap_or_default();
        let json = json!({
            "configured": url.is_some(),
            "host": url.as_deref().and_then(subscription::display_host),
            "title": meta.title,
            "upload": meta.upload,
            "download": meta.download,
            "total": meta.total,
            "expire": meta.expire,
            "updated_at": (meta.updated_at > 0).then_some(meta.updated_at),
            "next_update": next,
            "error": self.sub_error,
        })
        .to_string();
        self.status.send_if_modified(|s| {
            let changed = s.subscription != json;
            s.subscription = json;
            changed
        });
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
        let mut layers = Vec::new();
        let mut notes = Vec::new();
        for url in &settings.overlays {
            let cache = overlay::cache_path(&self.paths.overlay_cache(), url);
            if !cache.exists() {
                // Added by editing vpnx.kdl: fetch it on first use.
                self.fetch_overlay(url).await;
            }
            let label = overlay::label(url);
            match std::fs::read_to_string(&cache)
                .map_err(anyhow::Error::from)
                .and_then(|t| overlay::parse(&t))
            {
                Ok(value) => layers.push(Layer { name: label, value }),
                Err(_) => notes.push(format!(
                    "remote overlay {label} is not available yet, skipped: {}",
                    self.overlay_errors
                        .get(url)
                        .map_or("not downloaded", String::as_str)
                )),
            }
        }
        match std::fs::read_to_string(self.paths.overlay()) {
            Ok(t) => layers.push(Layer {
                name: "overlay.yaml".into(),
                value: serde_yaml::from_str(&t).context("overlay.yaml is not valid YAML")?,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reading overlay.yaml"),
        }
        self.publish_overlays();
        let forced = Forced {
            controller_socket: self.paths.controller_socket(),
            mixed_port: settings.mixed_port,
            tun: effective == Mode::Tun,
        };
        let merged = merge::merge(sub, layers, &forced)?;
        let warnings: Vec<String> = self
            .notice
            .iter()
            .cloned()
            .chain(notes)
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
