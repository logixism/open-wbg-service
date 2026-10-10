use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    api::{self, ApiState},
    applications::{self, Identity},
    compositor::{self, Backend, Focus},
    config::{Config, Store},
    discord,
    hid::{self, Device, DeviceInfo, Profile, ProfileId},
    linking::LinkState,
    metrics::{DataPoint, Sampler},
    proto::Subscriptions,
    telemetry,
};

pub struct Options {
    pub config_path: PathBuf,
    pub backend: Option<Backend>,
    pub dry_run: bool,
    pub no_api: bool,
    pub duration: Option<u64>,
    pub log_dir: PathBuf,
}

/// Lifetime ownership for the daemon and manual profile switches.
pub fn device_lock() -> Result<File> {
    let file = lock_file("open-wbg-service.lock")?;
    fs2::FileExt::try_lock_exclusive(&file).context("another open-wbg-service instance owns the keyboard; stop it before switching profiles or starting a second daemon")?;
    Ok(file)
}

fn lock_file(name: &str) -> Result<File> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; run in your desktop user session")?;
    let path = Path::new(&runtime).join(name);
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?)
}

/// Serialize complete HID read/write batches, not the daemon's idle time.
/// Keep the descriptor open between batches; Wootility is a separate HID client.
pub struct HardwareLock(File);

impl HardwareLock {
    pub fn open() -> Result<Self> {
        Ok(Self(lock_file("open-wbg-service-hid.lock")?))
    }

    pub fn lock(&mut self) -> Result<HardwareGuard<'_>> {
        fs2::FileExt::lock_exclusive(&self.0).context("waiting for keyboard HID access")?;
        Ok(HardwareGuard(&self.0))
    }
}

pub struct HardwareGuard<'a>(&'a File);

impl Drop for HardwareGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = fs2::FileExt::unlock(self.0) {
            tracing::error!(%error, "could not release keyboard HID access");
        }
    }
}

struct Tracked {
    device: Device,
    profiles: Vec<Profile>,
    link: LinkState,
    current: ProfileId,
    catalog_at: Instant,
    subscriptions: Option<Subscriptions>,
    telemetry_supported: bool,
    telemetry_error: Option<String>,
    error: Option<String>,
}

impl Tracked {
    fn open(info: &DeviceInfo, config: &Config) -> Result<Self> {
        let device = Device::open(info)?;
        let current = device.current_profile()?;
        let profiles = device.profiles()?;
        let mut link = LinkState::new(current, config.fallback_profile);
        // Adopt a pre-existing firmware link, e.g. after a previous daemon crashed.
        // If Mode selected an onboard profile, it becomes the return profile.
        link.target = device.linked_profile()?;
        let mut tracked = Self {
            device,
            profiles,
            link,
            current,
            catalog_at: Instant::now(),
            subscriptions: None,
            telemetry_supported: true,
            telemetry_error: None,
            error: None,
        };
        tracked.refresh_subscriptions();
        Ok(tracked)
    }

    fn refresh_subscriptions(&mut self) {
        if !self.telemetry_supported {
            return;
        }
        match telemetry::subscriptions(&self.device) {
            Ok(Some(subscriptions)) => {
                self.subscriptions = Some(subscriptions);
                self.telemetry_error = None;
            }
            Ok(None) => {
                self.telemetry_supported = false;
                self.subscriptions = None;
                tracing::info!(device = %self.device.info.product, "firmware has no external lightbar telemetry; app linking remains enabled");
            }
            Err(error) => {
                self.subscriptions = None; // Never keep a stale slot mapping after a failed refresh.
                let error = format!("{error:#}");
                if self.telemetry_error.as_ref() != Some(&error) {
                    tracing::warn!(device = %self.device.info.product, %error, "cannot refresh telemetry subscriptions");
                }
                self.telemetry_error = Some(error);
            }
        }
    }

    fn desired(&self, identity: Option<&Identity>, config: &Config) -> Result<Option<ProfileId>> {
        let Some(identity) = identity else {
            return Ok(None);
        };
        for rule in &config.rules {
            if let Some(id) = rule.matches(identity, &self.device.info.serial) {
                if !self.profiles.iter().any(|profile| profile.id == id) {
                    bail!(
                        "configured profile {}:{} is not stored on {}",
                        id.namespace,
                        id.index,
                        self.device.info.product
                    );
                }
                return Ok(Some(id));
            }
        }
        Ok(self
            .profiles
            .iter()
            .find(|profile| {
                profile.id.namespace == 1 && profile.apps.iter().any(|app| identity.matches(app))
            })
            .map(|p| p.id))
    }

