//! Windows: the themed "Theames" shortcut and the welcome message.
//!
//! Teams on Windows is a packaged app whose icon can't be changed. So Theames keeps a "Theames"
//! shortcut (Start menu, and the desktop on first setup) whose icon follows the selected theme icon,
//! pins it to the taskbar on first setup (`pin_to_taskbar`), and keeps the pinned copy in step. Shortcuts are written with Windows' own shortcut object
//! (WScript.Shell, via PowerShell), paths passed in environment variables, never in the script.

use anyhow::{anyhow, Result};
use std::hash::{Hash, Hasher};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::theame::Theame;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const THEMES_URL: &str = "https://theames.poplel.xyz";
const SHORTCUT: &str = "Theames.lnk";

fn appdata() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

fn start_menu_shortcut() -> Option<PathBuf> {
    Some(appdata()?.join(r"Microsoft\Windows\Start Menu\Programs").join(SHORTCUT))
}

fn pinned_shortcut() -> Option<PathBuf> {
    Some(appdata()?.join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar").join(SHORTCUT))
}

fn desktop_shortcut() -> Option<PathBuf> {
    Some(dirs::desktop_dir()?.join(SHORTCUT))
}

/// Bring the Theames shortcuts in line with the selected icon (`icon`: a theme file, `@theames`, or
/// none). `first_setup` also puts one on the desktop. The pinned taskbar copy is only updated.
pub fn sync_shortcuts(themes_dir: &Path, icon: Option<&str>, first_setup: bool) -> Result<String> {
    let exe = std::env::current_exe()?;
    let icon_location = icon_location(themes_dir, icon, &exe)?;
    let mut targets: Vec<PathBuf> = start_menu_shortcut().into_iter().collect();
    if let Some(desktop) = desktop_shortcut() {
        if first_setup || desktop.exists() {
            targets.push(desktop);
        }
    }
    if let Some(pinned) = pinned_shortcut().filter(|p| p.exists()) {
        targets.push(pinned);
    }
    for lnk in &targets {
        write_shortcut(lnk, &exe, &icon_location)?;
        // Same ID as Teams' windows, so they fold into this (pinned) button.
        crate::wintaskbar::set_shortcut_id(lnk);
    }
    refresh_icons();
    println!("[theames] shortcuts updated ({icon_location})");
    Ok(icon_location)
}

/// Remove the shortcuts Theames made (call `unpin_from_taskbar` first for the pinned one).
pub fn remove_shortcuts() {
    for lnk in [start_menu_shortcut(), desktop_shortcut(), pinned_shortcut()].into_iter().flatten() {
        let _ = std::fs::remove_file(lnk);
    }
    let _ = std::fs::remove_dir_all(config::base_dir().join("icons"));
}

/// "path,index" for the shortcut: Theames' own icon is in the exe; a theme icon becomes an .ico
/// whose name changes with its content, so Windows' icon cache can't hand out an old one.
fn icon_location(themes_dir: &Path, icon: Option<&str>, exe: &Path) -> Result<String> {
    let own = format!("{},0", exe.display());
    let Some(file) = icon.filter(|f| *f != config::THEAMES_ICON) else { return Ok(own) };
    let t = Theame::open(&themes_dir.join(file))?;
    let Some(name) = &t.manifest.icon else { return Ok(own) };
    let png = t.read_bytes(name)?;
    let ico = png_to_ico(&png)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ico.hash(&mut h);
    let dir = config::base_dir().join("icons");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:016x}.ico", h.finish()));
    if !path.exists() {
        std::fs::write(&path, &ico)?;
    }
    Ok(format!("{},0", path.display()))
}

