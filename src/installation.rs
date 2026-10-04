use anyhow::{Context, Result, bail};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

pub const UNIT: &str = "open-wbg-service.service";

async fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("systemctl --user timed out")?
    .context("running systemctl --user")
}

pub async fn enabled() -> Result<bool> {
    let output = systemctl(&["is-enabled", UNIT]).await?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "enabled" | "enabled-runtime" => Ok(true),
        "disabled" | "not-found" | "static" | "indirect" | "masked" | "masked-runtime"
        | "linked" | "linked-runtime" => Ok(false),
        _ => bail!(
            "cannot query user service: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub async fn set_enabled(value: bool) -> Result<()> {
    let output = systemctl(&[if value { "enable" } else { "disable" }, UNIT]).await?;
    if !output.status.success() {
        bail!(
            "cannot change autostart; run `open-wbg-service install-user` first: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn unit_path() -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let root = config
        .parent()
        .and_then(Path::parent)
        .context("invalid config location")?;
    Ok(root.join("systemd/user").join(UNIT))
}

fn systemd_arg(path: &Path) -> Result<String> {
    let text = path.to_str().context("systemd paths must be valid UTF-8")?;
    // Escape systemd ExecStart syntax, including specifiers; this is not a shell.
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    ))
}

pub async fn install(config: &Path, start: bool) -> Result<PathBuf> {
    let executable = fs::canonicalize(env::current_exe()?)?;
    let config = if config.is_absolute() {
        config.to_owned()
    } else {
        env::current_dir()?.join(config)
    };
    if !config.exists() {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        fs::create_dir_all(config.parent().context("config path has no parent")?)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config)?;
        file.write_all(toml::to_string_pretty(&crate::config::Config::default())?.as_bytes())?;
        file.sync_all()?;
    }
    let destination = unit_path()?;
    fs::create_dir_all(destination.parent().context("unit path has no parent")?)?;
    let unit = format!(
        "[Unit]\nDescription=Open Wooting Background Service\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nType=simple\nExecStart={} --config {} run\nRestart=on-failure\nRestartSec=3\nTimeoutStopSec=15\nNoNewPrivileges=yes\nUMask=0077\n\n[Install]\nWantedBy=graphical-session.target\n",
        systemd_arg(&executable)?,
        systemd_arg(&config)?
    );
    // Do not replace an unrelated unit or user customizations silently.
    if let Ok(existing) = fs::read_to_string(&destination)
        && existing != unit
    {
        bail!(
            "{} differs from the generated unit; inspect it or use uninstall-user before replacing it",
            destination.display()
        );
    }
    fs::write(&destination, unit)?;
    let reload = systemctl(&["daemon-reload"]).await?;
    if !reload.status.success() {
        bail!(
            "systemd reload failed: {}",
            String::from_utf8_lossy(&reload.stderr)
        );
    }
    // Niri --session imports this itself; also cover installation from a terminal.
    let vars: Vec<&str> = [
        "NIRI_SOCKET",
        "SWAYSOCK",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_RUNTIME_DIR",
    ]
    .into_iter()
    .filter(|key| env::var_os(key).is_some())
    .collect();
    if !vars.is_empty() {
        let mut args = vec!["import-environment"];
        args.extend(vars);
        let output = systemctl(&args).await?;
        if !output.status.success() {
            bail!(
                "cannot import compositor environment: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    set_enabled(true).await?;
    if start {
        let output = systemctl(&["start", UNIT]).await?;
        if !output.status.success() {
            bail!(
                "service start failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(destination)
}

pub async fn uninstall() -> Result<()> {
    let destination = unit_path()?;
    let text = fs::read_to_string(&destination)
        .with_context(|| format!("reading {}", destination.display()))?;
    if !text.contains("Description=Open Wooting Background Service") {
        bail!("refusing to remove a unit not generated by this service");
    }
    let output = systemctl(&["disable", "--now", UNIT]).await?;
    if !output.status.success() {
        bail!(
            "cannot disable service: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::remove_file(destination)?;
    let output = systemctl(&["daemon-reload"]).await?;
    if !output.status.success() {
        bail!(
            "systemd reload failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
