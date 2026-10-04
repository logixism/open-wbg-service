use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{applications::Identity, compositor::Backend, hid::ProfileId};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub backend: Backend,
    pub api: bool,
    pub api_port: u16,
    pub interval_ms: u64,
    pub catalog_refresh_secs: u64,
    pub restore_on_exit: bool,
    pub fallback_profile: u8,
    pub notify: bool,
    pub serials: Vec<String>,
    pub enabled_sources: BTreeSet<String>,
    pub rules: Vec<Rule>,
    pub discord: Discord,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            api: true,
            api_port: 50052,
            interval_ms: 1000,
            catalog_refresh_secs: 10,
            restore_on_exit: true,
            fallback_profile: 0,
            notify: false,
            serials: Vec::new(),
            rules: Vec::new(),
            discord: Discord::default(),
            enabled_sources: [
                "system_info_source",
                "system_volume",
                "system_battery_source",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discord {
    pub client_id: String,
    /// File containing an OAuth access token, never a Discord user/session token.
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub profile: String,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub executable: Option<String>,
    pub steam_id: Option<u32>,
    pub serial: Option<String>,
    #[serde(skip)]
    app_pattern: Option<Regex>,
    #[serde(skip)]
    title_pattern: Option<Regex>,
    #[serde(skip)]
    executable_pattern: Option<Regex>,
    #[serde(skip)]
    target: Option<ProfileId>,
}

pub fn parse_profile(value: &str) -> Result<ProfileId> {
    let (namespace, index) = value
        .split_once(':')
        .context("profile must be onboard:INDEX or linked:INDEX (zero-based)")?;
    let namespace = match namespace {
        "onboard" => 0,
        "linked" => 1,
        _ => bail!("unknown profile namespace {namespace:?}; use onboard or linked"),
    };
    Ok(ProfileId {
        namespace,
        index: index.parse().context("profile index must be 0..255")?,
    })
}

impl Rule {
    fn compile(&mut self) -> Result<()> {
        self.target = Some(parse_profile(&self.profile)?);
        if self.app_id.is_none()
            && self.title.is_none()
            && self.executable.is_none()
            && self.steam_id.is_none()
        {
            bail!("rule for {} has no window selector", self.profile);
        }
        self.app_pattern = self
            .app_id
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid app_id regex")?;
        self.title_pattern = self
            .title
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid title regex")?;
        self.executable_pattern = self
            .executable
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid executable regex")?;
        Ok(())
    }

    pub fn matches(&self, identity: &Identity, serial: &str) -> Option<ProfileId> {
        if self.serial.as_deref().is_some_and(|s| s != serial)
            || self
                .app_pattern
                .as_ref()
                .is_some_and(|p| !p.is_match(&identity.app_id))
            || self
                .title_pattern
                .as_ref()
                .is_some_and(|p| !p.is_match(&identity.title))
            || self
                .executable_pattern
                .as_ref()
                .is_some_and(|p| !identity.executable_paths.iter().any(|exe| p.is_match(exe)))
            || self
                .steam_id
                .is_some_and(|id| !identity.steam_ids.contains(&id))
        {
            None
        } else {
            self.target
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&mut self) -> Result<()> {
        if !(100..=60_000).contains(&self.interval_ms) {
            bail!("interval_ms must be 100..60000");
        }
        if !(1..=3600).contains(&self.catalog_refresh_secs) {
            bail!("catalog_refresh_secs must be 1..3600");
        }
        if self.fallback_profile > 3 {
            bail!("fallback_profile must be an onboard index 0..3");
        }
        if self.api_port == 0 {
            bail!("api_port must not be zero");
        }
        for source in &self.enabled_sources {
            if !SOURCES.iter().any(|(id, _)| *id == source) {
                bail!("unknown data source: {source}");
            }
        }
        for rule in &mut self.rules {
            rule.compile()?;
        }
        Ok(())
    }
}

pub const SOURCES: &[(&str, &str)] = &[
    ("system_info_source", "System information"),
    ("system_volume", "System volume"),
    ("system_battery_source", "System battery"),
    (
        "discord_source",
        "Discord (requires authorized RPC credentials)",
    ),
];

pub fn config_path() -> Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join("open-wbg-service/config.toml"))
}
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg("XDG_STATE_HOME", ".local/state")?.join("open-wbg-service"))
}
fn xdg(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(fallback))
}

/// API updates are persisted before becoming visible to either runtime worker.
pub struct Store {
    pub path: PathBuf,
    pub tx: watch::Sender<Arc<Config>>,
}
impl Store {
    pub fn update(&self, change: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
        let mut next = self.tx.borrow().as_ref().clone();
        change(&mut next)?;
        next.validate()?;
        let text = toml::to_string_pretty(&next)?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(".open-wbg-config-{}.tmp", std::process::id()));
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result = (|| -> Result<()> {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.tx.send_replace(Arc::new(next));
        Ok(())
    }
    pub fn reload(&self) -> Result<()> {
        let next = Config::load(&self.path)?;
        let current = self.tx.borrow();
        if std::mem::discriminant(&next.backend) != std::mem::discriminant(&current.backend)
            || next.api != current.api
            || next.api_port != current.api_port
        {
            bail!(
                "backend/API changes require a service restart; existing configuration remains active"
            );
        }
        drop(current);
        self.tx.send_replace(Arc::new(next));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_rules_fail_before_any_keyboard_write() {
        for text in [
            "[[rules]]\nprofile='linked:0'",
            "[[rules]]\nprofile='linked:0'\napp_id='['",
            "[[rules]]\nprofile='flash:1'\napp_id='game'",
        ] {
            let mut config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err());
        }
    }
}