/// An .ico with PNG images at 256/48/32/16 px.
fn png_to_ico(png: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(png).map_err(|e| anyhow!("theme icon isn't a readable PNG: {e}"))?;
    let sizes = [256u32, 48, 32, 16];
    let mut images = Vec::new();
    for s in sizes {
        let mut buf = Vec::new();
        img.resize_exact(s, s, image::imageops::FilterType::Lanczos3)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .map_err(|e| anyhow!("encoding the icon failed: {e}"))?;
        images.push((s, buf));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    Ok(out)
}

fn write_shortcut(lnk: &Path, exe: &Path, icon_location: &str) -> Result<()> {
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let script = "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:THEAMES_LNK); \
                  $s.TargetPath = $env:THEAMES_TARGET; $s.WorkingDirectory = $env:THEAMES_DIR; \
                  $s.IconLocation = $env:THEAMES_ICONLOC; $s.Description = 'Microsoft Teams, themed by Theames'; $s.Save()";
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("THEAMES_LNK", lnk)
        .env("THEAMES_TARGET", exe)
        .env("THEAMES_DIR", exe.parent().unwrap_or(Path::new(".")))
        .env("THEAMES_ICONLOC", icon_location)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if !status.success() {
        return Err(anyhow!("couldn't write the shortcut {}", lnk.display()));
    }
    Ok(())
}

/// Ask Explorer to redraw icons (taskbar included).
fn refresh_icons() {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHChangeNotify(event: i32, flags: u32, item1: *const std::ffi::c_void, item2: *const std::ffi::c_void);
    }
    const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
    // SAFETY: documented call with no items.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null()) };
    let _ = Command::new("ie4uinit.exe").arg("-show").creation_flags(CREATE_NO_WINDOW).status();
}

