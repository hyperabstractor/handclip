use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use global_hotkey::hotkey::{HotKey, Modifiers};
use serde::{Deserialize, Serialize};

const DEFAULT_RELAY: &str = "127.0.0.1:24871";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_id: String,
    pub relay_endpoints: Vec<String>,
    pub token_file: PathBuf,
    pub receive_dir: PathBuf,
    pub log_dir: PathBuf,
    #[serde(default)]
    pub target_shortcuts: Vec<TargetShortcut>,
    #[serde(default = "default_broadcast_shortcut")]
    pub broadcast_shortcut: String,
    #[serde(default = "default_notifications")]
    pub notifications: bool,
    /// Also host the relay inside this agent.
    #[serde(default)]
    pub run_relay: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetShortcut {
    pub device_id: String,
    pub shortcut: String,
}

#[derive(Debug, Default)]
pub struct ConfigOverrides {
    pub device_id: Option<String>,
    pub relay_endpoints: Vec<String>,
    pub token_file: Option<PathBuf>,
    pub receive_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub broadcast_shortcut: Option<String>,
}

impl AppConfig {
    /// First-launch settings; incomplete until the user adds a relay in Settings.
    pub fn draft() -> Self {
        Self {
            device_id: default_device_id(),
            relay_endpoints: Vec::new(),
            token_file: default_token_file(),
            receive_dir: default_receive_dir(),
            log_dir: default_log_dir(),
            target_shortcuts: Vec::new(),
            broadcast_shortcut: default_broadcast_shortcut(),
            notifications: true,
            run_relay: false,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_overrides(overrides: ConfigOverrides) -> Result<Self> {
        let device_id = overrides
            .device_id
            .ok_or_else(|| anyhow!("set --device or create a Handclip settings file"))?;
        let relay_endpoints = if overrides.relay_endpoints.is_empty() {
            vec![DEFAULT_RELAY.to_owned()]
        } else {
            overrides.relay_endpoints
        };
        let config = Self {
            device_id,
            relay_endpoints,
            token_file: overrides.token_file.unwrap_or_else(default_token_file),
            receive_dir: overrides.receive_dir.unwrap_or_else(default_receive_dir),
            log_dir: overrides.log_dir.unwrap_or_else(default_log_dir),
            target_shortcuts: Vec::new(),
            broadcast_shortcut: overrides
                .broadcast_shortcut
                .unwrap_or_else(default_broadcast_shortcut),
            notifications: true,
            run_relay: false,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !handclip_core::validate_device_id(&self.device_id) {
            bail!("device ID must use 1-64 ASCII letters, numbers, dots, dashes, or underscores");
        }
        if self.relay_endpoints.is_empty() {
            bail!("configure at least one relay endpoint");
        }
        for endpoint in &self.relay_endpoints {
            validate_endpoint(endpoint)?;
        }
        if self.token_file.as_os_str().is_empty() {
            bail!("token file path must not be empty");
        }
        if self.receive_dir.as_os_str().is_empty() {
            bail!("received-files directory must not be empty");
        }
        if self.log_dir.as_os_str().is_empty() {
            bail!("log directory must not be empty");
        }
        let mut device_ids = HashSet::new();
        let mut hotkey_ids = HashSet::new();
        for target in &self.target_shortcuts {
            if !handclip_core::validate_device_id(&target.device_id) {
                bail!("target device ID {:?} is invalid", target.device_id);
            }
            if !device_ids.insert(target.device_id.clone()) {
                bail!(
                    "target device ID {:?} is configured twice",
                    target.device_id
                );
            }
            let hotkey = validate_hotkey(
                &target.shortcut,
                &format!("shortcut for {}", target.device_id),
            )?;
            if !hotkey_ids.insert(hotkey.id()) {
                bail!(
                    "shortcut {:?} is configured more than once",
                    target.shortcut
                );
            }
        }
        let broadcast_hotkey = validate_hotkey(&self.broadcast_shortcut, "broadcast shortcut")?;
        if !hotkey_ids.insert(broadcast_hotkey.id()) {
            bail!(
                "broadcast shortcut {:?} is also assigned to a device",
                self.broadcast_shortcut
            );
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("failed to encode settings")?;
        std::fs::write(path, bytes)
            .with_context(|| format!("failed to save config file {}", path.display()))
    }
}

fn validate_hotkey(value: &str, description: &str) -> Result<HotKey> {
    let hotkey: HotKey = value
        .parse()
        .with_context(|| format!("invalid {description} {value:?}"))?;
    if !hotkey
        .mods
        .intersects(Modifiers::ALT | Modifiers::CONTROL | Modifiers::SHIFT | Modifiers::SUPER)
    {
        bail!("{description} must include at least one modifier key");
    }
    Ok(hotkey)
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    let endpoint = endpoint.trim();
    let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| anyhow!("invalid relay endpoint {endpoint:?}"))?;
        (host, port)
    } else {
        endpoint
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("relay endpoint must include a port: {endpoint:?}"))?
    };
    if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
        bail!("relay endpoint has an invalid host: {endpoint:?}");
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("relay endpoint has an invalid port: {endpoint:?}"))?;
    if port == 0 {
        bail!("relay endpoint port must not be zero: {endpoint:?}");
    }
    Ok(())
}

