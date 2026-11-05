//! The page->Theames request handlers. The injected UI calls these by name
//! over the CDP binding; each returns JSON data plus whether the change should
//! trigger a reinject of all pages.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{Config, Selection};
use crate::inject;
use crate::theame::{self, Theame};

/// State shared between the event loop and the request handlers.
pub struct Shared {
    pub config: Mutex<Config>,
    pub themes_dir: PathBuf,
}

impl Shared {
    pub async fn theme(&self) -> Result<inject::Theme> {
        let cfg = self.config.lock().await;
        inject::build_theme(&self.themes_dir, &cfg)
    }
}

/// macOS: bring the Teams app icon in line with the selected icon (see `appicon::sync`). It may
/// show the admin prompt, so it runs on a blocking thread and never holds the config lock while
/// waiting for the user.
#[cfg(target_os = "macos")]
pub async fn sync_teams_icon(shared: Arc<Shared>) {
    let (dir, icon, applied) = {
        let cfg = shared.config.lock().await;
        (shared.themes_dir.clone(), cfg.selection.icon.clone(), cfg.applied_icon.clone())
    };
    let before = applied.clone();
    let now = match tokio::task::spawn_blocking(move || crate::appicon::sync(&dir, icon.as_deref(), applied.as_deref())).await {
        Ok(Ok(now)) => now,
        Ok(Err(e)) => return crate::appicon::explain_failure(&e),
        Err(e) => return eprintln!("[theames] icon task failed: {e}"),
    };
    if now != before {
        let mut cfg = shared.config.lock().await;
        cfg.applied_icon = now;
        cfg.save().ok();
    }
}

/// Whether the handled request should re-apply the theme in place afterwards.
/// The page applies changes reversibly (enables/disables mods, swaps styles),
/// so no reloads are ever needed.
#[derive(Clone, Copy, PartialEq)]
pub enum Reinject {
    None,
    Apply,
    /// Remove Theames from every page, undo its system changes, and quit.
    Uninstall,
}

/// Handle one request. Returns (response data, follow-up action).
pub async fn handle(shared: &Arc<Shared>, action: &str, payload: Value) -> Result<(Value, Reinject)> {
    match action {
        "list" => Ok((list(shared).await, Reinject::None)),
        "getTheme" => Ok((serde_json::to_value(shared.theme().await?)?, Reinject::None)),
        "setSelection" => {
            let selection: Selection = serde_json::from_value(payload)?;
            let mut cfg = shared.config.lock().await;
            #[cfg(any(target_os = "macos", windows))]
            let icon_changed = cfg.selection.icon != selection.icon;
            cfg.selection = selection;
            cfg.save()?;
            #[cfg(target_os = "macos")]
            if icon_changed {
                // May show the admin prompt, so don't hold up the request.
                tokio::spawn(sync_teams_icon(shared.clone()));
            }
            #[cfg(windows)]
            if icon_changed {
                let (dir, icon) = (shared.themes_dir.clone(), cfg.selection.icon.clone());
                tokio::task::spawn_blocking(move || match crate::winshell::sync_shortcuts(&dir, icon.as_deref(), false) {
                    Ok(icon) => crate::wintaskbar::set_icon(icon),
                    Err(e) => eprintln!("[theames] shortcuts: {e}"),
                });
            }
            Ok((json!({ "ok": true }), Reinject::Apply))
        }
        "reinject" => Ok((json!({ "ok": true }), Reinject::Apply)),
        "uninstall" => Ok((json!({ "ok": true }), Reinject::Uninstall)),
        // payload: { file, name, value }  (value null/absent = back to the default)
        "setVariable" => {
            let file = payload["file"].as_str().ok_or_else(|| anyhow!("setVariable: missing file"))?.to_string();
            let name = payload["name"].as_str().ok_or_else(|| anyhow!("setVariable: missing name"))?.to_string();
            let mut cfg = shared.config.lock().await;
            match payload["value"].as_str() {
                Some(v) => {
                    cfg.variables.entry(file).or_default().insert(name, v.to_string());
                }
                None => {
                    if let Some(map) = cfg.variables.get_mut(&file) {
                        map.remove(&name);
                    }
                }
            }
            cfg.save()?;
            Ok((json!({ "ok": true }), Reinject::Apply))
        }
        "openFolder" => {
            open_folder(&shared.themes_dir);
            Ok((json!({ "ok": true }), Reinject::None))
        }
        other => Err(anyhow!("unknown action: {other}")),
    }
}

/// Installed themes (with their declared components) plus the current selection.
async fn list(shared: &Arc<Shared>) -> Value {
    let cfg = shared.config.lock().await;
    let mut themes = Vec::new();
    for file in theame::list(&shared.themes_dir) {
        match Theame::open(&shared.themes_dir.join(&file)) {
            Ok(t) => {
                let values = t.variable_values(cfg.variables.get(&file));
                let variables: Vec<Value> = t
                    .manifest
                    .variables
                    .iter()
                    .map(|v| {
                        json!({
                            "name": v.name,
                            "label": if v.label.is_empty() { v.name.clone() } else { v.label.clone() },
                            "type": v.kind,
                            "default": v.default,
                            "min": v.min,
                            "max": v.max,
                            "step": v.step,
                            "value": values.get(&v.name).cloned().unwrap_or_else(|| v.default.clone()),
                        })
                    })
                    .collect();
                themes.push(json!({
                    "file": file,
                    "name": t.manifest.name,
                    "version": t.manifest.version,
                    "author": t.manifest.author,
                    "mods": t.manifest.mods.iter().map(|m| &m.name).collect::<Vec<_>>(),
                    "backgrounds": t.manifest.backgrounds.iter().map(|b| &b.name).collect::<Vec<_>>(),
                    "chats": t.manifest.chats.iter().map(|c| &c.name).collect::<Vec<_>>(),
                    "hasIcon": t.manifest.icon.is_some(),
                    "variables": variables,
                }));
            }
            Err(e) => eprintln!("[theames] {e}"),
        }
    }
    json!({ "themes": themes, "selection": cfg.selection, "theamesIcon": theames_icon_available() })
}

fn open_folder(dir: &PathBuf) {
    std::fs::create_dir_all(dir).ok();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).status();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).status();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).status();
}

/// Can Theames offer its own icon for Teams? (macOS, running from Theames.app.)
fn theames_icon_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::appicon::theames_icon().is_some()
    }
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}
