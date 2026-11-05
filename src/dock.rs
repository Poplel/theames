//! macOS: run as a background app when running from `Theames.app`.
//!
//! Theames stands in for Teams: Teams' own (themed) Dock icon is the only one, Theames has none
//! (`LSUIElement` in Info.plist). It still checks in with the system as an application (an
//! NSApplication on the main thread, accessory policy); a bare binary in an .app never does, and
//! Finder then treats the launch as hung. The launcher's real work runs on a second thread.
//! It also starts at login and gives every Teams launch in the login session the debug port.
//! Outside a bundle (e.g. `cargo run`) nothing here is used.

use anyhow::Result;
use objc2::MainThreadMarker;
use objc2::runtime::AnyClass;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_service_management::SMAppService;
use std::process::Command;

/// Are we the executable of a `.app` bundle?
pub fn in_bundle() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

/// Run the first-run / permission check, then `work` on a background thread and the AppKit event
/// loop on the main thread. The process
/// exits when `work` ends (Teams closed, or a fatal error), or when the user quits from the Dock.
pub fn run(work: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    let mtm = MainThreadMarker::new().expect("run() must be called from the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // The first-run alerts come before the event loop starts, and a modal alert shown before
    // launching has finished returns at once (as if its first button were pressed).
    app.finishLaunching();

    // First run / permission check first: Teams must not be opened before Theames can theme it.
    match crate::onboarding::preflight(mtm) {
        crate::onboarding::Next::Go => {}
        crate::onboarding::Next::Quit => return Ok(()),
        crate::onboarding::Next::AwaitPermission => crate::onboarding::await_permission(mtm),
    }

    std::thread::Builder::new().name("theames".into()).spawn(move || {
        let code = match work() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[theames] fatal: {e:#}");
                1
            }
        };
        std::process::exit(code);
    })?;
    app.run();
    Ok(())
}

/// Give every Teams launch in this login session the debug port, however Teams is started (its
/// Dock icon, Spotlight, a link): launchd hands this variable to the apps it launches. It lasts
/// until logout; Theames sets it again when it starts at login. (Without it Theames still notices a
/// portless Teams and restarts it with the port, but that shows as a quick restart.)
pub fn set_session_env(port: u16) {
    let ok = Command::new("launchctl")
        .args(["setenv", "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", &crate::teams::browser_arg_env(port)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("[theames] could not set the Teams debug port for this session");
    }
}

/// Undo `set_session_env`.
pub fn clear_session_env() {
    let _ = Command::new("launchctl").args(["unsetenv", "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"]).status();
}

/// Undo `register_login_item`.
pub fn unregister_login_item() {
    if AnyClass::get(c"SMAppService").is_none() {
        return;
    }
    let service = unsafe { SMAppService::mainAppService() };
    match unsafe { service.unregisterAndReturnError() } {
        Ok(()) => println!("[theames] removed from Login Items"),
        Err(e) => eprintln!("[theames] could not remove from Login Items: {}", e.localizedDescription()),
    }
}

/// Path of the running Theames.app bundle.
pub fn bundle_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    Some(app.to_string_lossy().into_owned())
}

/// Add Theames to Login Items (SMAppService, macOS 13+), so it's running whenever Teams is. It is
/// listed under System Settings → General → Login Items, where it can be turned off. Not done when
/// running from a mounted disk image. True once registered.
pub fn register_login_item() -> bool {
    if AnyClass::get(c"SMAppService").is_none() {
        return false;
    }
    if std::env::current_exe().map(|p| p.starts_with("/Volumes")).unwrap_or(false) {
        return false;
    }
    let service = unsafe { SMAppService::mainAppService() };
    match unsafe { service.registerAndReturnError() } {
        Ok(()) => {
            println!("[theames] added to Login Items");
            true
        }
        Err(e) => {
            eprintln!("[theames] could not add to Login Items: {}", e.localizedDescription());
            false
        }
    }
}
