//! Abnormal-exit detection and reporting.
//!
//! Three failure modes matter here, and all of them used to be silent:
//!
//! * a panic on the main thread kills the process — launched from Finder,
//!   stderr goes nowhere, so the user just sees the pet vanish;
//! * a panic on a *background* thread kills only that thread, leaving the app
//!   running with a feature quietly dead (the worst case, because nothing
//!   looks wrong);
//! * being killed without a chance to clean up.
//!
//! So we keep two records.  `crash.log` is the forensic trail, appended from
//! the panic hook.  `session.json` is a one-bit "did the last run shut down
//! properly" flag, flipped at startup and cleared on a clean exit; a flag
//! still set at the next launch is what proves the previous run died.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Set by the panic hook, drained by the watchdog in `app`.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Crash logs are rotated rather than truncated so a panic loop cannot fill
/// the disk.
const MAX_CRASH_LOG: u64 = 256 * 1024;

/// Config directory, derived without Tauri so the panic hook can be installed
/// before the app (and any of its state) exists.
///
/// Matches `app_config_dir()` for the `dev.suona.pet` identifier.  suona is
/// macOS-only — it already needs `macos-private-api` for window transparency.
pub fn config_dir() -> Option<PathBuf> {
    // An override keeps tests — and unusual installs — off the real profile.
    if let Ok(dir) = std::env::var("SUONA_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join("Library/Application Support/dev.suona.pet"))
}

fn crash_log_path() -> Option<PathBuf> {
    Some(config_dir()?.join("crash.log"))
}

fn session_path() -> Option<PathBuf> {
    Some(config_dir()?.join("session.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Session {
    /// False while a run is in progress, true once it shut down cleanly.
    clean: bool,
    /// Why the previous run is considered unclean.
    #[serde(default)]
    reason: String,
    #[serde(default)]
    pid: u32,
}

/// A previous run that did not shut down cleanly.
#[derive(Debug, Clone)]
pub struct AbnormalExit {
    pub reason: String,
}

fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn append_crash(text: &str) {
    let Some(path) = crash_log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_CRASH_LOG {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{text}\n");
    }
}

fn write_session(clean: bool, reason: &str) {
    let Some(path) = session_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let session = Session {
        clean,
        reason: reason.to_string(),
        pid: std::process::id(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&session) {
        let _ = std::fs::write(path, text);
    }
}

/// Catch panics from every thread, leave a trail, and flag the session.
///
/// Installed first thing in `run()`, before any thread can be spawned.
pub fn install_panic_hook() {
    // Keep the default behaviour (printing to stderr) as well.
    let default_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());

        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        let summary = format!("线程 {name} 在 {location} panic：{payload}");

        append_crash(&format!(
            "=== {} panic\nthread:  {name}\nwhere:   {location}\nmessage: {payload}\n\n{}",
            now_stamp(),
            std::backtrace::Backtrace::force_capture()
        ));
        write_session(false, &summary);

        // try_lock, never lock: a panic can happen while this mutex is held,
        // and a hook that blocks would deadlock the process on its way out.
        if let Ok(mut slot) = LAST_PANIC.try_lock() {
            *slot = Some(summary);
        }

        default_hook(info);
    }));
}

/// Claim a panic recorded since the last check, for live reporting.
///
/// A panic on a background thread does not stop the app, so the user has to be
/// told while it is still running.
pub fn take_panic() -> Option<String> {
    LAST_PANIC.lock().ok()?.take()
}

/// Mark a run as started, and report how the previous one ended.
pub fn begin_session() -> Option<AbnormalExit> {
    let previous = session_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Session>(&t).ok());

    let previous_pid = previous.as_ref().map(|p| p.pid).unwrap_or(0);

    let abnormal = match previous {
        Some(ref s) if !s.clean => {
            // An empty reason means nothing was recorded — no panic hook fired,
            // so the process was killed or died hard.  Reporting the literal
            // placeholder ("running") would tell the user nothing.
            let reason = if s.reason.trim().is_empty() {
                "上次运行没有正常退出（被强制结束或意外终止）".to_string()
            } else {
                s.reason.clone()
            };
            Some(AbnormalExit { reason })
        }
        _ => None,
    };

    if let Some(a) = &abnormal {
        append_crash(&format!(
            "=== {}\nprevious run (pid {previous_pid}) ended uncleanly: {}",
            now_stamp(),
            a.reason
        ));
    }

    // Claim the session.  The reason stays empty unless a panic fills it in.
    write_session(false, "");
    abnormal
}

/// Mark the run as having shut down properly.
pub fn end_session() {
    write_session(true, "clean exit");
}

/// Where the forensic trail lives, for the UI to mention.
pub fn log_location() -> Option<String> {
    Some(crash_log_path()?.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `SUONA_CONFIG_DIR` is process-global, so the tests that set it must not
    /// overlap.  Recovered from poisoning because the panic test deliberately
    /// poisons nothing else, but a failure anywhere must not cascade.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("suona-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("SUONA_CONFIG_DIR", &dir);
        dir
    }

    #[test]
    fn crash_reporting_lifecycle() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("crash-test");

        // First ever run: nothing to report.
        assert!(begin_session().is_none(), "a fresh start must not report");

        // Begin again without a clean shutdown — this is what a kill or a
        // power loss looks like from the next launch's point of view.
        let abnormal = begin_session().expect("an unclean end must be reported");
        assert!(
            !abnormal.reason.contains("running"),
            "the placeholder must never reach the user, got: {}",
            abnormal.reason
        );

        // A clean shutdown clears the flag.
        end_session();
        assert!(
            begin_session().is_none(),
            "a clean exit must not be reported as abnormal"
        );

        end_session();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_hook_is_reported_for_live_surfacing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("panic-test");

        install_panic_hook();
        // A background thread panicking is the dangerous case: the process
        // survives, so only this record can tell the user.
        let _ = std::panic::catch_unwind(|| panic!("boom for the test"));

        let caught = take_panic().expect("the hook must record the panic");
        assert!(caught.contains("boom for the test"), "got: {caught}");
        // Draining is one-shot, so the watchdog cannot report it twice.
        assert!(take_panic().is_none());

        let log = std::fs::read_to_string(dir.join("crash.log")).unwrap();
        assert!(log.contains("boom for the test"), "crash.log missing entry");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
