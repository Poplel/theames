//! Updates: Theames itself and the installed themes, from the places the website already publishes.
//!
//! - Theames: `https://theames.poplel.xyz/downloads/SHA256SUMS.txt` lists the release files
//!   (`Theames-<version>-macOS.dmg`, `Theames-<version>-Windows-x64.zip`) with their SHA-256. A newer
//!   version is downloaded, checked against that hash (and on macOS, against the running app's code
//!   signing team), put in place of the running copy, and started; the new copy takes over from
//!   this one (see `instance`) and re-attaches to the running Teams.
//! - Themes: the marketplace folder lists the `.theame` files; each one's manifest has its version.
//!   An installed theme with a newer copy there is replaced and re-applied live, no restart. A
//!   marketplace theme whose manifest `replaces` an installed one (e.g. Games replacing Battleship)
//!   is installed in its place, and the user's choices move across.
//!
//! Runs a little after start and then every few hours. Theames updates itself while `auto_update`
//! is on (the panel's Settings toggle); each theme has its own toggle (off = listed in
//! `theme_updates_off`), and a theme with it off is never replaced or touched. The panel's
//! "Check now" also updates Theames itself, toggle or not. `THEAMES_UPDATE_URL` and
//! `THEAMES_MARKET_URL` point it somewhere else for testing.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bridge::Shared;
use crate::config::{self, Config};
use crate::theame::{self, Theame};

const SITE: &str = "https://theames.poplel.xyz/downloads/";
const MARKET: &str = "https://media.pophosting.xyz/assets/Theames/Themes/";
const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(6 * 3600);
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;
/// Passed to the copy Theames starts after updating itself.
pub const RESTARTED_FLAG: &str = "--updated";

static RESTARTED: AtomicBool = AtomicBool::new(false);
/// Did this process start because the previous Theames just updated itself?
#[cfg_attr(not(windows), allow(dead_code))]
pub fn restarted() -> bool {
    RESTARTED.load(Ordering::Relaxed)
}

/// What the panel shows.
#[derive(Default, Clone)]
pub struct Status {
    pub checking: bool,
    /// Unix seconds of the last finished check.
    pub last_check: Option<u64>,
    pub message: String,
    /// Set when the last check failed.
    pub error: bool,
}

fn site() -> String {
    std::env::var("THEAMES_UPDATE_URL").unwrap_or_else(|_| SITE.to_string())
}
fn market() -> String {
    std::env::var("THEAMES_MARKET_URL").unwrap_or_else(|_| MARKET.to_string())
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Called once at start, before anything else: note a post-update restart, tidy up after one.
pub fn startup() {
    if std::env::args().any(|a| a == RESTARTED_FLAG) {
        RESTARTED.store(true, Ordering::Relaxed);
        println!("[theames] started by an update");
    }
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        // the copy that was running when the update replaced it
        let _ = std::fs::remove_file(old_exe(&exe));
    }
    let _ = std::fs::remove_dir_all(config::base_dir().join("updates"));
}

/// The background schedule. Never returns.
pub async fn run(shared: Arc<Shared>) {
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        check(shared.clone(), false).await;
        tokio::time::sleep(EVERY).await;
    }
}

/// Status for the panel.
pub async fn status_json(shared: &Arc<Shared>) -> Value {
    let st = shared.update.lock().await.clone();
    json!({
        "auto": shared.config.lock().await.auto_update,
        "version": env!("CARGO_PKG_VERSION"),
        "checking": st.checking,
        "lastCheck": st.last_check,
        "message": st.message,
        "error": st.error,
    })
}

/// Check for (and install) updates now: themes whose toggle is on, and Theames itself when its
/// toggle is on or `manual` (the panel's "Check now"). Only one check runs at a time.
pub async fn check(shared: Arc<Shared>, manual: bool) {
    let app_too = manual || shared.config.lock().await.auto_update;
    {
        let mut st = shared.update.lock().await;
        if st.checking {
            return;
        }
        st.checking = true;
        st.message = "Checking for updates\u{2026}".into();
        st.error = false;
    }
    let mut notes: Vec<String> = Vec::new();
    let mut failed = false;

    // Themes first: an app update restarts Theames.
    match update_themes(&shared).await {
        Ok(changed) => {
            if !changed.is_empty() {
                shared.reapply.notify_waiters();
                notes.push(format!("Updated {}", changed.join(", ")));
            }
        }
        Err(e) => {
            eprintln!("[theames] theme update check failed: {e:#}");
            notes.push("Couldn't check the themes".into());
            failed = true;
        }
    }
    let app = if app_too { tokio::task::spawn_blocking(update_app).await.unwrap_or_else(|e| Err(anyhow!("{e}"))) } else { Ok(None) };
    match app {
        Ok(Some(v)) => {
            println!("[theames] installed Theames {v}; restarting");
            set(&shared, format!("Installed Theames {v}. Restarting\u{2026}"), false).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            match restart() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("[theames] restart after update failed: {e:#}");
                    notes.push(format!("Theames {v} is installed and starts next time"));
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("[theames] app update failed: {e:#}");
            notes.push(format!("Couldn't update Theames: {e}"));
            failed = true;
        }
    }
    let msg = if notes.is_empty() { "Everything is up to date".to_string() } else { notes.join(". ") };
    println!("[theames] update check: {msg}");
    let mut st = shared.update.lock().await;
    st.checking = false;
    st.last_check = Some(now());
    st.message = msg;
    st.error = failed;
}

