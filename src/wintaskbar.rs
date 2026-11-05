//! Windows: Teams' taskbar button wears the Theames icon, as on macOS.
//!
//! The taskbar groups windows, and picks the button's icon, by each window's AppUserModelID and
//! its relaunch properties (Windows' documented per-window properties, set through
//! SHGetPropertyStoreForWindow). Theames gives every Teams top-level window its own ID plus the
//! selected icon as the relaunch icon, and gives the Theames shortcut the same ID, so Teams'
//! windows show up under the (pinned) Theames button with the themed icon. A watcher thread keeps
//! new windows (pop-out chats, a restarted Teams) stamped; `release` undoes it.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

/// The ID Teams' windows and the Theames shortcut share.
pub const AUMID: &str = "Theames.MicrosoftTeams";

/// The icon to stamp ("path,index"), or none once uninstalling.
static ICON: Mutex<Option<String>> = Mutex::new(None);

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid(u32, u16, u16, [u8; 8]);
#[repr(C)]
struct PropKey {
    fmtid: Guid,
    pid: u32,
}
/// PROPVARIANT (24 bytes on 64-bit): only VT_EMPTY and VT_LPWSTR are used.
#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    ptr: *const u16,
    pad: usize,
}

const FMTID_APPUSERMODEL: Guid = Guid(0x9F4C2855, 0x9F79, 0x4B39, [0xA8, 0xD0, 0xE1, 0xD4, 0x2D, 0xE1, 0xD5, 0xF3]);
const PID_RELAUNCH_COMMAND: u32 = 2;
const PID_RELAUNCH_ICON: u32 = 3;
const PID_RELAUNCH_NAME: u32 = 4;
const PID_ID: u32 = 5;
const IID_IPROPERTYSTORE: Guid = Guid(0x886D8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
const CLSID_SHELLLINK: Guid = Guid(0x00021401, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_ISHELLLINKW: Guid = Guid(0x000214F9, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IPERSISTFILE: Guid = Guid(0x0000010B, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    fn CoCreateInstance(clsid: *const Guid, outer: *mut c_void, ctx: u32, iid: *const Guid, out: *mut *mut c_void) -> i32;
}
#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetPropertyStoreForWindow(hwnd: isize, iid: *const Guid, out: *mut *mut c_void) -> i32;
    fn ExtractIconExW(file: *const u16, index: i32, large: *mut isize, small: *mut isize, count: u32) -> u32;
    fn SetCurrentProcessExplicitAppUserModelID(id: *const u16) -> i32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn GetWindow(hwnd: isize, cmd: u32) -> isize;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn QueryFullProcessImageNameW(process: isize, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Call a COM method by vtable slot.
unsafe fn slot<T>(obj: *mut c_void, index: usize) -> T {
    unsafe {
        let vtbl = *(obj as *const *const usize);
        std::mem::transmute_copy(&*vtbl.add(index))
    }
}

unsafe fn com_release(obj: *mut c_void) {
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void) -> u32 = slot(obj, 2);
        f(obj);
    }
}

/// IPropertyStore::SetValue (slot 6) for one AppUserModel property; `None` clears it.
unsafe fn set_prop(store: *mut c_void, pid: u32, value: Option<&[u16]>) -> i32 {
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void, *const PropKey, *const PropVariant) -> i32 = slot(store, 6);
        let key = PropKey { fmtid: FMTID_APPUSERMODEL, pid };
        let pv = match value {
            Some(w) => PropVariant { vt: 31, reserved: [0; 3], ptr: w.as_ptr(), pad: 0 },
            None => PropVariant { vt: 0, reserved: [0; 3], ptr: std::ptr::null(), pad: 0 },
        };
        f(store, &key, &pv)
    }
}

unsafe fn commit(store: *mut c_void) {
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void) -> i32 = slot(store, 7);
        f(store);
    }
}

/// Start keeping Teams' windows on the Theames button, with `icon` ("path,index").
pub fn start(icon: String) {
    *ICON.lock().unwrap() = Some(icon);
    // SAFETY: plain call with a NUL-terminated string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(wide(AUMID).as_ptr()) };
    std::thread::Builder::new()
        .name("teams-taskbar".into())
        .spawn(watch)
        .ok();
}

/// Use a different icon (the user picked one in the panel); open windows are re-stamped.
pub fn set_icon(icon: String) {
    *ICON.lock().unwrap() = Some(icon);
}

/// Give Teams' windows back their own taskbar button and icon (uninstall).
pub fn release() {
    *ICON.lock().unwrap() = None;
    // SAFETY: COM/user32 calls on windows we enumerate; failures are ignored.
    unsafe {
        CoInitializeEx(std::ptr::null_mut(), 0x2);
        for hwnd in teams_windows() {
            stamp(hwnd, None);
        }
    }
}