    fn transition(&mut self, desired: Option<ProfileId>, dry_run: bool) -> Result<Option<String>> {
        let Some(plan) = self.link.plan(self.current, desired) else {
            return Ok(None);
        };
        if !dry_run {
            if let Some(selection) = plan.selection {
                self.device.select_linked(selection)?;
            }
            if let Some(profile) = plan.activate {
                self.device.activate(profile)?;
            }
            self.current = self.device.current_profile()?;
        }
        self.link = plan.next;
        let name = desired
            .and_then(|id| self.profiles.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("Onboard P{}", self.link.baseline.index + 1));
        tracing::info!(device = %self.device.info.product, profile = %name, ?desired, dry_run, "profile transition");
        if !dry_run {
            self.refresh_subscriptions();
        }
        Ok(Some(format!("{}: {name}", self.device.info.product)))
    }

    fn step(
        &mut self,
        identity: Option<&Identity>,
        config: &Config,
        points: &[DataPoint],
        tick: bool,
        dry_run: bool,
    ) -> Result<Option<String>> {
        let current = self.device.current_profile()?;
        let changed = current != self.current;
        self.current = current;
        if self.catalog_at.elapsed() >= Duration::from_secs(config.catalog_refresh_secs) {
            self.profiles = self.device.profiles()?;
            self.catalog_at = Instant::now();
            self.refresh_subscriptions();
        } else if changed {
            self.refresh_subscriptions();
        }
        let desired = self.desired(identity, config)?;
        let notification = self.transition(desired, dry_run)?;
        if tick
            && !dry_run
            && let Some(subscriptions) = &self.subscriptions
            && let Err(error) = telemetry::update(&self.device, subscriptions, points)
        {
            let error = format!("{error:#}");
            if self.telemetry_error.as_ref() != Some(&error) {
                tracing::warn!(%error, "keyboard telemetry update failed");
            }
            self.telemetry_error = Some(error);
        }
        Ok(notification)
    }

    fn restore(&mut self, config: &Config, dry_run: bool) {
        if !config.restore_on_exit || dry_run {
            return;
        }
        let result = self.device.current_profile().and_then(|current| {
            self.current = current;
            self.transition(None, false).map(|_| ())
        });
        if let Err(error) = result {
            tracing::error!(device = %self.device.info.product, %error, "could not restore automatic profile");
        }
    }

    fn status(&self) -> Value {
        json!({"device":self.device.info, "current":self.current, "automatic_target":self.link.target,
            "return_profile":self.link.baseline, "profiles":self.profiles, "error":self.error,
            "telemetry_supported":self.telemetry_supported,"telemetry_error":self.telemetry_error,
            "subscriptions":self.subscriptions.as_ref().map(|s| s.subscriptions.iter().map(|s| json!({"index":s.index,"hash":s.hash})).collect::<Vec<_>>())})
    }
}

struct HardwareChannels {
    config: watch::Receiver<Arc<Config>>,
    focus: watch::Receiver<Focus>,
    points: watch::Receiver<Vec<DataPoint>>,
    shutdown: watch::Receiver<bool>,
    status: watch::Sender<Value>,
    notify: tokio::sync::mpsc::Sender<String>,
}

