//! Uninstalling Theames: put the computer back the way it was before Theames. Installed themes are
//! kept. Reached from the panel's "Uninstall Theames" button or `--uninstall`.
//! - macOS (`open Theames.app --args --uninstall`, through LaunchServices so macOS counts the icon
//!   change as Theames' own): Teams' own icon, no login item, no session debug port.
//! - Windows (`Theames.exe --uninstall`, also what Settings → Apps runs): unpinned from the taskbar,
//!   no shortcuts or generated icons, no Installed-apps entry, and Theames.exe itself is removed
//!   once it has exited.
//! Both: no Theames settings or log. A running Theames is stopped first.

use anyhow::Result;

use crate::{config, instance};

pub fn run() -> Result<()> {
    let _instance = instance::acquire()?;
    cleanup()?;
    notify_done();
    #[cfg(windows)]
    crate::winshell::delete_exe_after_exit();
    Ok(())
}

/// The undo steps themselves.
pub fn cleanup() -> Result<()> {
    println!("[theames] uninstalling");
    #[cfg(target_os = "macos")]
    {
        let cfg = config::Config::load()?;
        match crate::appicon::sync(&config::themes_dir(), None, cfg.applied_icon.as_deref()) {
            Ok(_) => {}
            Err(e) => crate::appicon::explain_failure(&e),
        }
        crate::dock::unregister_login_item();
        crate::dock::clear_session_env();
    }
    #[cfg(windows)]
    {
        crate::wintaskbar::release();
        crate::winshell::unpin_from_taskbar();
        crate::winshell::remove_shortcuts();
        crate::winshell::remove_uninstall_entry();
    }

    let dir = config::base_dir();
    for f in ["config.json", "teams-icon.png", "teams-icon.icns", "theames.pid", "theames.log", "theames.lock"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    Ok(())
}

/// Tell the user it's done (the panel is gone by now).
pub fn notify_done() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            "display dialog \"Theames was uninstalled and Microsoft Teams is back to normal. You can now move Theames to the Trash.\" with title \"Theames\" buttons {\"OK\"} default button \"OK\"",
        ])
        .output();
    #[cfg(windows)]
    crate::winshell::message("Theames was uninstalled and Microsoft Teams is back to normal.");
}