const fn default_notifications() -> bool {
    true
}

pub fn default_config_file() -> PathBuf {
    platform_data_dir().join("config.json")
}

pub fn default_log_dir() -> PathBuf {
    platform_data_dir().join("logs")
}

pub fn default_token_file() -> PathBuf {
    platform_data_dir().join("token")
}

pub fn default_receive_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Caches")
            .join("Handclip")
            .join("received");
    }
    platform_data_dir().join("received")
}

/// Writes a token pasted into Settings; a blank token keeps the existing file.
pub fn save_token(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;

    let token = token.trim();
    if token.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create token directory {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| writeln!(file, "{token}"))
        .with_context(|| format!("failed to save token file {}", path.display()))
}

/// The computer's name, reduced to characters a device ID allows.
fn default_device_id() -> String {
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok());
    #[cfg(not(target_os = "macos"))]
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok();
    name.unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase()
}

pub fn default_broadcast_shortcut() -> String {
    "Control+Alt+0".into()
}

fn platform_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Handclip");
    }
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data).join("Handclip");
    }
    std::env::temp_dir().join("Handclip")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AppConfig {
        AppConfig {
            device_id: "laptop".into(),
            relay_endpoints: vec![
                "relay.example.ts.net:24871".into(),
                "192.0.2.10:24871".into(),
            ],
            token_file: PathBuf::from("/tmp/token"),
            receive_dir: PathBuf::from("/tmp/received"),
            log_dir: PathBuf::from("/tmp/logs"),
            target_shortcuts: vec![TargetShortcut {
                device_id: "desktop".into(),
                shortcut: "Control+Alt+1".into(),
            }],
            broadcast_shortcut: default_broadcast_shortcut(),
            notifications: true,
            run_relay: false,
        }
    }

    #[test]
    fn validates_ordered_relay_endpoints() {
        assert!(valid_config().validate().is_ok());

        let mut invalid = valid_config();
        invalid.relay_endpoints = vec!["missing-port".into()];
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn rejects_unmodified_shortcuts() {
        let mut invalid = valid_config();
        invalid.target_shortcuts[0].shortcut = "KeyC".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_shortcuts() {
        let mut invalid = valid_config();
        invalid.broadcast_shortcut = invalid.target_shortcuts[0].shortcut.clone();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn draft_needs_setup_and_broadcast_only_config_is_valid() {
        let draft = AppConfig::draft();
        assert!(draft.validate().is_err(), "a draft has no relay yet");

        let mut broadcast_only = valid_config();
        broadcast_only.target_shortcuts.clear();
        assert!(broadcast_only.validate().is_ok());
    }

    #[test]
    fn saves_token_and_keeps_it_when_blank() {
        let path = std::env::temp_dir()
            .join(format!("handclip-token-{}", std::process::id()))
            .join("token");
        save_token(&path, "  secret \n").unwrap();
        save_token(&path, "   ").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn builds_config_from_explicit_overrides() {
        let config = AppConfig::from_overrides(ConfigOverrides {
            device_id: Some("mini".into()),
            relay_endpoints: vec!["127.0.0.1:24871".into()],
            ..ConfigOverrides::default()
        })
        .unwrap();
        assert_eq!(config.device_id, "mini");
        assert_eq!(config.relay_endpoints, ["127.0.0.1:24871"]);
    }

    #[test]
    fn migrates_legacy_single_shortcut_settings() {
        let legacy = serde_json::json!({
            "device_id": "air",
            "relay_endpoints": ["127.0.0.1:24871"],
            "token_file": "/tmp/token",
            "receive_dir": "/tmp/received",
            "log_dir": "/tmp/logs",
            "shortcut": "Command+Option+KeyC",
            "notifications": true
        });
        let config: AppConfig = serde_json::from_value(legacy).unwrap();

        assert!(config.target_shortcuts.is_empty());
        assert_eq!(config.broadcast_shortcut, default_broadcast_shortcut());
        assert!(!config.run_relay);
        assert!(config.validate().is_ok());
    }
}
