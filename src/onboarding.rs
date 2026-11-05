//! First run and permission check (macOS, Theames.app), before Teams is touched.
//!
//! Theames themes the Teams icon, which macOS only allows apps with the App Management permission
//! to do, and granting that makes macOS want Theames reopened. So Teams is only opened once the
//! icon can actually be written:
//! - first launch: a permission splash that sends the user to App Management — or skips the icon
//!   and carries on (the thank-you splash with the themes link comes later, once Theames is running
//!   in Teams: `welcome`);
//! - any launch with an icon still to apply: write it now (the real permission check). Teams opens
//!   only on success; otherwise explain and offer the settings.
//! After sending the user to App Management, Theames stays running without opening Teams: macOS
//! then offers to quit and reopen it once the permission is switched on, and the reopened Theames
//! carries on from there.
//! Runs on the main thread (it shows alerts) before the launcher's work starts.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSApplication, NSEventMask, NSLinkAttributeName, NSTextField};
use objc2_foundation::{NSAttributedString, NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSRect, NSSize, NSString, NSURL};
use std::time::{Duration, Instant};

use crate::{appicon, config};

const THEMES_URL: &str = "https://theames.poplel.xyz";
const SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles";

/// What to do after the preflight.
pub enum Next {
    /// Carry on: open Teams and theme it.
    Go,
    /// Quit now.
    Quit,
    /// Stay running without touching Teams while the user grants App Management (macOS offers to
    /// quit and reopen Theames when it's switched on).
    AwaitPermission,
}

pub fn preflight(mtm: MainThreadMarker) -> Next {
    let first_run = !config::config_path().exists();
    let Ok(mut cfg) = config::Config::load() else { return Next::Go };

    if first_run {
        let choice = alert(
            mtm,
            "Theames needs one permission",
            "Theames themes Microsoft Teams — including its app icon. macOS only lets apps you allow \
             change other apps, so turn on Theames under System Settings → Privacy & Security → \
             App Management. If Theames isn't in the list, click + and choose Theames from your Applications folder. macOS will then offer to quit and reopen Theames.",
            &["Open Settings", "Keep Teams' Own Icon"],
            false,
        );
        if choice == 0 {
            cfg.save().ok();
            open_url(SETTINGS_URL);
            return Next::AwaitPermission;
        }
        cfg.selection.icon = None;
        cfg.save().ok();
        return Next::Go;
    }

    // An icon to apply (first time after setup, or after a Teams update dropped it): do it now.
    let Some(icon) = cfg.selection.icon.clone() else { return Next::Go };
    match appicon::sync(&config::themes_dir(), Some(&icon), cfg.applied_icon.as_deref()) {
        Ok(applied) => {
            if applied != cfg.applied_icon {
                cfg.applied_icon = applied;
                cfg.save().ok();
            }
            Next::Go
        }
        Err(e) if e.to_string().to_lowercase().contains("cancel") => {
            println!("[theames] password prompt cancelled; not opening Teams");
            Next::Quit
        }
        Err(e) => {
            eprintln!("[theames] {e}");
            let choice = alert(
                mtm,
                "Theames can't change the Microsoft Teams icon yet",
                "Turn on Theames under System Settings → Privacy & Security → App Management. \
                 If Theames isn't in the list, click + and choose Theames from your Applications folder. macOS will then offer to quit and reopen Theames.",
                &["Open Settings", "Keep Teams' Own Icon", "Quit"],
                false,
            );
            match choice {
                0 => {
                    open_url(SETTINGS_URL);
                    Next::AwaitPermission
                }
                1 => {
                    cfg.selection.icon = None;
                    cfg.save().ok();
                    Next::Go
                }
                _ => Next::Quit,
            }
        }
    }
}

/// The one-time welcome, shown the first time Theames is up and running in Teams.
pub fn welcome(mtm: MainThreadMarker) {
    alert(
        mtm,
        "Thanks for downloading Theames!",
        "Theames is running. Open the Theames menu from Teams' ••• (Settings and more) menu to pick \
         themes, backgrounds and the app icon.",
        &["Got It"],
        true,
    );
}

/// Stay running (without Teams) while the user switches on App Management: macOS then offers to
/// quit and reopen Theames, which picks up from there. Theames has no Dock icon or window here, so
/// opening it again (which only activates this instance) brings the instructions back. Gives up
/// after 15 minutes. Never returns.
pub fn await_permission(mtm: MainThreadMarker) -> ! {
    println!("[theames] waiting for the App Management permission");
    let app = NSApplication::sharedApplication(mtm);
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    // The splash that sent the user to System Settings just closed, so we count as active already.
    let mut was_active = true;
    loop {
        if Instant::now() > deadline {
            std::process::exit(0);
        }
        // A normal event loop, so "Quit & Reopen" (an Apple event) and reopen clicks are handled.
        let until = NSDate::dateWithTimeIntervalSinceNow(0.5);
        let event = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), NSDefaultRunLoopMode, true)
        };
        if let Some(event) = event {
            app.sendEvent(&event);
        }
        let active = app.isActive();
        if active && !was_active {
            let choice = alert(
                mtm,
                "Theames is waiting for permission",
                &format!(
                    "Turn on Theames under System Settings → Privacy & Security → App Management. \
                     If Theames isn't in the list, click + and choose Theames from your Applications folder. macOS will then offer to quit and reopen Theames."
                ),
                &["Open Settings", "Quit Theames"],
                false,
            );
            if choice == 0 {
                open_url(SETTINGS_URL);
            } else {
                std::process::exit(0);
            }
        }
        was_active = app.isActive();
    }
}

/// A modal alert with Theames' icon; returns the index of the button pressed. `link` adds the
/// clickable "get more themes" line.
fn alert(mtm: MainThreadMarker, title: &str, text: &str, buttons: &[&str], link: bool) -> isize {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(text));
    for b in buttons {
        alert.addButtonWithTitle(&NSString::from_str(b));
    }
    if link {
        alert.setAccessoryView(Some(&themes_link(mtm)));
    }
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let r = alert.runModal();
    println!("[theames] alert \"{title}\" -> response {r}");
    r - NSAlertFirstButtonReturn
}

/// "Get more themes at theames.poplel.xyz", clickable.
fn themes_link(mtm: MainThreadMarker) -> Retained<NSTextField> {
    let url = NSURL::URLWithString(&NSString::from_str(THEMES_URL)).expect("valid URL");
    let attrs: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices(&[unsafe { NSLinkAttributeName }], &[&*url as &AnyObject]);
    let text = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str("Get more themes at theames.poplel.xyz"),
            Some(&attrs),
        )
    };
    let field = NSTextField::labelWithAttributedString(&text, mtm);
    field.setSelectable(true);
    field.setAllowsEditingTextAttributes(true);
    field.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 20.0)));
    field
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).status();
}
