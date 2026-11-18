//! Theames configuration: where themes live, which components are selected,
//! and the per-OS launch settings. Stored as JSON under the Theames data dir
//! (override the base dir with the THEAMES_HOME env var).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub debug_port: u16,
    pub mac: MacConfig,
    pub windows: WindowsConfig,
    /// Which theme components are currently applied.
    pub selection: Selection,
    /// User-chosen values for theme variables: file -> (variable -> value).
    /// Missing entries fall back to the manifest's default.
    pub variables: HashMap<String, HashMap<String, String>>,
    /// The theme icon last written onto the Teams app ("file.theame:hash"), so the launcher only
    /// asks for a password again when Teams lost it (an update) or a different icon is chosen.
    pub applied_icon: Option<String>,
    /// macOS: the Theames.app path last added to Login Items. Theames adds itself again only when it
    /// has moved (e.g. installed into /Applications), so turning it off in System Settings sticks.
    pub login_item_path: Option<String>,
    /// The welcome splash (shown the first time Theames is up and running in Teams) was shown.
    pub welcomed: bool,
    /// Windows: Theames tried pinning its shortcut to the taskbar once (never again, so an unpin sticks).
    pub pin_attempted: bool,
    /// Install new versions of Theames itself automatically (the Settings toggle).
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// Theme files whose own "Update automatically" toggle is off. Every other theme updates
    /// itself from the marketplace.
    pub theme_updates_off: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MacConfig {
    /// App name passed to `open -a`.
    pub app_name: String,
    /// Process name killed with `pkill -x` before relaunch.
    pub process_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowsConfig {
    /// Full path to ms-teams.exe. Empty = auto-detect under WindowsApps.
    pub teams_path: String,
    pub process_name: String,
}

/// A theme component is addressed as "file.theame::ComponentName". For an icon
/// the manifest has a single icon, so just "file.theame".
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Selection {
    /// Mods to inject (many, mix-and-match across themes).
    pub mods: Vec<String>,
    /// The one background to apply, or none.
    pub background: Option<String>,
    /// The one chat/message-area CSS theme to apply, or none.
    pub chats: Option<String>,
    /// The icon applied to the Microsoft Teams app (macOS): a theme file, `THEAMES_ICON` for
    /// Theames' own icon, or none for Teams' own.
    pub icon: Option<String>,
}

/// `Selection::icon` value meaning Theames' own app icon (the default on first setup).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const THEAMES_ICON: &str = "@theames";

impl Default for Config {
    fn default() -> Self {
        Config {
            debug_port: 9222,
            mac: MacConfig::default(),
            windows: WindowsConfig::default(),
            selection: Selection::default(),
            variables: HashMap::new(),
            applied_icon: None,
            login_item_path: None,
            welcomed: false,
            pin_attempted: false,
            auto_update: true,
            theme_updates_off: Vec::new(),
        }
    }
}

impl Default for MacConfig {
    fn default() -> Self {
        MacConfig {
            app_name: "Microsoft Teams".to_string(),
            process_name: "MSTeams".to_string(),
        }
    }
}

impl Default for WindowsConfig {
    fn default() -> Self {
        WindowsConfig {
            teams_path: String::new(),
            process_name: "ms-teams.exe".to_string(),
        }
    }
}

/// Base dir for Theames data (config + themes). `THEAMES_HOME` overrides it.
pub fn base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("THEAMES_HOME") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Theames")
}

pub fn themes_dir() -> PathBuf {
    base_dir().join("themes")
}

pub fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            Ok(serde_json::from_str(&text)?)
        } else {
            // First setup: on macOS, Teams wears the Theames icon until the user picks another.
            #[allow(unused_mut)]
            let mut cfg = Config::default();
            #[cfg(any(target_os = "macos", windows))]
            {
                cfg.selection.icon = Some(THEAMES_ICON.to_string());
            }
            Ok(cfg)
        }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(base_dir())?;
        std::fs::write(config_path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