async fn set(shared: &Arc<Shared>, message: String, error: bool) {
    let mut st = shared.update.lock().await;
    st.message = message;
    st.error = error;
}

// ---- versions + downloads --------------------------------------------------------------------

/// "1.2.10" > "1.2.9". Anything that isn't a number counts as 0.
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.trim().trim_start_matches('v').split('.').map(|x| x.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (mut x, mut y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x > y
}

fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let mut resp = ureq::get(url).header("User-Agent", concat!("Theames/", env!("CARGO_PKG_VERSION"))).call()
        .with_context(|| format!("GET {url}"))?;
    let mut buf = Vec::new();
    resp.body_mut().as_reader().take(MAX_DOWNLOAD).read_to_end(&mut buf)?;
    Ok(buf)
}
fn get_text(url: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&get_bytes(url)?).into_owned())
}
fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}
/// Resolve `href` from a listing at `base`.
fn join_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(rest) = href.strip_prefix('/') {
        let origin = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        return format!("{origin}/{rest}");
    }
    format!("{}/{}", base.trim_end_matches('/'), href.trim_start_matches("./"))
}
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
/// The `.theame` links in a folder listing: (file name, absolute URL).
fn listed_theames(html: &str, base: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(k) = lower[from..].find("href=") {
        let start = from + k + 5;
        from = start;
        let Some(q) = html[start..].chars().next().filter(|c| *c == '"' || *c == '\'') else { continue };
        let body = &html[start + 1..];
        let Some(end) = body.find(q) else { break };
        let href = body[..end].replace("&amp;", "&");
        if !href.to_ascii_lowercase().ends_with(".theame") {
            continue;
        }
        let url = join_url(base, &href);
        let name = percent_decode(url.rsplit('/').next().unwrap_or(""));
        if !name.is_empty() && !name.contains(['/', '\\']) && !out.iter().any(|(n, _)| *n == name) {
            out.push((name, url));
        }
    }
    out
}

// ---- themes --------------------------------------------------------------------------------

/// Bring installed themes up to date. Returns the names of the themes that changed.
async fn update_themes(shared: &Arc<Shared>) -> Result<Vec<String>> {
    let dir = shared.themes_dir.clone();
    let base = market();
    let off = shared.config.lock().await.theme_updates_off.clone();
    // the network part runs off the async threads
    let plan = tokio::task::spawn_blocking(move || plan_themes(&dir, &base, &off)).await??;
    if plan.is_empty() {
        return Ok(Vec::new());
    }
    let mut cfg = shared.config.lock().await;
    let mut changed = Vec::new();
    for step in plan {
        let path = shared.themes_dir.join(&step.file);
        let tmp = shared.themes_dir.join(format!(".{}.download", step.file));
        std::fs::write(&tmp, &step.data)?;
        std::fs::rename(&tmp, &path)?;
        for old in &step.replaces {
            migrate_choices(&mut cfg, old, &step.theame);
            let _ = std::fs::remove_file(shared.themes_dir.join(old));
        }
        let what = if step.replaces.is_empty() {
            format!("{} to {}", step.theame.manifest.name, step.theame.manifest.version)
        } else {
            format!("{} (replaces {})", step.theame.manifest.name, step.replaces.iter().map(|f| f.trim_end_matches(".theame")).collect::<Vec<_>>().join(", "))
        };
        println!("[theames] theme update: {what}");
        changed.push(what);
    }
    cfg.save()?;
    Ok(changed)
}

struct ThemeStep {
    file: String,
    data: Vec<u8>,
    theame: Theame,
    /// installed files this one takes over from
    replaces: Vec<String>,
}