/// The one-time welcome, the first time Theames is up and running in Teams.
pub fn welcome() {
    #[repr(C)]
    struct MsgBoxParams {
        size: u32,
        owner: *mut std::ffi::c_void,
        instance: *mut std::ffi::c_void,
        text: *const u16,
        caption: *const u16,
        style: u32,
        icon: *const u16,
        help_id: usize,
        callback: *const std::ffi::c_void,
        language: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxIndirectW(params: *const MsgBoxParams) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let text = wide(
        "Thanks for downloading Theames!\n\n\
         Theames is running. Open the Theames menu from Teams' \u{2022}\u{2022}\u{2022} (Settings and more) menu \
         to pick themes, backgrounds and the icon.\n\n\
         Theames is on your taskbar: use it to start Teams with Theames. Its icon changes with your theme. \
         (If it isn't there, right-click Theames in the Start menu \u{2192} Pin to taskbar.)\n\n\
         Get more themes at theames.poplel.xyz \u{2014} open it now?",
    );
    let caption = wide("Theames");
    const MB_YESNO: u32 = 0x4;
    const MB_USERICON: u32 = 0x80;
    const IDYES: i32 = 6;
    // SAFETY: valid, NUL-terminated buffers that outlive the call; icon = the exe's icon group 1.
    let answer = unsafe {
        let params = MsgBoxParams {
            size: std::mem::size_of::<MsgBoxParams>() as u32,
            owner: std::ptr::null_mut(),
            instance: GetModuleHandleW(std::ptr::null()),
            text: text.as_ptr(),
            caption: caption.as_ptr(),
            style: MB_YESNO | MB_USERICON,
            icon: 1 as *const u16,
            help_id: 0,
            callback: std::ptr::null(),
            language: 0,
        };
        MessageBoxIndirectW(&params)
    };
    if answer == IDYES {
        let _ = Command::new("explorer").arg(THEMES_URL).creation_flags(CREATE_NO_WINDOW).status();
    }
}

/// Pin the Start-menu shortcut to the taskbar (first setup only). Windows has no public API for
/// this, so try, in order: the shortcut's own "Pin to taskbar" shell verb, then the taskbar's pin
/// interface in a separate helper process (so nothing it does can take Theames down). Success =
/// the pinned copy shows up in the taskbar's pin folder.
pub fn pin_to_taskbar() -> bool {
    let (Some(lnk), Some(pinned)) = (start_menu_shortcut(), pinned_shortcut()) else { return false };
    if pinned.exists() {
        return true;
    }
    let pinned_now = || {
        for _ in 0..20 {
            if pinned.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        false
    };
    let verb = "$p = $env:THEAMES_LNK; $i = (New-Object -ComObject Shell.Application).Namespace((Split-Path $p)).ParseName((Split-Path $p -Leaf)); \
                $v = $i.Verbs() | Where-Object { ($_.Name -replace '&','') -match 'taskbar' -and ($_.Name -replace '&','') -notmatch 'unpin' }; \
                if ($v) { $v | Select-Object -First 1 | ForEach-Object { $_.DoIt() } } else { $i.InvokeVerb('taskbarpin') }";
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", verb])
        .env("THEAMES_LNK", &lnk)
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    if pinned_now() {
        println!("[theames] pinned to the taskbar (shell verb)");
        return true;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).arg("--pin-taskbar").arg(&lnk).creation_flags(CREATE_NO_WINDOW).status();
    }
    if pinned_now() {
        println!("[theames] pinned to the taskbar");
        return true;
    }
    eprintln!("[theames] couldn't pin to the taskbar automatically");
    false
}

/// `Theames.exe --pin-taskbar <shortcut>`: pin through the taskbar's own pin list (the COM object
/// Explorer uses, `IPinnedList3::Modify`). Undocumented, hence run as a throwaway helper process.
/// `--unpin-taskbar <shortcut>` does the reverse.
pub fn pin_helper(args: &[String]) -> Option<i32> {
    let [flag, lnk] = args else { return None };
    let pin = match flag.as_str() {
        "--pin-taskbar" => true,
        "--unpin-taskbar" => false,
        _ => return None,
    };
    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);
    const CLSID_TASKBAND_PIN: Guid = Guid(0x90AA3A4E, 0x1CBA, 0x4233, [0xB8, 0xBB, 0x53, 0x57, 0x73, 0xD4, 0x84, 0x49]);
    const IID_PINNED_LIST3: Guid = Guid(0x0DD79AE2, 0xD156, 0x45D4, [0x9E, 0xEB, 0x3B, 0x54, 0x97, 0x69, 0xE9, 0x40]);
    const PLMC_EXPLORER: i32 = 4;
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut std::ffi::c_void, flags: u32) -> i32;
        fn CoCreateInstance(clsid: *const Guid, outer: *mut std::ffi::c_void, ctx: u32, iid: *const Guid, out: *mut *mut std::ffi::c_void) -> i32;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ILCreateFromPathW(path: *const u16) -> *mut std::ffi::c_void;
        fn ILFree(pidl: *mut std::ffi::c_void);
    }
    type Modify = unsafe extern "system" fn(*mut std::ffi::c_void, *const std::ffi::c_void, *const std::ffi::c_void, i32) -> i32;
    type Release = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let wide: Vec<u16> = lnk.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: COM calls on a freshly created object; Modify is slot 16 of IPinnedList3's vtable
    // (IUnknown's 3, then EnumObjects .. GetPinnedItemForAppID). A mismatch would only end this
    // helper process.
    let hr = unsafe {
        CoInitializeEx(std::ptr::null_mut(), 0x2);
        let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(&CLSID_TASKBAND_PIN, std::ptr::null_mut(), 0x1, &IID_PINNED_LIST3, &mut obj);
        if hr < 0 || obj.is_null() {
            hr
        } else {
            let pidl = ILCreateFromPathW(wide.as_ptr());
            let vtbl = *(obj as *const *const usize);
            let modify: Modify = std::mem::transmute(*vtbl.add(16));
            let hr = if pidl.is_null() {
                -1
            } else if pin {
                modify(obj, std::ptr::null(), pidl, PLMC_EXPLORER)
            } else {
                modify(obj, pidl, std::ptr::null(), PLMC_EXPLORER)
            };
            if !pidl.is_null() {
                ILFree(pidl);
            }
            let release: Release = std::mem::transmute(*vtbl.add(2));
            release(obj);
            hr
        }
    };
    Some(if hr >= 0 { 0 } else { 1 })
}

