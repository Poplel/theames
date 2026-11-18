//! Theames launcher: opens Teams with its WebView2 debug port, injects the
//! bootstrap UI + selected theme into every Teams page (now and on reload),
//! serves the in-Teams manager panel over a CDP binding, and stays running as
//! the bridge until Teams closes.

// Headless on Windows: no console window (the app has no UI of its own).
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod appicon;
mod bridge;
mod cdp;
mod config;
#[cfg(target_os = "macos")]
mod dock;
mod inject;
mod instance;
mod teams;
mod theame;
mod update;
#[cfg(windows)]
mod winshell;
#[cfg(windows)]
mod wintaskbar;
#[cfg(target_os = "macos")]
mod onboarding;
#[cfg(any(target_os = "macos", windows))]
mod uninstall;

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use bridge::Shared;
use cdp::Cdp;
use config::Config;

/// Injected into every page: sets up the bridge, launcher button, and panel.
const BOOTSTRAP: &str = include_str!("bootstrap.js");

/// Name of the CDP binding the page calls to reach Theames.
const BINDING: &str = "__theamesSend";

type Sessions = Arc<Mutex<HashMap<String, String>>>;

/// The launcher's shared state, reachable from page watchers (for the one-time welcome).
static SHARED: std::sync::OnceLock<Arc<Shared>> = std::sync::OnceLock::new();
/// Set while the welcome splash is still to be shown (macOS, Theames.app, first time in Teams).
static WELCOME_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Page targets that currently exist, and the ones an attach loop is already working on.
#[derive(Default)]
struct PageState {
    live: HashSet<String>,
    attaching: HashSet<String>,
}
type Pages = Arc<Mutex<PageState>>;

fn main() -> Result<()> {
    // Elevated helper run (from the admin prompt) that only sets/clears the Teams icon.
    #[cfg(target_os = "macos")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(code) = appicon::helper_main(&args) {
            std::process::exit(code);
        }
    }
    // Windows: throwaway helper that pins the shortcut through the taskbar's pin list.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(code) = winshell::pin_helper(&args) {
            std::process::exit(code);
        }
    }
    redirect_output_to_log();
    println!("[theames] --- start v{} ---", env!("CARGO_PKG_VERSION"));
    update::startup();
    #[cfg(any(target_os = "macos", windows))]
    if std::env::args().any(|a| a == "--uninstall") {
        return uninstall::run();
    }
    // From Theames.app on macOS the system needs a real application on the main thread.
    #[cfg(target_os = "macos")]
    if dock::in_bundle() {
        return dock::run(run_blocking);
    }
    #[cfg(windows)]
    {
        let res = run_blocking();
        if let Err(e) = &res {
            eprintln!("[theames] fatal: {e:#}");
            show_error(&format!("Theames couldn't start:\n\n{e:#}"));
        }
        return res;
    }
    #[cfg(not(windows))]
    run_blocking()
}

fn run_blocking() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

/// Launched from Finder / the Dock there is no terminal, so everything the launcher prints
/// would vanish. Send stdout/stderr to `theames.log` in the data dir instead (a terminal keeps
/// its output). The log starts over once it passes 1 MB.
#[cfg(unix)]
fn redirect_output_to_log() {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    if std::io::stdout().is_terminal() {
        return;
    }
    let dir = config::base_dir();
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("theames.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        std::fs::remove_file(&path).ok();
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    unsafe extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
    }
    // SAFETY: plain dup2 of a valid, open fd onto stdout/stderr; the duplicates outlive `file`.
    unsafe {
        dup2(file.as_raw_fd(), 1);
        dup2(file.as_raw_fd(), 2);
    }
}

/// Windows: the launcher has no console (windows_subsystem), so send stdout/stderr to
/// `theames.log` in the data dir. Rust looks the standard handles up on every write, so pointing
/// them at the log file is enough. The log starts over once it passes 1 MB.
#[cfg(windows)]
fn redirect_output_to_log() {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    let dir = config::base_dir();
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("theames.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        std::fs::remove_file(&path).ok();
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    // SAFETY: valid handle of an open file, kept open for the life of the process (forgotten).
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle() as _);
        SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle() as _);
    }
    std::mem::forget(file);
}

/// Windows: show a fatal error, since there's no console to print it to.
#[cfg(windows)]
fn show_error(message: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(message), wide("Theames"));
    // SAFETY: NUL-terminated UTF-16 buffers that outlive the call. MB_ICONERROR = 0x10.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10);
    }
}