fn watch() {
    // SAFETY: as above.
    unsafe { CoInitializeEx(std::ptr::null_mut(), 0x2) };
    let mut stamped: HashMap<isize, String> = HashMap::new();
    loop {
        let Some(icon) = ICON.lock().unwrap().clone() else { return };
        stamped.retain(|hwnd, _| unsafe { IsWindow(*hwnd) } != 0);
        for hwnd in teams_windows() {
            if stamped.get(&hwnd) != Some(&icon) {
                unsafe { stamp(hwnd, Some(&icon)) };
                stamped.insert(hwnd, icon.clone());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
}

/// Teams' visible, unowned top-level windows (the main window and pop-outs).
fn teams_windows() -> Vec<isize> {
    extern "system" fn collect(hwnd: isize, out: isize) -> i32 {
        // SAFETY: `out` is the Vec passed to EnumWindows below, alive for the whole call.
        unsafe {
            if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, 4) == 0 && is_teams(hwnd) {
                (*(out as *mut Vec<isize>)).push(hwnd);
            }
        }
        1
    }
    let mut found: Vec<isize> = Vec::new();
    // SAFETY: the callback only pushes into `found`.
    unsafe { EnumWindows(collect, &mut found as *mut Vec<isize> as isize) };
    found
}

unsafe fn is_teams(hwnd: isize) -> bool {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(0x1000, 0, pid);
        if process == 0 {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        ok && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase().ends_with("\\ms-teams.exe")
    }
}

/// Put one Teams window on the Theames button with `icon`, or (None) give it back to Teams.
unsafe fn stamp(hwnd: isize, icon: Option<&str>) {
    unsafe {
        let mut store: *mut c_void = std::ptr::null_mut();
        if SHGetPropertyStoreForWindow(hwnd, &IID_IPROPERTYSTORE, &mut store) < 0 || store.is_null() {
            return;
        }
        // Clearing the ID first makes the taskbar re-read the button when only the icon changed.
        set_prop(store, PID_ID, None);
        commit(store);
        match icon {
            Some(icon) => {
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
                let (cmd, name, ico, id) = (wide(&format!("\"{exe}\"")), wide("Microsoft Teams"), wide(icon), wide(AUMID));
                set_prop(store, PID_RELAUNCH_COMMAND, Some(&cmd));
                set_prop(store, PID_RELAUNCH_NAME, Some(&name));
                set_prop(store, PID_RELAUNCH_ICON, Some(&ico));
                set_prop(store, PID_ID, Some(&id));
                commit(store);
                set_window_icon(hwnd, icon);
            }
            None => {
                for pid in [PID_RELAUNCH_COMMAND, PID_RELAUNCH_NAME, PID_RELAUNCH_ICON] {
                    set_prop(store, pid, None);
                }
                commit(store);
                SendMessageW(hwnd, 0x0080, 1, 0); // WM_SETICON big -> default
                SendMessageW(hwnd, 0x0080, 0, 0); // small
            }
        }
        com_release(store);
    }
}

/// The window's own icon (title bar, Alt-Tab, thumbnails).
unsafe fn set_window_icon(hwnd: isize, icon: &str) {
    unsafe {
        let (path, index) = match icon.rsplit_once(',') {
            Some((p, i)) => (p, i.trim().parse::<i32>().unwrap_or(0)),
            None => (icon, 0),
        };
        let (mut large, mut small) = (0isize, 0isize);
        if ExtractIconExW(wide(path).as_ptr(), index, &mut large, &mut small, 1) > 0 {
            if large != 0 {
                SendMessageW(hwnd, 0x0080, 1, large);
            }
            if small != 0 {
                SendMessageW(hwnd, 0x0080, 0, small);
            }
        }
    }
}

/// Give a shortcut the shared ID, so the running Teams windows fold into it on the taskbar.
pub fn set_shortcut_id(lnk: &Path) -> bool {
    // SAFETY: COM calls on objects we create; every object is released.
    unsafe {
        CoInitializeEx(std::ptr::null_mut(), 0x2);
        let mut link: *mut c_void = std::ptr::null_mut();
        if CoCreateInstance(&CLSID_SHELLLINK, std::ptr::null_mut(), 0x1, &IID_ISHELLLINKW, &mut link) < 0 || link.is_null() {
            return false;
        }
        let qi: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 = slot(link, 0);
        let (mut file, mut store): (*mut c_void, *mut c_void) = (std::ptr::null_mut(), std::ptr::null_mut());
        let mut ok = false;
        if qi(link, &IID_IPERSISTFILE, &mut file) >= 0 && !file.is_null() {
            let load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32 = slot(file, 5);
            let path = wide(&lnk.display().to_string());
            if load(file, path.as_ptr(), 2) >= 0 && qi(link, &IID_IPROPERTYSTORE, &mut store) >= 0 && !store.is_null() {
                let id = wide(AUMID);
                ok = set_prop(store, PID_ID, Some(&id)) >= 0;
                commit(store);
                let save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32 = slot(file, 6);
                ok = ok && save(file, std::ptr::null(), 1) >= 0;
                com_release(store);
            }
            com_release(file);
        }
        com_release(link);
        ok
    }
}
