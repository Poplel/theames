//! Theming the Microsoft Teams app icon (macOS).
//!
//! Uses the same call as Finder's "paste icon" (NSWorkspace setIcon:forFile:), which stores a custom
//! icon on the bundle (an `Icon\r` file plus Finder info). The Dock shows it, Teams' code signature
//! still verifies and Gatekeeper still accepts the app. Teams lives in a root-owned bundle, so when
//! the in-process call isn't allowed we re-run this binary as root through the standard macOS admin
//! prompt (`theames --set-app-icon <app> <image>` / `--clear-app-icon <app>`). macOS additionally
//! requires the App Management permission (System Settings → Privacy & Security) for Theames. A Teams update
//! replaces the bundle and drops the icon; the launcher notices and applies it again.

use anyhow::{anyhow, Result};
use objc2::AnyThread;
use objc2_app_kit::{NSImage, NSWorkspace, NSWorkspaceIconCreationOptions};
use objc2_foundation::NSString;
use std::ffi::{c_char, c_void, OsStr};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::theame::Theame;

const TEAMS_BUNDLE_ID: &str = "com.microsoft.teams2";

/// Where Microsoft Teams is installed.
pub fn teams_app() -> PathBuf {
    let ws = NSWorkspace::sharedWorkspace();
    ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(TEAMS_BUNDLE_ID))
        .and_then(|url| url.path())
        .map(|p| PathBuf::from(p.to_string()))
        .unwrap_or_else(|| PathBuf::from("/Applications/Microsoft Teams.app"))
}

/// Theames' own icon (`Theames.icns` in the app bundle), if we're running from Theames.app.
pub fn theames_icon() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let icns = exe.parent()?.parent()?.join("Resources").join("Theames.icns");
    icns.exists().then_some(icns)
}

/// Does the bundle currently carry a custom icon?
pub fn has_custom_icon(app: &Path) -> bool {
    app.join("Icon\r").exists()
}

/// Set (`Some(image)`) or remove (`None`) the custom icon in this process. False if not permitted.
pub fn set_icon_here(app: &Path, image: Option<&Path>) -> bool {
    let img = match image {
        Some(p) => match NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&p.to_string_lossy())) {
            Some(i) => Some(i),
            None => return false,
        },
        None => None,
    };
    let ws = NSWorkspace::sharedWorkspace();
    let path = NSString::from_str(&app.to_string_lossy());
    // Replacing a custom icon in place leaves macOS (LaunchServices, the Dock) showing the old one;
    // a freshly created custom icon is picked up. So remove the current one first.
    if img.is_some() && has_custom_icon(app) {
        ws.setIcon_forFile_options(None, &path, NSWorkspaceIconCreationOptions(0));
    }
    let ok = ws.setIcon_forFile_options(img.as_deref(), &path, NSWorkspaceIconCreationOptions(0));
    if ok {
        touch(app);
    }
    ok
}

/// macOS re-reads an app's icon only when the bundle folder looks changed. Replacing an existing
/// custom icon only rewrites the `Icon\r` file inside it, so without this the Dock and Finder keep
/// showing the previous icon (even after the Dock restarts).
fn touch(app: &Path) {
    if let Ok(dir) = std::fs::File::open(app) {
        let _ = dir.set_modified(std::time::SystemTime::now());
    }
}

/// Set or remove the custom icon, asking for an admin password only if the bundle isn't writable.
pub fn apply(app: &Path, image: Option<&Path>) -> Result<()> {
    if set_icon_here(app, image) {
        return Ok(());
    }
    let mut args: Vec<&OsStr> = Vec::new();
    match image {
        Some(img) => args.extend([OsStr::new("--set-app-icon"), app.as_os_str(), img.as_os_str()]),
        None => args.extend([OsStr::new("--clear-app-icon"), app.as_os_str()]),
    }
    run_helper_as_admin(&args)
}