async fn run() -> Result<()> {
    // One launcher at a time: take over from any running one before touching Teams.
    let _instance = instance::acquire()?;
    #[allow(unused_mut)]
    let mut config = Config::load()?;
    let themes_dir = config::themes_dir();
    std::fs::create_dir_all(&themes_dir).ok();
    println!(
        "[theames] themes dir: {} ({} installed)",
        themes_dir.display(),
        theame::list(&themes_dir).len()
    );

    // From Theames.app on macOS, Theames is a background app that stays running between Teams
    // sessions and re-attaches whenever Teams comes back.
    #[cfg(target_os = "macos")]
    let resident = dock::in_bundle();
    #[cfg(not(target_os = "macos"))]
    let resident = false;
    #[cfg(target_os = "macos")]
    if resident {
        dock::set_session_env(config.debug_port);
        let here = dock::bundle_path();
        if here.is_some() && config.login_item_path != here && dock::register_login_item() {
            config.login_item_path = here;
            config.save().ok();
        }
    }

    WELCOME_PENDING.store((resident || cfg!(windows)) && !config.welcomed, std::sync::atomic::Ordering::SeqCst);

    // Windows: keep the themed "Theames" shortcut(s) in step (desktop one on first setup).
    #[cfg(windows)]
    {
        match winshell::sync_shortcuts(&themes_dir, config.selection.icon.as_deref(), !config.welcomed) {
            // Teams' taskbar button wears the same icon, under the Theames button.
            Ok(icon) => wintaskbar::start(icon),
            Err(e) => eprintln!("[theames] shortcuts: {e}"),
        }
        winshell::register_uninstall_entry();
        // Pin it to the taskbar once, on first setup.
        if !config.pin_attempted {
            winshell::pin_to_taskbar();
            config.pin_attempted = true;
            config.save().ok();
        }
    }
    let shared = Arc::new(Shared {
        config: Mutex::new(config),
        themes_dir,
        update: Mutex::new(update::Status::default()),
        reapply: tokio::sync::Notify::new(),
    });
    let _ = SHARED.set(shared.clone());
    tokio::spawn(update::run(shared.clone()));

    // Before Teams starts, so its Dock icon comes up themed.
    #[cfg(target_os = "macos")]
    bridge::sync_teams_icon(shared.clone()).await;

    let mut browser_ws = {
        let cfg = shared.config.lock().await;
        teams::ensure_running(&cfg).await?
    };
    loop {
        serve(&shared, &browser_ws).await?;
        if !resident {
            break;
        }
        println!("[theames] Teams closed; waiting for it to start again.");
        browser_ws = wait_for_teams(&shared).await;
        // An update may have replaced the bundle (and dropped the icon) while Teams was closed.
        #[cfg(target_os = "macos")]
        bridge::sync_teams_icon(shared.clone()).await;
    }
    println!("[theames] Teams closed; exiting.");
    Ok(())
}