fn plan_themes(dir: &Path, base: &str, off: &[String]) -> Result<Vec<ThemeStep>> {
    let installed: Vec<(String, Theame)> = theame::list(dir)
        .into_iter()
        .filter(|f| !off.iter().any(|o| o.eq_ignore_ascii_case(f)))      // toggled off: leave alone
        .filter_map(|f| Theame::open(&dir.join(&f)).ok().map(|t| (f, t)))
        .collect();
    if installed.is_empty() {
        return Ok(Vec::new());
    }
    let listing = listed_theames(&get_text(base)?, base);
    let mut steps = Vec::new();
    for (file, url) in listing {
        let data = match get_bytes(&url) {
            Ok(d) => d,
            Err(e) => { eprintln!("[theames] {file}: {e:#}"); continue; }
        };
        let remote = match Theame::from_bytes(file.clone(), data.clone()) {
            Ok(t) => t,
            Err(e) => { eprintln!("[theames] {file}: {e:#}"); continue; }
        };
        let mine = installed.iter().find(|(f, _)| f.eq_ignore_ascii_case(&file));
        let replaces: Vec<String> = installed
            .iter()
            .filter(|(f, t)| !f.eq_ignore_ascii_case(&file) && remote.manifest.replaces.iter().any(|r| {
                r.eq_ignore_ascii_case(&t.manifest.name) || r.eq_ignore_ascii_case(f) || format!("{r}.theame").eq_ignore_ascii_case(f)
            }))
            .map(|(f, _)| f.clone())
            .collect();
        let update = match mine {
            Some((_, t)) => newer(&remote.manifest.version, &t.manifest.version) || !replaces.is_empty(),
            None => !replaces.is_empty(),
        };
        if update {
            steps.push(ThemeStep { file: mine.map(|(f, _)| f.clone()).unwrap_or(file), data, theame: remote, replaces });
        }
    }
    Ok(steps)
}

/// Move the user's choices from theme file `old` to `new`: same component name if `new` has it,
/// else its only component of that kind. Saved variable values move too.
fn migrate_choices(cfg: &mut Config, old: &str, new: &Theame) {
    let m = &new.manifest;
    let map = |r: &str, names: Vec<&String>| -> Option<String> {
        let comp = r.strip_prefix(old)?.strip_prefix("::")?;
        let pick = names.iter().find(|n| n.as_str() == comp).or(if names.len() == 1 { names.first() } else { None })?;
        Some(format!("{}::{}", new.file, pick))
    };
    let mods: Vec<&String> = m.mods.iter().map(|x| &x.name).collect();
    let mut out = Vec::new();
    for r in std::mem::take(&mut cfg.selection.mods) {
        let r2 = if r.starts_with(&format!("{old}::")) { map(&r, mods.clone()) } else { Some(r) };
        if let Some(r2) = r2 {
            if !out.contains(&r2) {
                out.push(r2);
            }
        }
    }
    cfg.selection.mods = out;
    let bgs: Vec<&String> = m.backgrounds.iter().map(|x| &x.name).collect();
    if let Some(b) = cfg.selection.background.clone().filter(|b| b.starts_with(&format!("{old}::"))) {
        cfg.selection.background = map(&b, bgs);
    }
    let chats: Vec<&String> = m.chats.iter().map(|x| &x.name).collect();
    if let Some(c) = cfg.selection.chats.clone().filter(|c| c.starts_with(&format!("{old}::"))) {
        cfg.selection.chats = map(&c, chats);
    }
    if cfg.selection.icon.as_deref() == Some(old) {
        cfg.selection.icon = m.icon.as_ref().map(|_| new.file.clone());
    }
    if let Some(vars) = cfg.variables.remove(old) {
        cfg.variables.entry(new.file.clone()).or_default().extend(vars);
    }
}

// ---- Theames itself ------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const ASSET: (&str, &str) = ("Theames-", "-macOS.dmg");
#[cfg(windows)]
const ASSET: (&str, &str) = ("Theames-", "-Windows-x64.zip");

/// The newest release for this platform in SHA256SUMS.txt: (version, file, sha256).
#[cfg(any(target_os = "macos", windows))]
fn latest_release(sums: &str) -> Option<(String, String, String)> {
    let mut best: Option<(String, String, String)> = None;
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(file)) = (parts.next(), parts.next()) else { continue };
        let file = file.trim_start_matches('*');
        let Some(ver) = file.strip_prefix(ASSET.0).and_then(|r| r.strip_suffix(ASSET.1)) else { continue };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if best.as_ref().is_none_or(|b| newer(ver, &b.0)) {
            best = Some((ver.to_string(), file.to_string(), hash.to_ascii_lowercase()));
        }
    }
    best
}