/// Take Theames off the taskbar (uninstall): the pin list first, the shell verb as a fallback.
pub fn unpin_from_taskbar() {
    let Some(pinned) = pinned_shortcut().filter(|p| p.exists()) else { return };
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).arg("--unpin-taskbar").arg(&pinned).creation_flags(CREATE_NO_WINDOW).status();
    }
    if pinned.exists() {
        let verb = "$p = $env:THEAMES_LNK; $i = (New-Object -ComObject Shell.Application).Namespace((Split-Path $p)).ParseName((Split-Path $p -Leaf)); \
                    $v = $i.Verbs() | Where-Object { ($_.Name -replace '&','') -match 'unpin' -and ($_.Name -replace '&','') -match 'taskbar' }; \
                    if ($v) { $v | Select-Object -First 1 | ForEach-Object { $_.DoIt() } } else { $i.InvokeVerb('taskbarunpin') }";
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", verb])
            .env("THEAMES_LNK", &pinned)
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Theames";

/// List Theames under Settings → Apps → Installed apps (per user, no admin needed), with an
/// Uninstall button that runs `Theames.exe --uninstall`.
pub fn register_uninstall_entry() {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe_s = exe.display().to_string();
    let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let values: [(&str, String); 8] = [
        ("DisplayName", "Theames".into()),
        ("DisplayVersion", env!("CARGO_PKG_VERSION").into()),
        ("Publisher", "Theames".into()),
        ("DisplayIcon", format!("{exe_s},0")),
        ("InstallLocation", dir),
        ("UninstallString", format!("\"{exe_s}\" --uninstall")),
        ("QuietUninstallString", format!("\"{exe_s}\" --uninstall")),
        ("URLInfoAbout", THEMES_URL.into()),
    ];
    // SAFETY: plain registry calls with NUL-terminated UTF-16 buffers; the key is closed.
    unsafe {
        let mut key: isize = 0;
        if reg::RegCreateKeyExW(reg::HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr(), 0, std::ptr::null(), 0, reg::KEY_WRITE, std::ptr::null(), &mut key, std::ptr::null_mut()) != 0 {
            return;
        }
        for (name, value) in &values {
            let data = wide(value);
            reg::RegSetValueExW(key, wide(name).as_ptr(), 0, reg::REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32);
        }
        for name in ["NoModify", "NoRepair"] {
            let one: u32 = 1;
            reg::RegSetValueExW(key, wide(name).as_ptr(), 0, reg::REG_DWORD, &one as *const u32 as *const u8, 4);
        }
        reg::RegCloseKey(key);
    }
}

pub fn remove_uninstall_entry() {
    // SAFETY: deletes our own key under HKCU.
    unsafe {
        reg::RegDeleteTreeW(reg::HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr());
    }
}

/// Remove Theames.exe once this process has exited (a running exe can't delete itself).
pub fn delete_exe_after_exit() {
    let Ok(exe) = std::env::current_exe() else { return };
    let cmd = format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", exe.display());
    let _ = Command::new("cmd").arg("/c").raw_arg(&cmd).creation_flags(CREATE_NO_WINDOW | 0x0000_0008).spawn();
}

/// A plain Theames message box.
pub fn message(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let (t, c) = (wide(text), wide("Theames"));
    // SAFETY: NUL-terminated buffers that outlive the call. MB_ICONINFORMATION = 0x40.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), 0x40);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The registry calls we need (advapi32).
mod reg {
    pub const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    pub const KEY_WRITE: u32 = 0x20006;
    pub const REG_SZ: u32 = 1;
    pub const REG_DWORD: u32 = 4;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn RegCreateKeyExW(key: isize, subkey: *const u16, reserved: u32, class: *const u16, options: u32, sam: u32, security: *const std::ffi::c_void, result: *mut isize, disposition: *mut u32) -> i32;
        pub fn RegSetValueExW(key: isize, name: *const u16, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32;
        pub fn RegCloseKey(key: isize) -> i32;
        pub fn RegDeleteTreeW(key: isize, subkey: *const u16) -> i32;
    }
}
