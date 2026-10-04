#![cfg_attr(not(target_os = "linux"), allow(unused))]
#[cfg(not(target_os = "linux"))]
compile_error!("open-wbg-service currently targets Linux");

mod api;
mod applications;
mod compositor;
mod config;
mod discord;
mod hid;
mod installation;
mod linking;
mod metrics;
mod proto;
mod runtime;
mod telemetry;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(
    version,
    about = "Independent Wooting background service for Linux",
    long_about = r#"Independent Wooting background service for Linux.

Uses native Niri, Sway or Hyprland IPC and keyboard-resident Wootility app links.
Wootility remains the profile editor; the proprietary background service is not needed.
Only volatile profile selection and subscribed lightbar data are written, never flash.
Modern FF55 keyboards and App Linking-capable firmware (2.14+) are required.

Build/install (Rust toolchain, C compiler, pkg-config and libudev headers required):
  cargo build --release --locked
  cargo install --path . --locked
Fedora build dependencies: gcc pkgconf-pkg-config systemd-devel.
Debian/Ubuntu build dependencies: build-essential pkg-config libudev-dev.
Audio metrics use pactl (PulseAudio or PipeWire's PulseAudio server).

Quick start:
  open-wbg-service profiles
  open-wbg-service run --dry-run --duration 10
  open-wbg-service run
  open-wbg-service install-user --start

Run install-user from your compositor's terminal, never as root. It imports the
current compositor environment and installs a systemd user unit for this executable.
If hidraw permission is denied, install packaging/70-open-wbg-service.rules into
/etc/udev/rules.d/, run `sudo udevadm control --reload-rules`, then reconnect the keyboard.

Configure linked profiles in Wootility's My Profiles tab. Existing Windows-only
app paths need Linux associations. Custom Niri app_id rules are optional; print
an annotated example with `example-config`. In the browser, allow Wootility's
local/loopback network permission to connect to the API on localhost:50052.
SIGINT/SIGTERM restore the previous onboard profile. SIGHUP reloads rules/providers.
Use `watch` to inspect real focused identities, `status` for the daemon's state.
Do not run alongside the proprietary service or another profile-switching daemon.
Updates use your package manager or a new source build, not Wooting's updater."#
)]
struct Cli {
    /// TOML configuration (default: $XDG_CONFIG_HOME/open-wbg-service/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run until SIGINT/SIGTERM, without requiring Wootility to stay open
    Run {
        #[arg(long, value_enum)]
        backend: Option<compositor::Backend>,
        /// Read keyboards and report decisions, but never write HID reports
        #[arg(long)]
        dry_run: bool,
        /// Disable the localhost:50052 Wootility compatibility API
        #[arg(long)]
        no_api: bool,
        /// Stop gracefully after this many seconds (useful for diagnostics)
        #[arg(long)]
        duration: Option<u64>,
    },
    /// List supported control interfaces without changing keyboard state
    Devices,
    /// Read onboard/linked profile names and app links directly from keyboards
    Profiles {
        #[arg(long)]
        serial: Option<String>,
    },
    /// Activate a stored profile on all keyboards (or one --serial); stop daemon first
    Switch {
        /// onboard:INDEX or linked:INDEX; indices are zero-based
        profile: String,
        #[arg(long)]
        serial: Option<String>,
    },
    /// Print compositor focus events and resolved Steam/Proton identities as JSON
    Watch {
        #[arg(long, value_enum)]
        backend: Option<compositor::Backend>,
        #[arg(long)]
        duration: Option<u64>,
    },
    /// Sample enabled Linux data providers and print actual values as JSON
    Metrics,
    /// List installed XDG desktop and native/Flatpak Steam applications
    Apps,
    /// Read the running daemon's loopback status endpoint
    Status,
    /// Validate configuration without starting workers or changing the keyboard
    Check,
    /// Print an annotated TOML example; no configuration is written
    ExampleConfig,
    /// Install and enable this executable as a systemd user service (never root)
    InstallUser {
        #[arg(long)]
        start: bool,
    },
    /// Stop/remove only this service's generated systemd user unit; keep all settings
    UninstallUser,
    /// Authorize your own Discord RPC application; no Wooting credentials are used
    #[command(
        long_about = "Authorize a Discord RPC application owned by you. Discord requires an approved\napplication or your account on its RPC tester list. Set OPEN_WBG_DISCORD_CLIENT_SECRET\nin the environment (never pass the secret on the command line). The redirect URI\nmust be registered on that application. Discord shows its consent dialog. The\nOAuth token is stored in a private 0600 file, not in the TOML configuration.\nExpired authorization must be renewed with this command. This is not a Discord\nuser/session token login and cannot bypass Discord's approval requirements."
    )]
    DiscordLogin {
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        redirect_uri: String,
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
}

fn selected_devices(serial: Option<&str>, config: &config::Config) -> Result<Vec<hid::DeviceInfo>> {
    let devices: Vec<_> = hid::enumerate()?
        .into_iter()
        .filter(|device| {
            serial.map_or_else(
                || config.serials.is_empty() || config.serials.contains(&device.serial),
                |serial| serial == device.serial,
            )
        })
        .collect();
    if devices.is_empty() {
        bail!(
            "no matching Wooting FF55 control interface found; check connection, serial filter, and hidraw permissions (packaging/70-open-wbg-service.rules)"
        );
    }
    Ok(devices)
}