/// Install a newer Theames if there is one. Some(version) when it did (restart to run it).
#[cfg(any(target_os = "macos", windows))]
fn update_app() -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    if !crate::dock::in_bundle() {
        return Ok(None); // a development build (cargo run): nothing to replace
    }
    let base = site();
    let sums = get_text(&join_url(&base, "SHA256SUMS.txt"))?;
    let Some((ver, file, hash)) = latest_release(&sums) else { return Ok(None) };
    if !newer(&ver, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    println!("[theames] Theames {ver} is available; downloading {file}");
    let data = get_bytes(&join_url(&base, &file))?;
    if sha256_hex(&data) != hash {
        bail!("the download of {file} doesn't match its published checksum");
    }
    let work = config::base_dir().join("updates");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    install(&work, &file, &data)?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(Some(ver))
}
#[cfg(not(any(target_os = "macos", windows)))]
fn update_app() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn team_id(app: &Path) -> Option<String> {
    let out = std::process::Command::new("codesign").args(["-dv", "--verbose=2"]).arg(app).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).map(str::to_string).filter(|t| t != "not set")
}

/// macOS: mount the DMG, check the new Theames.app is signed by the same team as this one, and
/// swap it in for the running bundle (the running binary stays valid until we exit).
#[cfg(target_os = "macos")]
fn install(work: &Path, file: &str, data: &[u8]) -> Result<()> {
    let bundle = PathBuf::from(crate::dock::bundle_path().ok_or_else(|| anyhow!("can't find Theames.app"))?);
    install_into(work, file, data, &bundle)
}
#[cfg(target_os = "macos")]
fn install_into(work: &Path, file: &str, data: &[u8], bundle: &Path) -> Result<()> {
    use std::process::Command;
    let bundle = bundle.to_path_buf();
    let parent = bundle.parent().ok_or_else(|| anyhow!("Theames.app has no folder"))?.to_path_buf();
    let dmg = work.join(file);
    std::fs::write(&dmg, data)?;
    let mnt = work.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    let ok = Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"]).arg(&mnt).arg(&dmg).status()?;
    if !ok.success() {
        bail!("couldn't open the downloaded disk image");
    }
    let result = (|| -> Result<()> {
        let src = mnt.join("Theames.app");
        if !src.is_dir() {
            bail!("the disk image has no Theames.app");
        }
        let verified = Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&src).status()?.success();
        let (theirs, ours) = (team_id(&src), team_id(&bundle));
        if !verified || theirs.is_none() || theirs != ours {
            bail!("the new Theames isn't signed by the same developer ({theirs:?} vs {ours:?})");
        }
        let staged = parent.join(".Theames-update.app");
        let old = parent.join(".Theames-old.app");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        let copied = Command::new("ditto").arg(&src).arg(&staged).status()?.success();
        if !copied {
            let _ = std::fs::remove_dir_all(&staged);
            bail!("couldn't copy the new version next to {} (no permission?)", bundle.display());
        }
        std::fs::rename(&bundle, &old).with_context(|| format!("moving {} aside", bundle.display()))?;
        if let Err(e) = std::fs::rename(&staged, &bundle) {
            let _ = std::fs::rename(&old, &bundle);
            return Err(e).context("putting the new version in place");
        }
        let _ = std::fs::remove_dir_all(&old);
        let _ = Command::new("touch").arg(&bundle).status();
        Ok(())
    })();
    let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mnt).status();
    result
}

#[cfg(windows)]
fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Windows: take Theames.exe out of the zip and swap it in for the running one (Windows lets a
/// running .exe be renamed, not overwritten).
#[cfg(windows)]
fn install(work: &Path, _file: &str, data: &[u8]) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))?;
    let mut entry = zip.by_name("Theames.exe").map_err(|_| anyhow!("the zip has no Theames.exe"))?;
    let mut exe_bytes = Vec::new();
    entry.read_to_end(&mut exe_bytes)?;
    let fresh = work.join("Theames.exe");
    std::fs::write(&fresh, &exe_bytes)?;
    let exe = std::env::current_exe()?;
    let old = old_exe(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("moving the running Theames.exe aside")?;
    if let Err(e) = std::fs::copy(&fresh, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e).context("writing the new Theames.exe");
    }
    Ok(())
}