/// Run this binary as root with `args`, through the standard macOS admin prompt (Authorization
/// Services), and wait for it. This must be Theames' own request: macOS credits a write into
/// another app's bundle to the *responsible* app, and only Theames holds App Management. (Asking via
/// `osascript ... with administrator privileges` made osascript responsible, so the write was refused.)
/// The helper prints OK on success; its stdout comes back over the pipe Authorization Services gives us.
fn run_helper_as_admin(args: &[&OsStr]) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let exe = std::env::current_exe()?;
    let tool = CString::new(exe.as_os_str().as_bytes())?;
    let owned: Vec<CString> = args.iter().map(|a| CString::new(a.as_bytes())).collect::<Result<_, _>>()?;
    let mut argv: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());

    let right_name = c"system.privilege.admin";
    let prompt = c"Theames needs your password to change the Microsoft Teams icon.";
    let mut right = auth::Item { name: right_name.as_ptr(), value_length: 0, value: std::ptr::null_mut(), flags: 0 };
    let rights = auth::ItemSet { count: 1, items: &mut right };
    let mut prompt_item = auth::Item {
        name: c"prompt".as_ptr(),
        value_length: prompt.to_bytes().len(),
        value: prompt.as_ptr() as *mut c_void,
        flags: 0,
    };
    let env = auth::ItemSet { count: 1, items: &mut prompt_item };

    // SAFETY: plain Authorization Services calls with valid, NUL-terminated buffers that outlive them;
    // the authorization ref is freed on every path.
    unsafe {
        let mut aref: auth::Ref = std::ptr::null_mut();
        let st = auth::AuthorizationCreate(std::ptr::null(), std::ptr::null(), 0, &mut aref);
        if st != 0 {
            return Err(anyhow!("AuthorizationCreate failed ({st})"));
        }
        let flags = auth::INTERACTION_ALLOWED | auth::EXTEND_RIGHTS | auth::PRE_AUTHORIZE;
        let st = auth::AuthorizationCopyRights(aref, &rights, &env, flags, std::ptr::null_mut());
        if st != 0 {
            auth::AuthorizationFree(aref, 0);
            return Err(if st == auth::ERR_CANCELED {
                anyhow!("the password prompt was cancelled")
            } else {
                anyhow!("authorization failed ({st})")
            });
        }
        let mut pipe: *mut c_void = std::ptr::null_mut();
        let st = auth::AuthorizationExecuteWithPrivileges(aref, tool.as_ptr(), 0, argv.as_ptr(), &mut pipe);
        auth::AuthorizationFree(aref, auth::DESTROY_RIGHTS);
        if st != 0 {
            return Err(anyhow!("could not run the admin helper ({st})"));
        }
        // Read the helper's output to the end: that also waits for it to finish.
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = auth::fread(buf.as_mut_ptr() as *mut c_void, 1, buf.len(), pipe);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        auth::fclose(pipe);
        let text = String::from_utf8_lossy(&out);
        if text.lines().any(|l| l.trim() == "OK") {
            Ok(())
        } else {
            Err(anyhow!("macOS refused to change the Teams icon ({})", text.trim()))
        }
    }
}

/// The few Authorization Services entry points we need (Security.framework).
mod auth {
    use std::ffi::{c_char, c_void};
    pub type Ref = *mut c_void;
    #[repr(C)]
    pub struct Item {
        pub name: *const c_char,
        pub value_length: usize,
        pub value: *mut c_void,
        pub flags: u32,
    }
    #[repr(C)]
    pub struct ItemSet {
        pub count: u32,
        pub items: *mut Item,
    }
    pub const INTERACTION_ALLOWED: u32 = 1 << 0;
    pub const EXTEND_RIGHTS: u32 = 1 << 1;
    pub const DESTROY_RIGHTS: u32 = 1 << 3;
    pub const PRE_AUTHORIZE: u32 = 1 << 4;
    pub const ERR_CANCELED: i32 = -60006;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        pub fn AuthorizationCreate(rights: *const ItemSet, env: *const ItemSet, flags: u32, out: *mut Ref) -> i32;
        pub fn AuthorizationCopyRights(auth: Ref, rights: *const ItemSet, env: *const ItemSet, flags: u32, out: *mut *mut ItemSet) -> i32;
        pub fn AuthorizationExecuteWithPrivileges(auth: Ref, tool: *const c_char, flags: u32, args: *const *const c_char, pipe: *mut *mut c_void) -> i32;
        pub fn AuthorizationFree(auth: Ref, flags: u32) -> i32;
    }
    unsafe extern "C" {
        pub fn fread(ptr: *mut c_void, size: usize, n: usize, stream: *mut c_void) -> usize;
        pub fn fclose(stream: *mut c_void) -> i32;
    }
}

