//! Single-instance guard: at most one launcher runs at a time, newest wins.
//!
//! The launcher holds an OS file lock (`theames.lock`) for its whole life. The OS
//! drops it however the process ends (crash, `kill -9`), so it can never go stale.
//! A launcher that finds the lock held asks the holder to exit, using the PID the
//! holder published in `theames.pid`, then takes the lock over. The PID lives in a
//! separate file because Windows blocks other handles from reading a locked file.

use anyhow::{anyhow, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config;

/// How long to let the old launcher exit before forcing it.
const GRACE: Duration = Duration::from_secs(3);
/// Give up taking over after this long.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Windows: spawn helper commands without flashing a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keep alive for the life of the process; dropping it releases the lock.
pub struct InstanceLock {
    _file: File,
}

pub fn acquire() -> Result<InstanceLock> {
    let dir = config::base_dir();
    std::fs::create_dir_all(&dir)?;
    let pid_path = dir.join("theames.pid");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("theames.lock"))?;

    let me = std::process::id();
    let start = Instant::now();
    let mut asked: Option<(u32, Instant)> = None;
    let mut forced: Option<u32> = None;

    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if start.elapsed() > TIMEOUT {
            return Err(anyhow!("another Theames launcher holds the lock and would not exit"));
        }
        // Only ever signal a process that is really a launcher: the pid file can briefly
        // name a dead launcher whose pid the OS has since reused.
        let holder = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&p| p != me && is_launcher(p));
        if let Some(pid) = holder {
            match asked {
                Some((p, _)) if p == pid => {}
                _ => {
                    println!("[theames] replacing running launcher (pid {pid})");
                    terminate(pid, false);
                    asked = Some((pid, Instant::now()));
                }
            }
            if let Some((p, at)) = asked {
                if p == pid && forced != Some(pid) && at.elapsed() > GRACE {
                    terminate(pid, true);
                    forced = Some(pid);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    std::fs::write(&pid_path, me.to_string())?;
    Ok(InstanceLock { _file: file })
}

#[cfg(unix)]
fn is_launcher(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("theames"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_launcher(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("theames.exe"))
        .unwrap_or(false)
}

#[cfg(unix)]
fn terminate(pid: u32, force: bool) {
    let sig = if force { "-KILL" } else { "-TERM" };
    let _ = Command::new("kill").args([sig, &pid.to_string()]).status();
}

// The launcher keeps no state worth a graceful shutdown (config is saved per request
// and the page cleans up mods on the next bootstrap), so Windows just ends it.
#[cfg(windows)]
fn terminate(pid: u32, _force: bool) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}