/// Wait for Teams to run again and return its browser WebSocket URL. If Teams starts without the
/// debug port (e.g. from its own Dock icon), restart it with the port, as on first launch.
async fn wait_for_teams(shared: &Arc<Shared>) -> String {
    let cfg = teams::config_clone(&*shared.config.lock().await);
    let port = cfg.debug_port;
    let mut portless_ticks = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Ok(Ok(ws)) = tokio::task::spawn_blocking(move || teams::browser_ws_url(port)).await {
            println!("[theames] Teams is back on port {port}.");
            return ws;
        }
        let running = tokio::task::spawn_blocking({
            let name = cfg.mac.process_name.clone();
            move || std::process::Command::new("pgrep").args(["-x", &name]).stdout(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        portless_ticks = if running { portless_ticks + 1 } else { 0 };
        // Give a starting Teams a few seconds to open the port before deciding it has none.
        if portless_ticks >= 4 {
            println!("[theames] Teams started without the debug port; restarting it with the port.");
            if let Ok(Ok(ws)) = {
                let c = teams::config_clone(&cfg);
                tokio::spawn(async move { teams::ensure_running(&c).await }).await
            } {
                return ws;
            }
            portless_ticks = 0;
        }
    }
}

/// One Teams session: attach to every page, keep them bootstrapped and answer the panel's requests
/// until Teams closes (the browser connection ends).
async fn serve(shared: &Arc<Shared>, browser_ws: &str) -> Result<()> {
    let (client, mut events) = Cdp::connect(browser_ws).await?;
    println!("[theames] connected; discovering Teams pages...");

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let pages: Pages = Arc::new(Mutex::new(PageState::default()));

    client
        .send("Target.setDiscoverTargets", json!({ "discover": true }), None)
        .await?;

    tokio::spawn(sweep_pages(client.clone(), sessions.clone(), pages.clone()));
    // Updated theme files: re-apply them on every page of this session.
    let reapplier = tokio::spawn({
        let (c, s, sh) = (client.clone(), sessions.clone(), shared.clone());
        async move {
            loop {
                sh.reapply.notified().await;
                reinject_all(&c, &s).await;
            }
        }
    });
    let _stop_reapplier = AbortOnDrop(reapplier);

    while let Some(event) = events.recv().await {
        if std::env::var_os("THEAMES_DEBUG_TARGETS").is_some() {
            if let Some(m) = event.get("method").and_then(Value::as_str).filter(|m| m.starts_with("Target.")) {
                let i = &event["params"]["targetInfo"];
                println!("[theames] event {m} type={} id={}", i["type"], i["targetId"].as_str().or(event["params"]["targetId"].as_str()).unwrap_or("-"));
            }
        }
        match event.get("method").and_then(Value::as_str) {
            Some("Target.targetCreated") => {
                let info = &event["params"]["targetInfo"];
                if info.get("type").and_then(Value::as_str) == Some("page") {
                    if let Some(id) = info.get("targetId").and_then(Value::as_str) {
                        pages.lock().await.live.insert(id.to_string());
                        tokio::spawn(attach_with_retry(client.clone(), sessions.clone(), pages.clone(), id.to_string()));
                    }
                }
            }
            // Teams creates its real pages as type "other" and only later turns them into
            // "page", so a page can first appear here rather than in targetCreated.
            Some("Target.targetInfoChanged") => {
                let info = &event["params"]["targetInfo"];
                if info["type"].as_str() == Some("page") {
                    if let Some(id) = info["targetId"].as_str() {
                        let new = {
                            let mut st = pages.lock().await;
                            st.live.insert(id.to_string());
                            !st.attaching.contains(id) && !sessions.lock().await.contains_key(id)
                        };
                        if new {
                            tokio::spawn(attach_with_retry(client.clone(), sessions.clone(), pages.clone(), id.to_string()));
                        }
                    }
                }
            }
            Some("Target.targetDestroyed") => {
                if let Some(id) = event["params"]["targetId"].as_str() {
                    sessions.lock().await.remove(id);
                    pages.lock().await.live.remove(id);
                }
            }
            // While Teams starts up it moves pages between processes, which drops our session
            // even though the page lives on. Attach again, or that page never gets Theames.
            Some("Target.detachedFromTarget") => {
                let p = &event["params"];
                if let (Some(sid), Some(tid)) = (p["sessionId"].as_str(), p["targetId"].as_str()) {
                    let ours = {
                        let mut map = sessions.lock().await;
                        let ours = map.get(tid).map(String::as_str) == Some(sid);
                        if ours {
                            map.remove(tid);
                        }
                        ours
                    };
                    if ours && pages.lock().await.live.contains(tid) {
                        println!("[theames] page {tid} detached; re-attaching");
                        tokio::spawn(attach_with_retry(client.clone(), sessions.clone(), pages.clone(), tid.to_string()));
                    }
                }
            }
            Some("Runtime.bindingCalled") => {
                if event["params"]["name"].as_str() == Some(BINDING) {
                    let (c, sh, s) = (client.clone(), shared.clone(), sessions.clone());
                    tokio::spawn(async move {
                        if let Err(e) = on_request(&c, &sh, &s, event).await {
                            eprintln!("[theames] request failed: {e}");
                        }
                    });
                }
            }
            _ => {}
        }
    }

    Ok(())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Safety net for pages the event stream doesn't announce usably: while Teams starts up it
/// replaces some pages (the real UI page among them) with new targets. Every couple of seconds,
/// ask the browser for its pages and attach any we aren't attached to or already attaching.
async fn sweep_pages(client: Arc<Cdp>, sessions: Sessions, pages: Pages) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let Ok(list) = client.send("Target.getTargets", json!({}), None).await else { return };
        let ids: Vec<String> = list["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"].as_str() == Some("page"))
            .filter_map(|t| t["targetId"].as_str().map(str::to_string))
            .collect();
        for id in ids {
            let missing = {
                let mut st = pages.lock().await;
                st.live.insert(id.clone());
                !st.attaching.contains(&id) && !sessions.lock().await.contains_key(&id)
            };
            if missing {
                println!("[theames] sweep found unattached page {id}");
                tokio::spawn(attach_with_retry(client.clone(), sessions.clone(), pages.clone(), id));
            }
        }
    }
}