/// Make the Teams icon match the selected theme icon (`icon` = the selected theme file, if any).
/// `applied` is what we last wrote; returns the new value to remember. The bundle is only touched
/// when needed: a different icon was chosen, or Teams lost ours (an update replaced the bundle).
/// A custom icon we didn't set (e.g. one pasted in Finder) is never removed.
pub fn sync(themes_dir: &Path, icon: Option<&str>, applied: Option<&str>) -> Result<Option<String>> {
    let app = teams_app();
    let present = has_custom_icon(&app);
    let wanted = match icon {
        Some(config::THEAMES_ICON) => match theames_icon() {
            Some(p) => Some((config::THEAMES_ICON, "Theames.icns".to_string(), std::fs::read(p)?)),
            None => None,
        },
        Some(file) => {
            let t = Theame::open(&themes_dir.join(file))?;
            match &t.manifest.icon {
                Some(name) => Some((file, name.clone(), t.read_bytes(name)?)),
                None => None,
            }
        }
        None => None,
    };
    let Some((file, name, bytes)) = wanted else {
        if present && applied.is_some() {
            apply(&app, None)?;
            println!("[theames] removed the theme icon from Teams");
            refresh_dock();
        }
        return Ok(None);
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let sig = format!("{file}:{:016x}", h.finish());
    if present && applied == Some(sig.as_str()) {
        return Ok(Some(sig));
    }
    let ext = name.rsplit('.').next().unwrap_or("png");
    let img = config::base_dir().join(format!("teams-icon.{ext}"));
    std::fs::write(&img, &bytes)?;
    apply(&app, Some(&img))?;
    println!("[theames] applied the {file} icon to {}", app.display());
    refresh_dock();
    Ok(Some(sig))
}


/// Make the Dock show the new icon. The Dock keeps its own icon cache
/// (`com.apple.dock.iconcache` in the per-user cache folder) and keeps drawing Teams' previous icon
/// from it — even after a restart, and whether or not Teams is running. Clearing that cache (macOS
/// rebuilds it) and restarting the Dock is what makes it re-read the icon.
fn refresh_dock() {
    if let Ok(out) = Command::new("getconf").arg("DARWIN_USER_CACHE_DIR").output() {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !dir.is_empty() {
            let _ = std::fs::remove_file(Path::new(&dir).join("com.apple.dock.iconcache"));
        }
    }
    let _ = Command::new("killall").arg("Dock").status();
}

/// Tell the user why the icon couldn't be changed. Cancelling the password prompt is not an error
/// worth an alert; anything else is almost always the App Management permission, so offer to open it.
pub fn explain_failure(err: &anyhow::Error) {
    let msg = err.to_string();
    eprintln!("[theames] {msg}");
    if msg.to_lowercase().contains("cancel") {
        return;
    }
    let out = Command::new("osascript")
        .args([
            "-e", "set r to display alert \"Theames couldn't change the Microsoft Teams icon\" message \"macOS only lets apps you allow modify other apps. Turn on Theames under System Settings → Privacy & Security → App Management, then choose the icon again.\" buttons {\"Not Now\", \"Open Settings\"} default button \"Open Settings\"",
            "-e", "button returned of r",
        ])
        .output();
    if let Ok(o) = out {
        if String::from_utf8_lossy(&o.stdout).trim() == "Open Settings" {
            let _ = Command::new("open")
                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles")
                .status();
        }
    }
}

/// Entry point for the elevated helper invocation. Returns the process exit code.
pub fn helper_main(args: &[String]) -> Option<i32> {
    let ok = match args {
        [flag, app, img] if flag == "--set-app-icon" => set_icon_here(Path::new(app), Some(Path::new(img))),
        [flag, app] if flag == "--clear-app-icon" => set_icon_here(Path::new(app), None),
        _ => return None,
    };
    println!("{}", if ok { "OK" } else { "FAILED: setIcon was refused" });
    Some(if ok { 0 } else { 1 })
}