fn hardware(
    mut channels: HardwareChannels,
    wake: mpsc::Receiver<()>,
    backend: Backend,
    dry_run: bool,
) -> Result<()> {
    let mut access = HardwareLock::open()?;
    let mut devices: HashMap<String, Tracked> = HashMap::new();
    let mut failures: HashMap<String, String> = HashMap::new();
    let mut next_scan = Instant::now();
    let mut next_tick = Instant::now();
    let mut previous_focus = Focus::default();
    let mut identity = None;
    let mut enumeration_error = None;
    loop {
        if *channels.shutdown.borrow() {
            break;
        }
        let config_changed = channels.config.has_changed().unwrap_or(false);
        let config = channels.config.borrow_and_update().clone();
        let focus = channels.focus.borrow().clone();
        let focus_changed = focus != previous_focus;
        if focus_changed {
            identity = focus.window.as_ref().map(applications::identify);
            previous_focus = focus.clone();
        }
        let now = Instant::now();
        let tick = now >= next_tick;
        if tick {
            next_tick = now + Duration::from_millis(config.interval_ms);
        }
        let scan = now >= next_scan || config_changed;
        let update = tick || focus_changed || config_changed || scan;
        let io_guard = if update { Some(access.lock()?) } else { None };
        if scan {
            next_scan = now + Duration::from_secs(3);
            match hid::enumerate() {
                Ok(found) => {
                    enumeration_error = None;
                    let found: Vec<_> = found
                        .into_iter()
                        .filter(|d| config.serials.is_empty() || config.serials.contains(&d.serial))
                        .collect();
                    devices.retain(|path, device| {
                        if found.iter().any(|info| &info.path == path) {
                            true
                        } else {
                            device.restore(&config, dry_run);
                            false
                        }
                    });
                    failures.retain(|path, _| found.iter().any(|info| &info.path == path));
                    for info in found {
                        if devices.contains_key(&info.path) {
                            continue;
                        }
                        match Tracked::open(&info, &config) {
                            Ok(device) => {
                                tracing::info!(device = %info.product, profiles = device.profiles.len(), "keyboard connected");
                                failures.remove(&info.path);
                                devices.insert(info.path.clone(), device);
                            }
                            Err(error) => {
                                let error = format!("{error:#}");
                                if failures.get(&info.path) != Some(&error) {
                                    tracing::warn!(path = %info.path, %error, "keyboard unavailable");
                                }
                                failures.insert(info.path, error);
                            }
                        }
                    }
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    if enumeration_error.as_ref() != Some(&error) {
                        tracing::warn!(%error, "cannot enumerate keyboards");
                    }
                    enumeration_error = Some(error);
                }
            }
        }
        if update {
            let points = channels.points.borrow();
            for device in devices.values_mut() {
                if *channels.shutdown.borrow() {
                    break;
                }
                match device.step(identity.as_ref(), &config, &points, tick, dry_run) {
                    Ok(notification) => {
                        device.error = None;
                        if config.notify
                            && !dry_run
                            && let Some(notification) = notification
                        {
                            let _ = channels.notify.try_send(notification);
                        }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if device.error.as_ref() != Some(&error) {
                            tracing::warn!(%error, "keyboard operation failed; will retry without committing transition");
                        }
                        device.error = Some(error);
                    }
                }
            }
            let mut device_status: Vec<_> = devices.values().map(Tracked::status).collect();
            device_status.sort_by(|a, b| {
                a["device"]["path"]
                    .as_str()
                    .cmp(&b["device"]["path"].as_str())
            });
            channels.status.send_replace(
                json!({"service":"open-wbg-service","version":env!("CARGO_PKG_VERSION"),
                "backend":backend,"dry_run":dry_run,"focus":focus,"devices":device_status,
                "unavailable_devices":failures,"enumeration_error":enumeration_error}),
            );
        }
        drop(io_guard);
        let wait = next_tick
            .min(next_scan)
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        if wake
            .recv_timeout(wait)
            .is_err_and(|error| matches!(error, mpsc::RecvTimeoutError::Disconnected))
        {
            break;
        }
    }
    if !devices.is_empty() {
        let _io = access.lock()?;
        let config = channels.config.borrow().clone();
        for device in devices.values_mut() {
            device.restore(&config, dry_run);
        }
    }
    Ok(())
}

fn same_inventory(a: &[DataPoint], b: &[DataPoint]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.path == b.path
                && a.name == b.name
                && a.source == b.source
                && a.min == b.min
                && a.max == b.max
                && std::mem::discriminant(&a.value) == std::mem::discriminant(&b.value)
        })
}