async fn execute(cli: Cli, log_dir: PathBuf) -> Result<()> {
    let path = cli
        .config
        .clone()
        .map(Ok)
        .unwrap_or_else(config::config_path)?;
    if cli.config.is_some() && !path.is_file() {
        bail!("explicit config file {} does not exist", path.display());
    }
    match cli.command {
        Command::ExampleConfig => print!("{}", include_str!("../examples/config.toml")),
        Command::Run {
            backend,
            dry_run,
            no_api,
            duration,
        } => {
            runtime::run(runtime::Options {
                config_path: path,
                backend,
                dry_run,
                no_api,
                duration,
                log_dir,
            })
            .await?
        }
        Command::Devices => println!("{}", serde_json::to_string_pretty(&hid::enumerate()?)?),
        Command::Profiles { serial } => {
            let _lock = runtime::device_lock()?;
            let config = config::Config::load(&path)?;
            let mut catalogs = Vec::new();
            for info in selected_devices(serial.as_deref(), &config)? {
                let device = hid::Device::open(&info)?;
                catalogs.push(json!({"device":info,"current":device.current_profile()?,"linked":device.linked_profile()?,"profiles":device.profiles()?}));
            }
            println!("{}", serde_json::to_string_pretty(&catalogs)?);
        }
        Command::Switch { profile, serial } => {
            let id = config::parse_profile(&profile)?;
            let config = config::Config::load(&path)?;
            let _lock = runtime::device_lock()?;
            for info in selected_devices(serial.as_deref(), &config)? {
                let device = hid::Device::open(&info)?;
                let catalog = device.profiles()?;
                if !catalog.iter().any(|p| p.id == id) {
                    bail!("profile {profile} does not exist on {}", info.product);
                }
                if id.namespace == 1 {
                    device.select_linked(Some(id))?;
                } else if device.linked_profile()?.is_some() {
                    device.select_linked(None)?;
                }
                device.activate(id)?;
                println!(
                    "{}",
                    json!({"device":info.product,"current":device.current_profile()?})
                );
            }
        }
        Command::Watch { backend, duration } => {
            let config = config::Config::load(&path)?;
            let backend = backend.unwrap_or(config.backend).resolve()?;
            let (tx, mut rx) = tokio::sync::watch::channel(compositor::Focus::default());
            let task = tokio::spawn(compositor::run(backend, tx));
            let until = tokio::time::sleep(Duration::from_secs(duration.unwrap_or(u64::MAX / 4)));
            tokio::pin!(until);
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = &mut until => break,
                    change = rx.changed() => {
                        change?;
                        let focus = rx.borrow_and_update().clone();
                        let identity = focus.window.as_ref().map(applications::identify);
                        println!("{}", json!({"backend":backend,"focus":focus,"identity":identity}));
                    }
                }
            }
            task.abort();
        }
        Command::Metrics => {
            let config = config::Config::load(&path)?;
            let points = tokio::task::spawn_blocking(move || {
                let mut sampler = metrics::Sampler::new();
                let _ = sampler.sample(&config.enabled_sources);
                std::thread::sleep(Duration::from_millis(250));
                sampler.sample(&config.enabled_sources)
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&points)?);
        }
        Command::Apps => println!(
            "{}",
            serde_json::to_string_pretty(
                &tokio::task::spawn_blocking(applications::installed_apps).await?
            )?
        ),
        Command::Status => {
            let config = config::Config::load(&path)?;
            let response: serde_json::Value = reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}/status", config.api_port))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .context("cannot reach daemon; run `open-wbg-service run` first")?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Command::Check => {
            let config = config::Config::load(&path)?;
            println!(
                "{}",
                json!({"config":path,"valid":true,"rules":config.rules.len(),"sources":config.enabled_sources})
            );
        }
        Command::InstallUser { start } => {
            config::Config::load(&path)?;
            let unit = installation::install(&path, start).await?;
            println!(
                "Installed {}\nAutostart enabled. View logs: journalctl --user -u {}",
                unit.display(),
                installation::UNIT
            );
        }
        Command::UninstallUser => installation::uninstall().await?,
        Command::DiscordLogin {
            client_id,
            redirect_uri,
            token_file,
        } => {
            let secret = std::env::var("OPEN_WBG_DISCORD_CLIENT_SECRET").context("set OPEN_WBG_DISCORD_CLIENT_SECRET for your own approved/tester Discord application")?;
            let token_file = token_file.unwrap_or(config::state_dir()?.join("discord-token.json"));
            discord::authorize(&client_id, &secret, &redirect_uri, &token_file).await?;
            let config = config::Config::load(&path)?;
            let (tx, _rx) = tokio::sync::watch::channel(std::sync::Arc::new(config));
            config::Store { path, tx }.update(|config| {
                config.discord.client_id = client_id;
                config.discord.token_file = Some(token_file);
                config.enabled_sources.insert("discord_source".into());
                Ok(())
            })?;
            println!(
                "Discord authorized. Reload the daemon with SIGHUP or restart its user service."
            );
        }
    }
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let log_dir = config::state_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("open-wbg-service")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)?;
    let (file, _guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false),
        )
        .init();
    execute(cli, log_dir).await
}