/// Start the freshly installed copy; it takes over from this process (see `instance`).
fn restart() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let bundle = crate::dock::bundle_path().ok_or_else(|| anyhow!("no bundle"))?;
        std::process::Command::new("open").args(["-n", &bundle, "--args", RESTARTED_FLAG]).status()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        std::process::Command::new(std::env::current_exe()?).arg(RESTARTED_FLAG).spawn()?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        bail!("not supported here")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(newer("1.0.1", "1.0.0"));
        assert!(newer("1.10.0", "1.9.9"));
        assert!(newer("2", "1.9"));
        assert!(!newer("1.0.0", "1.0.0"));
        assert!(!newer("1.0", "1.0.0"));
        assert!(!newer("0.9.9", "1.0.0"));
    }
    #[test]
    fn listing() {
        let html = r#"<a href="Aurora.theame">x</a><a href='/assets/Theames/Themes/Games.theame'>g</a>
            <a href="https://h.example/T/Split%20View.theame">s</a><a href="readme.txt">r</a><a href="Aurora.theame">dup</a>"#;
        let l = listed_theames(html, "https://media.example/assets/Theames/Themes/");
        assert_eq!(l.len(), 3);
        assert_eq!(l[0], ("Aurora.theame".into(), "https://media.example/assets/Theames/Themes/Aurora.theame".into()));
        assert_eq!(l[1].1, "https://media.example/assets/Theames/Themes/Games.theame");
        assert_eq!(l[2].0, "Split View.theame");
    }
    /// Needs a folder listing at THEAMES_TEST_MARKET (e.g. `python3 -m http.server` in a folder of
    /// .theame files) and THEAMES_TEST_THEMES = a themes folder to update. Run with --ignored.
    #[test]
    #[ignore]
    fn themes_from_a_market() {
        let market = std::env::var("THEAMES_TEST_MARKET").unwrap();
        let dir = PathBuf::from(std::env::var("THEAMES_TEST_THEMES").unwrap());
        let plan = plan_themes(&dir, &market, &["Terminal.theame".to_string()]).unwrap();
        let mut cfg = Config::default();
        cfg.selection.mods = vec!["Battleship.theame::Battleship".into(), "Aurora.theame::Aurora Glow (animated)".into()];
        cfg.variables.entry("Battleship.theame".into()).or_default().insert("x".into(), "1".into());
        for step in &plan {
            println!("step {} {} replaces {:?}", step.file, step.theame.manifest.version, step.replaces);
            std::fs::write(dir.join(&step.file), &step.data).unwrap();
            for old in &step.replaces {
                migrate_choices(&mut cfg, old, &step.theame);
                std::fs::remove_file(dir.join(old)).unwrap();
            }
        }
        println!("mods now {:?} vars {:?}", cfg.selection.mods, cfg.variables.keys().collect::<Vec<_>>());
        let files: Vec<String> = plan.iter().map(|s| s.file.clone()).collect();
        assert!(files.contains(&"Games.theame".to_string()), "Games installed in place of Battleship");
        assert!(files.contains(&"Aurora.theame".to_string()), "newer Aurora picked up");
        assert!(!files.contains(&"Terminal.theame".to_string()), "same version left alone");
        assert_eq!(cfg.selection.mods, vec!["Games.theame::Games".to_string(), "Aurora.theame::Aurora Glow (animated)".to_string()]);
        assert!(cfg.variables.contains_key("Games.theame") && !cfg.variables.contains_key("Battleship.theame"));
        assert!(!dir.join("Battleship.theame").exists() && dir.join("Games.theame").exists());
    }

    /// THEAMES_TEST_DMG = a release DMG, THEAMES_TEST_BUNDLE = a signed Theames.app to replace.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn mac_install() {
        let dmg = PathBuf::from(std::env::var("THEAMES_TEST_DMG").unwrap());
        let bundle = PathBuf::from(std::env::var("THEAMES_TEST_BUNDLE").unwrap());
        let work = std::env::temp_dir().join("theames-install-test");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let data = std::fs::read(&dmg).unwrap();
        install_into(&work, dmg.file_name().unwrap().to_str().unwrap(), &data, &bundle).unwrap();
        let ok = std::process::Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&bundle).status().unwrap();
        assert!(ok.success());
        assert!(!bundle.parent().unwrap().join(".Theames-old.app").exists());
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn sums() {
        let s = format!("{0}  Theames-1.0.0-macOS.dmg\n{0}  Theames-1.0.0-Windows-x64.zip\n{1}  Theames-1.2.0-macOS.dmg\n{1}  Theames-1.2.0-Windows-x64.zip\nbad line\n",
            "a".repeat(64), "b".repeat(64));
        let (v, f, h) = latest_release(&s).unwrap();
        assert_eq!(v, "1.2.0");
        assert!(f.starts_with("Theames-1.2.0-"));
        assert_eq!(h, "b".repeat(64));
    }
}
