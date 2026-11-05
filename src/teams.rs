//! Launching/attaching Teams and finding its CDP endpoint.
//!
//! The new Teams renders in a Microsoft Edge WebView2 helper, so the host
//! `--remote-debugging-port` flag does nothing. The working mechanism is the
//! WebView2 env var `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`, which must be set
//! in the environment the app launches with.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::config::Config;

pub fn browser_arg_env(port: u16) -> String {
    format!("--remote-debugging-port={port}")
}

/// GET a DevTools HTTP endpoint (blocking).
fn http_get(port: u16, path: &str) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let body = ureq::get(&url).call()?.body_mut().read_to_string()?;
    Ok(body)
}

/// The browser-level WebSocket URL, if the debug port is answering.
pub fn browser_ws_url(port: u16) -> Result<String> {
    let body = http_get(port, "/json/version")?;
    let v: Value = serde_json::from_str(&body)?;
    v.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in /json/version"))
}

/// Is Teams already exposing a usable CDP port?
pub fn port_is_up(port: u16) -> bool {
    browser_ws_url(port).is_ok()
}

#[cfg(target_os = "macos")]
fn kill_teams(config: &Config) {
    // Quit gracefully first so Teams closes all its windows cleanly; a hard
    // kill can leave an orphaned blank window behind.
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("tell application \"{}\" to quit", config.mac.app_name),
        ])
        .status();

    // Wait up to ~5s for it to exit.
    for _ in 0..25 {
        let alive = std::process::Command::new("pgrep")
            .args(["-x", &config.mac.process_name])
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Fallback: force-kill any stragglers.
    let _ = std::process::Command::new("pkill")
        .args(["-x", &config.mac.process_name])
        .status();
    let _ = std::process::Command::new("pkill")
        .args(["-f", "Microsoft Teams WebView"])
        .status();
}

#[cfg(target_os = "windows")]
fn kill_teams(config: &Config) {
    use std::os::windows::process::CommandExt;
    if !windows_teams_running(config) {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", &config.windows.process_name])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no console flash
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::thread::sleep(std::time::Duration::from_millis(800));
}

/// Is a Teams process running (even with no window, e.g. closed to the tray)?
#[cfg(target_os = "windows")]
fn windows_teams_running(config: &Config) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", config.windows.process_name), "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains(&config.windows.process_name.to_lowercase()))
        .unwrap_or(false)
}

/// Bring a running Teams to the front. Closing Teams' window leaves it running in the tray, so
/// clicking the Theames taskbar button must show its window again: starting Teams' launcher once
/// more hands that to the running instance, the same as clicking Teams' own icon.
#[cfg(target_os = "windows")]
fn show_teams(config: &Config) {
    match resolve_windows_teams(config) {
        Ok(exe) => {
            let _ = std::process::Command::new(exe)
                .env("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", browser_arg_env(config.debug_port))
                .spawn();
        }
        Err(e) => eprintln!("[theames] couldn't bring Teams to the front: {e}"),
    }
}

#[cfg(target_os = "macos")]
fn spawn_teams(config: &Config) -> Result<()> {
    let env = format!(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS={}",
        browser_arg_env(config.debug_port)
    );
    std::process::Command::new("open")
        .arg("-a")
        .arg(&config.mac.app_name)
        .arg("--env")
        .arg(&env)
        .status()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_teams(config: &Config) -> Result<()> {
    let exe = resolve_windows_teams(config)?;
    std::process::Command::new(exe)
        .env(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            browser_arg_env(config.debug_port),
        )
        .spawn()?;
    Ok(())
}

/// Where to start Teams from on Windows: the configured path; else Teams' App Execution Alias
/// (`ms-teams.exe` in %LOCALAPPDATA%\Microsoft\WindowsApps), the supported way to start a packaged
/// app that still passes our environment (the debug port) on; else a scan of WindowsApps, which
/// normal accounts usually aren't allowed to list.
#[cfg(target_os = "windows")]
fn resolve_windows_teams(config: &Config) -> Result<std::path::PathBuf> {
    use std::path::PathBuf;
    if !config.windows.teams_path.is_empty() {
        return Ok(PathBuf::from(&config.windows.teams_path));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let aliases = PathBuf::from(local).join("Microsoft").join("WindowsApps");
        for alias in [aliases.join("ms-teams.exe"), aliases.join("MSTeams_8wekyb3d8bbwe").join("ms-teams.exe")] {
            // An execution alias is a reparse point that can't be followed, so don't use exists().
            if std::fs::symlink_metadata(&alias).is_ok() {
                return Ok(alias);
            }
        }
    }
    let program_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    let windows_apps = PathBuf::from(program_files).join("WindowsApps");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(&windows_apps)
        .map_err(|e| anyhow!("could not find Microsoft Teams (no ms-teams.exe alias, and {} isn't readable: {e})", windows_apps.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("MSTeams_") && n.ends_with("_x64__8wekyb3d8bbwe"))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches
        .pop()
        .map(|p| p.join("ms-teams.exe"))
        .ok_or_else(|| anyhow!("could not find Teams under {}", windows_apps.display()))
}

/// Ensure Teams is running with the debug port and return its browser WS URL.
/// If Teams is already exposing the port, attach to it; otherwise (re)launch it
/// with the WebView2 debug env var and poll until the port answers.
pub async fn ensure_running(config: &Config) -> Result<String> {
    let port = config.debug_port;

    if port_is_up(port) {
        println!("[theames] Teams already on debug port {port}, attaching.");
        // Windows: Theames is what the taskbar button starts, so show Teams (it may be in the tray).
        #[cfg(target_os = "windows")]
        show_teams(config);
        return browser_ws_url(port);
    }

    println!("[theames] launching Teams with debugging on port {port}...");
    let cfg = config_clone(config);
    tokio::task::spawn_blocking(move || {
        // Kill any debug-less Teams so the relaunch picks up the env var.
        kill_teams(&cfg);
        spawn_teams(&cfg)
    })
    .await??;

    // Poll for the port (blocking HTTP on a worker thread).
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if tokio::task::spawn_blocking(move || port_is_up(port)).await? {
            println!("[theames] debug port is up.");
            return browser_ws_url(port);
        }
        if Instant::now() > deadline {
            bail!("debug port {port} did not come up within 45s");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Minimal clone of the fields the blocking launch needs.
pub fn config_clone(config: &Config) -> Config {
    Config {
        debug_port: config.debug_port,
        mac: crate::config::MacConfig {
            app_name: config.mac.app_name.clone(),
            process_name: config.mac.process_name.clone(),
        },
        windows: crate::config::WindowsConfig {
            teams_path: config.windows.teams_path.clone(),
            process_name: config.windows.process_name.clone(),
        },
        selection: crate::config::Selection::default(),
        variables: Default::default(),
        applied_icon: None,
        login_item_path: None,
        welcomed: false,
        pin_attempted: false,
    }
}