pub async fn run(options: Options) -> Result<()> {
    let _lock = device_lock()?;
    let config = Config::load(&options.config_path)?;
    let backend = options.backend.unwrap_or(config.backend).resolve()?;
    // Bind before touching the keyboard: a proprietary daemon already on the port
    // must cause startup to fail, not two competing HID controllers.
    let listener = if config.api && !options.no_api {
        Some(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.api_port)).await
            .context("cannot bind local Wootility API; stop the proprietary background service or choose another api_port")?)
    } else {
        None
    };
    let (settings_tx, mut settings_rx) = watch::channel(Arc::new(config));
    let settings = Arc::new(Mutex::new(Store {
        path: options.config_path,
        tx: settings_tx,
    }));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (focus_tx, mut focus_rx) = watch::channel(Focus::default());
    let (points_tx, mut points_rx) = watch::channel(Vec::new());
    let (discord_tx, discord_rx) = watch::channel(Vec::new());
    let (inventory_tx, inventory_rx) = watch::channel(0u64);
    let (status_tx, status_rx) =
        watch::channel(json!({"service":"open-wbg-service","starting":true}));
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let worker_channels = HardwareChannels {
        config: settings_rx.clone(),
        focus: focus_rx.clone(),
        points: points_rx.clone(),
        shutdown: shutdown_rx.clone(),
        status: status_tx,
        notify: notify_tx,
    };
    let hardware = thread::Builder::new()
        .name("wooting-hid".into())
        .spawn(move || hardware(worker_channels, wake_rx, backend, options.dry_run))?;
    let sample_config = settings_rx.clone();
    let sample_stop = shutdown_rx.clone();
    let metrics = thread::Builder::new()
        .name("wooting-metrics".into())
        .spawn(move || {
            let mut sampler = Sampler::new();
            while !*sample_stop.borrow() {
                let started = Instant::now();
                let config = sample_config.borrow().clone();
                let mut points = sampler.sample(&config.enabled_sources);
                if config.enabled_sources.contains("discord_source") {
                    points.extend(discord_rx.borrow().iter().cloned());
                }
                let changed = !same_inventory(&points_tx.borrow(), &points);
                points_tx.send_replace(points);
                // Subscribers fetch the inventory after this notification.
                if changed {
                    inventory_tx.send_modify(|revision| *revision = revision.wrapping_add(1));
                }
                let delay =
                    Duration::from_millis(config.interval_ms).saturating_sub(started.elapsed());
                let until = Instant::now() + delay;
                while Instant::now() < until && !*sample_stop.borrow() {
                    thread::sleep(
                        until
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(100)),
                    );
                }
            }
        })?;
    let compositor = tokio::spawn(compositor::run(backend, focus_tx));
    let mut discord = tokio::spawn(discord::run(
        settings_rx.clone(),
        discord_tx,
        shutdown_rx.clone(),
    ));
    let notify = tokio::spawn(async move {
        while let Some(message) = notify_rx.recv().await {
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::process::Command::new("notify-send")
                    .args([
                        "--app-name=open-wbg-service",
                        "--",
                        "Keyboard profile",
                        &message,
                    ])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            match result {
                Ok(Ok(output)) if output.status.success() => {}
                _ => tracing::warn!(
                    "could not display profile notification; install notify-send and a notification daemon"
                ),
            }
        }
    });
    let mut api_task = listener.map(|listener| {
        tokio::spawn(api::serve(
            listener,
            ApiState {
                settings: settings.clone(),
                backend,
                points: points_rx.clone(),
                status: status_rx,
                inventory_changed: inventory_rx,
                log_dir: options.log_dir,
            },
            shutdown_rx.clone(),
        ))
    });
    tracing::info!(
        ?backend,
        dry_run = options.dry_run,
        api = api_task.is_some(),
        "open-wbg-service ready"
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let deadline = options
        .duration
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut health = tokio::time::interval(Duration::from_millis(250));
    let mut result = Ok(());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
            _ = hup.recv() => {
                match settings.lock().expect("settings mutex poisoned").reload() {
                    Ok(()) => tracing::info!("configuration reloaded"),
                    Err(error) => tracing::error!(%error,"configuration rejected; keeping previous settings"),
                }
            }
            changed = focus_rx.changed() => { if changed.is_err() { result = Err(anyhow::anyhow!("compositor task exited")); break; } let _ = wake_tx.try_send(()); }
            changed = points_rx.changed() => { if changed.is_err() { result = Err(anyhow::anyhow!("metric worker exited")); break; } let _ = wake_tx.try_send(()); }
            _ = settings_rx.changed() => { let _ = wake_tx.try_send(()); }
            _ = health.tick() => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) { break; }
                if hardware.is_finished() || metrics.is_finished() || discord.is_finished() || api_task.as_ref().is_some_and(|task| task.is_finished()) {
                    result = Err(anyhow::anyhow!("a background worker exited unexpectedly; inspect logs")); break;
                }
            }
        }
    }
    shutdown_tx.send_replace(true);
    let _ = wake_tx.try_send(());
    compositor.abort();
    // HID ownership and restoration finish before releasing the process lock.
    let joined = tokio::task::spawn_blocking(move || hardware.join()).await?;
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => result = Err(error.context("HID worker failed")),
        Err(_) => result = Err(anyhow::anyhow!("HID worker panicked")),
    }
    notify.abort();
    if tokio::time::timeout(Duration::from_secs(3), &mut discord)
        .await
        .is_err()
    {
        discord.abort();
    }
    if let Some(mut task) = api_task.take() {
        match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
            Ok(Ok(Err(error))) => result = Err(error),
            Ok(Err(error)) => result = Err(error.into()),
            Err(_) => task.abort(),
            _ => {}
        }
    }
    let until = Instant::now() + Duration::from_secs(2);
    while !metrics.is_finished() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if metrics.is_finished() {
        let _ = metrics.join();
    } else {
        tracing::warn!("metric worker is still waiting on OS I/O; no HID operations remain");
    }
    tracing::info!("open-wbg-service stopped");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn hid_batches_are_exclusive_and_release_after_errors() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "open-wbg-hid-lock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // Both descriptors retain the inode; no test artifact survives a panic.
        std::fs::remove_file(path).unwrap();
        let mut access = HardwareLock(file);

        for fail in [false, true] {
            let result: Result<()> = (|| {
                let _guard = access.lock()?;
                assert_eq!(
                    fs2::FileExt::try_lock_exclusive(&probe).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                if fail {
                    bail!("interrupted HID batch");
                }
                Ok(())
            })();
            assert_eq!(result.is_err(), fail);
            fs2::FileExt::try_lock_exclusive(&probe).unwrap();
            fs2::FileExt::unlock(&probe).unwrap();
        }
    }
}