/// Attach to a page, retrying while it still exists: on a cold launch a page can be swapped to
/// another process mid-attach ("Session with given id not found"). One loop per page at a time.
async fn attach_with_retry(client: Arc<Cdp>, sessions: Sessions, pages: Pages, target_id: String) {
    if !pages.lock().await.attaching.insert(target_id.clone()) {
        return;
    }
    for attempt in 1..=20 {
        if !pages.lock().await.live.contains(&target_id) {
            break;
        }
        match attach_page(&client, &sessions, &target_id).await {
            Ok(()) => break,
            Err(e) => {
                eprintln!("[theames] attach {target_id} failed (attempt {attempt}): {e}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    pages.lock().await.attaching.remove(&target_id);
}

/// Attach to a page, expose the bridge binding, and inject the bootstrap (which
/// pulls and applies the current theme). Runs on this page now and on reload.
async fn attach_page(client: &Arc<Cdp>, sessions: &Sessions, target_id: &str) -> Result<()> {
    let attached = client
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no sessionId"))?
        .to_string();

    sessions
        .lock()
        .await
        .insert(target_id.to_string(), session.clone());

    let s = Some(session.as_str());
    client.send("Runtime.enable", json!({}), s).await.ok();
    client.send("Page.enable", json!({}), s).await.ok();
    client
        .send("Runtime.addBinding", json!({ "name": BINDING }), s)
        .await
        .ok();
    client
        .send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": BOOTSTRAP }),
            s,
        )
        .await
        .ok();
    let first = client
        .send(
            "Runtime.evaluate",
            json!({ "expression": BOOTSTRAP, "awaitPromise": false }),
            s,
        )
        .await?;
    match first.get("exceptionDetails") {
        Some(ex) => eprintln!("[theames] bootstrap threw on page {target_id}: {}", describe(ex)),
        None => println!("[theames] attached + bootstrapped page {target_id}"),
    }

    tokio::spawn(watch_page(client.clone(), sessions.clone(), target_id.to_string(), session));
    Ok(())
}

/// One line describing a CDP `exceptionDetails`.
fn describe(ex: &Value) -> String {
    ex["exception"]["description"]
        .as_str()
        .or_else(|| ex["text"].as_str())
        .unwrap_or("unknown error")
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// What `watch_page` asks each page.
const PROBE: &str = "typeof window.__theamesOpenPanel !== 'function' ? 'missing' : \
    (document.querySelector('[data-tid=\"more-options-header\"]') ? 'ready' : 'ok')";

/// The first time Theames is up and running in Teams, say thanks (once, ever).
async fn maybe_welcome() {
    if !WELCOME_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if let Some(shared) = SHARED.get() {
        let mut cfg = shared.config.lock().await;
        if cfg.welcomed {
            return;
        }
        cfg.welcomed = true;
        cfg.save().ok();
    }
    println!("[theames] up and running in Teams; showing the welcome");
    // Alerts belong on the main thread, which is running the AppKit event loop.
    #[cfg(target_os = "macos")]
    dispatch2::DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            onboarding::welcome(mtm);
        }
    });
    #[cfg(windows)]
    std::thread::spawn(winshell::welcome);
}

/// Teams can finish building a page in a way that slips past the single injection in
/// `attach_page` (most visible on a cold launch, when the launcher starts Teams itself): the
/// page then has no Theames menu and nothing ever retries. So keep checking that the page still
/// has the bootstrap and inject again when it doesn't. Quick at first, relaxed afterwards; ends
/// when the page's target goes away.
async fn watch_page(client: Arc<Cdp>, sessions: Sessions, target_id: String, session: String) {
    let mut ticks = 0u32;
    loop {
        let wait = if ticks < 60 { 1000 } else { 5000 };
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        ticks += 1;
        if sessions.lock().await.get(&target_id) != Some(&session) {
            return;
        }
        // "missing" = no bootstrap; "ok" = bootstrapped; "ready" = bootstrapped and Teams' UI (with
        // the "..." menu Theames adds itself to) is up.
        let probe = client
            .send(
                "Runtime.evaluate",
                json!({ "expression": PROBE, "returnByValue": true }),
                Some(&session),
            )
            .await;
        let Ok(probe) = probe else { return };
        match probe["result"]["value"].as_str() {
            Some("ready") => {
                maybe_welcome().await;
                continue;
            }
            Some("ok") => continue,
            _ => {}
        }
        match client
            .send("Runtime.evaluate", json!({ "expression": BOOTSTRAP }), Some(&session))
            .await
        {
            Ok(r) => match r.get("exceptionDetails") {
                Some(ex) => eprintln!("[theames] re-inject threw on page {target_id}: {}", describe(ex)),
                None => println!("[theames] page {target_id} was missing the bootstrap; re-injected"),
            },
            Err(_) => return,
        }
    }
}

/// Handle a bridge request from a page and reply through window.__theamesOnResponse.
async fn on_request(
    client: &Cdp,
    shared: &Arc<Shared>,
    sessions: &Sessions,
    event: Value,
) -> Result<()> {
    let session = event["sessionId"].as_str().unwrap_or_default().to_string();
    let raw = event["params"]["payload"].as_str().unwrap_or("{}");
    let msg: Value = serde_json::from_str(raw)?;
    let id = msg["id"].clone();
    let action = msg["action"].as_str().unwrap_or_default();
    let payload = msg["payload"].clone();

    let (response, reinject) = match bridge::handle(shared, action, payload).await {
        Ok((data, reinject)) => (json!({ "id": id, "ok": true, "data": data }), reinject),
        Err(e) => (
            json!({ "id": id, "ok": false, "error": e.to_string() }),
            bridge::Reinject::None,
        ),
    };

    // Reply to the requesting page first (before any reload tears it down),
    // passing the response JSON as a single string argument.
    let arg = serde_json::to_string(&serde_json::to_string(&response)?)?;
    client
        .send(
            "Runtime.evaluate",
            json!({ "expression": format!("window.__theamesOnResponse && window.__theamesOnResponse({arg})") }),
            Some(&session),
        )
        .await
        .ok();

    match reinject {
        bridge::Reinject::Apply => reinject_all(client, sessions).await,
        bridge::Reinject::Uninstall => uninstall_now(client, sessions).await,
        bridge::Reinject::None => {}
    }
    Ok(())
}

/// The panel's "Uninstall Theames": take Theames out of every open Teams page (styles, mods,
/// menu), undo its system changes (macOS: icon, login item, session port; settings everywhere),
/// tell the user, and quit.
async fn uninstall_now(client: &Cdp, sessions: &Sessions) {
    println!("[theames] uninstalling (from the panel)");
    let teardown = "(function(){try{window.__theamesCleanup&&window.__theamesCleanup()}catch(e){};\
        ['theames-background','theames-chats','theames-transparency'].forEach(function(id){var s=document.getElementById(id);if(s)s.remove()});\
        ['__theamesOpenPanel','__theamesApplyTheme','__theamesCleanup','__theamesBootstrapped'].forEach(function(k){try{delete window[k]}catch(e){}});})()";
    let targets: Vec<String> = sessions.lock().await.values().cloned().collect();
    for session in targets {
        client
            .send("Runtime.evaluate", json!({ "expression": teardown }), Some(&session))
            .await
            .ok();
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        let _ = tokio::task::spawn_blocking(|| {
            if let Err(e) = uninstall::cleanup() {
                eprintln!("[theames] uninstall: {e}");
            }
            uninstall::notify_done();
            #[cfg(windows)]
            winshell::delete_exe_after_exit();
        })
        .await;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = std::fs::remove_file(config::config_path());
    }
    std::process::exit(0);
}

/// Tell every attached page to re-pull and re-apply the current theme.
async fn reinject_all(client: &Cdp, sessions: &Sessions) {
    let targets: Vec<String> = sessions.lock().await.values().cloned().collect();
    for session in targets {
        client
            .send(
                "Runtime.evaluate",
                json!({ "expression": "window.__theamesApplyTheme && window.__theamesApplyTheme()" }),
                Some(&session),
            )
            .await
            .ok();
    }
}
