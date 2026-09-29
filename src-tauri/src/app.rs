//! Application shell: polling loop, de-duplication, event fan-out.

use crate::agents::{AgentConfigView, AgentSettings};
use crate::collectors::{collect_all, now_millis, Ctx};
use crate::crash;
use crate::model::{Agent, AgentEvent, Severity, Snapshot};
use crate::pet::{self, Dock, PetSize, PetState, Settings};
use crate::screens::EdgePolicy;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Size};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;

/// How far back to look on a cold start.
const DEFAULT_WINDOW_DAYS: i64 = 7;
/// Items inspected per source per poll.
const SCAN_LIMIT: usize = 12;
/// How often the collectors re-read the world.
const POLL_SECS: u64 = 20;

/// How long the window must sit still before a drag counts as finished and we
/// decide whether to dock.
const DRAG_SETTLE_MS: u64 = 320;

/// Sampling period for position-derived work (~30 Hz).  Fast enough that the
/// bell tracks the cursor, slow enough to leave the main thread alone.
const GEOMETRY_TICK_MS: u64 = 33;

/// Minimum logical travel before a window movement counts as a real drag
/// rather than click jitter.
const MIN_DRAG_TRAVEL: f64 = 4.0;

/// Event name the pet listens on.
pub const EVENT_CHANNEL: &str = "suona://events";
/// Event carrying [`PetState`] (size, dock edge, bell angle).
pub const PET_CHANNEL: &str = "suona://pet";
/// Event carrying a [`Notice`] — an app-level message, not an agent event.
pub const NOTICE_CHANNEL: &str = "suona://notice";

/// Something the pet should say that did not come from an agent: a crash, a
/// failure to start, anything about suona itself.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    pub title: String,
    pub detail: String,
    pub severity: Severity,
}

pub struct AppState {
    /// Ids already surfaced, so a completed job is announced exactly once.
    announced: Mutex<HashSet<String>>,
    latest: Mutex<Option<Snapshot>>,
    /// Set on the very first poll: suppress a notification storm for history.
    warmed_up: Mutex<bool>,
    /// True while the full event list is open.
    expanded: AtomicBool,
    /// Size, dock edge, sound preference and current bell angle.
    pet: Mutex<PetState>,
    /// While `now < quiet_until`, window movement is our own doing — expanding
    /// the list, docking, resizing — and must not be read as a user drag.
    quiet_until: Mutex<Instant>,
    /// Whether the window currently holds focus, so a stray blur cannot
    /// dismiss a list that was never focused in the first place.
    focused: AtomicBool,
    /// How the previous run ended, held until the front end asks for it —
    /// emitting during setup would race the webview's own startup.
    startup_notice: Mutex<Option<Notice>>,
    /// Which agents are watched, and where each one lives.
    agents: Mutex<AgentSettings>,
    /// Which screen edges the pet may hide against, and how far to stay off
    /// the top.  Sampled on the main thread and cached, because the placement
    /// threads cannot ask AppKit.
    edge_policy: Mutex<EdgePolicy>,
    /// Whether the gesture in progress has moved the window far enough to be
    /// a drag rather than a click.  Both end in a DOM `click` event, and the
    /// front end has no way to tell them apart on its own.
    dragged: AtomicBool,
}

impl AppState {
    pub fn new(settings: Settings) -> Self {
        Self {
            announced: Mutex::new(HashSet::new()),
            latest: Mutex::new(None),
            warmed_up: Mutex::new(false),
            expanded: AtomicBool::new(false),
            pet: Mutex::new(PetState {
                dock: Dock::None,
                size: settings.size,
                sound: settings.sound,
                angle: 0.0,
                strip: crate::pet::dims(settings.size).strip,
            }),
            quiet_until: Mutex::new(Instant::now()),
            focused: AtomicBool::new(false),
            startup_notice: Mutex::new(None),
            agents: Mutex::new(settings.agents),
            // Optimistic until the first sample lands, a moment later.
            edge_policy: Mutex::new(EdgePolicy::FALLBACK),
            dragged: AtomicBool::new(false),
        }
    }

    fn pet(&self) -> PetState {
        self.pet.lock().unwrap().clone()
    }

    fn dock(&self) -> Dock {
        self.pet.lock().unwrap().dock
    }
}

/// One full sweep of every data source.
pub fn scan(window_days: i64, agents: &AgentSettings) -> Snapshot {
    let since = now_millis() - window_days * 24 * 60 * 60 * 1000;
    let ctx = Ctx::new(since, SCAN_LIMIT, agents);
    let (events, summaries) = collect_all(&ctx);
    Snapshot {
        generated_at: now_millis(),
        summaries,
        events,
    }
}

/// Human-facing notification text for an event.
fn notification_for(ev: &AgentEvent) -> (String, String) {
    let title = format!("{} · {}", ev.agent.label(), ev.title);
    let mut body = String::new();
    if let Some(job) = ev.meta.get("job") {
        body.push_str(job);
    }
    if !ev.detail.is_empty() {
        if !body.is_empty() {
            body.push_str(" — ");
        }
        body.push_str(&ev.detail);
    }
    if body.is_empty() {
        body = ev.kind_label().to_string();
    }
    (title, body)
}

/// Background poller: refresh, push to the UI, raise notifications for the
/// events the user actually needs to hear about.
pub fn spawn_poller(app: AppHandle) {
    std::thread::spawn(move || loop {
        // Read the configuration every sweep so a change in the settings
        // window takes effect on the next tick, with no restart.
        let agents = app.state::<AppState>().agents.lock().unwrap().clone();
        let snapshot = scan(DEFAULT_WINDOW_DAYS, &agents);

        let (new_alerts, warmed): (Vec<AgentEvent>, bool) = {
            let state = app.state::<AppState>();
            let mut announced = state.announced.lock().unwrap();
            let mut warmed = state.warmed_up.lock().unwrap();

            let alerts = snapshot
                .events
                .iter()
                .filter(|e| e.severity.deserves_notification())
                .filter(|e| !announced.contains(&e.id))
                .cloned()
                .collect::<Vec<_>>();

            for e in &snapshot.events {
                announced.insert(e.id.clone());
            }
            // Bound the set so a long-running session does not grow forever.
            if announced.len() > 4000 {
                announced.clear();
            }

            let was_warm = *warmed;
            *warmed = true;
            (if was_warm { alerts } else { Vec::new() }, was_warm)
        };

        if let Some(state) = app.try_state::<AppState>() {
            *state.latest.lock().unwrap() = Some(snapshot.clone());
        }

        let _ = app.emit(EVENT_CHANNEL, &snapshot);

        // On the first pass we swallow the backlog: the user does not want a
        // notification for every historical failure.
        if warmed {
            for ev in new_alerts.iter().take(3) {
                let (title, body) = notification_for(ev);
                let _ = app.notification().builder().title(title).body(body).show();
            }
        }

        std::thread::sleep(Duration::from_secs(POLL_SECS));
    });
}

/// Persist where the user parked the pet, so it comes back to the same spot.
///
/// We poll the window position on a slow timer instead of writing on every
/// `Moved` event: a drag emits hundreds of them, and this converges on the
/// resting position just as reliably.
pub fn spawn_position_saver(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<(i32, i32)> = None;
        loop {
            std::thread::sleep(Duration::from_millis(1000));

            // While the list is open the window origin sits higher up, and
            // while docked it is a thin strip at a screen edge.  Neither is
            // where the pet actually lives, so neither is worth remembering.
            let state = app.state::<AppState>();
            if state.expanded.load(Ordering::Relaxed) || state.dock() != Dock::None {
                continue;
            }

            let Some(win) = app.get_webview_window("pet") else {
                continue;
            };
            let Ok(pos) = win.outer_position() else {
                continue;
            };

            // Self-heal, every second, before anything else.  A one-shot
            // check at startup cannot be trusted on a mixed-DPI desk: the same
            // physical coordinate means different things in the point spaces
            // of two displays, so a set_position aimed at a sane spot can land
            // the window somewhere with no screen at all.  Verifying the
            // *outcome* is the only thing that actually holds.
            let pet_size = app.state::<AppState>().pet().size;
            if let Some((px, py)) = pet_screen_centre(&win, pet_size) {
                let monitors = app.available_monitors().unwrap_or_default();
                if !monitor_holds_pet(&monitors, px, py) {
                    log_event(
                        &app,
                        &format!(
                            "watchdog: pet at ({px:.0},{py:.0}) is on no display — recentring"
                        ),
                    );
                    centre_on_primary(&app, &win, pet_size);
                    last = None;
                    continue;
                }
            }

            let current = (pos.x, pos.y);
            if last == Some(current) {
                continue;
            }
            // Mark as handled even if the write fails, so a read-only config
            // directory does not turn into a write attempt every second.
            last = Some(current);
            save_position(&app, current);
        }
    });
}

fn position_file(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    Some(dir.join("window.json"))
}

pub fn settings_file(app: &AppHandle) -> Option<PathBuf> {
    // Same resolution as the crash log, so the whole profile moves together —
    // and SUONA_CONFIG_DIR works for the headless scan too.
    crash::config_dir()
        .or_else(|| app.path().app_config_dir().ok())
        .map(|d| d.join("settings.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    settings_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default()
}

/// Read `settings.json` without an app handle, for the `--scan` entry point.
pub fn load_settings_from_disk() -> Settings {
    crash::config_dir()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, settings: &Settings) {
    let Some(path) = settings_file(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, text);
    }
}

fn save_position(app: &AppHandle, (x, y): (i32, i32)) {
    let Some(path) = position_file(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{{\"x\":{x},\"y\":{y}}}"));
}

/// The window is created at the config's default size; if the user picked a
/// different level, shrink or grow it before anything is shown.
fn apply_initial_size(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pet") else {
        return;
    };
    hush_moves(app, 900);
    let size = app.state::<AppState>().pet().size;
    let (w, h) = window_size(Dock::None, size, false);
    let _ = win.set_size(Size::Logical(LogicalSize::new(w, h)));
}

fn restore_position(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pet") else {
        return;
    };
    let Some(path) = position_file(app) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(saved) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let (Some(x), Some(y)) = (
        saved.get("x").and_then(|v| v.as_i64()),
        saved.get("y").and_then(|v| v.as_i64()),
    ) else {
        return;
    };

    let (x, y) = (x as i32, y as i32);

    let Ok(scale) = win.scale_factor() else {
        return;
    };
    let pet_size = app.state::<AppState>().pet().size;

    // What the user actually looks at is the *pet*, and the pet hangs at the
    // bottom-centre of a 380x352 window.  Checking the window rectangle is not
    // enough: on a mixed-DPI multi-monitor desk a window can overlap a screen
    // while the pet itself lands in dead space between displays — invisible,
    // and faithfully restored to the same spot on every relaunch.
    let (lx, ly) = pet::pet_centre_in_window(pet_size);
    let pet_x = x as f64 + lx * scale;
    let pet_y = y as f64 + ly * scale;

    let monitors = app.available_monitors().unwrap_or_default();
    if monitor_holds_pet(&monitors, pet_x, pet_y) {
        let _ = win.set_position(PhysicalPosition::new(x, y));
        return;
    }

    log_event(
        app,
        &format!(
            "restore: rejecting ({x},{y}) — pet would land at \
             ({pet_x:.0},{pet_y:.0}), outside every display. Recentring."
        ),
    );
    centre_on_primary(app, &win, pet_size);
}

/// The pet's centre on screen, in physical pixels.
///
/// Uses the window's *current* scale factor, which is the one that applies to
/// a window sitting on a single display — the only situation this is asked
/// about.
fn pet_screen_centre(win: &tauri::WebviewWindow, size: PetSize) -> Option<(f64, f64)> {
    let pos = win.outer_position().ok()?;
    let scale = win.scale_factor().ok()?;
    let (lx, ly) = pet::pet_centre_in_window(size);
    Some((pos.x as f64 + lx * scale, pos.y as f64 + ly * scale))
}

/// True when a point is comfortably inside one of the attached displays.
///
/// The margin keeps a pet parked right against an edge reachable: it only has
/// to be visible enough to grab.
fn monitor_holds_pet(monitors: &[tauri::Monitor], x: f64, y: f64) -> bool {
    let displays: Vec<pet::Display> = monitors
        .iter()
        .map(|m| {
            let origin = m.position();
            let size = m.size();
            pet::Display {
                x: origin.x as f64,
                y: origin.y as f64,
                w: size.width as f64,
                h: size.height as f64,
                margin: 40.0 * m.scale_factor(),
            }
        })
        .collect();
    pet::inside_any(&displays, x, y)
}

/// Park the window in the middle of the primary display — the one place the
/// user is guaranteed to be looking at.
fn centre_on_primary(app: &AppHandle, win: &tauri::WebviewWindow, pet_size: PetSize) {
    let Ok(Some(monitor)) = app.primary_monitor() else {
        return;
    };
    let origin = monitor.position();
    let size = monitor.size();
    let scale = monitor.scale_factor();

    // Sized for the target display, not whichever one it came from.
    let (w, h) = window_size(Dock::None, pet_size, false);
    let w_px = (w * scale).round() as i32;
    let h_px = (h * scale).round() as i32;

    let x = origin.x + ((size.width as i32 - w_px) / 2).max(0);
    let y = origin.y + ((size.height as i32 - h_px) / 2).max(0);
    let _ = win.set_size(Size::Logical(LogicalSize::new(w, h)));
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

#[tauri::command]
pub fn get_snapshot(state: tauri::State<AppState>) -> Option<Snapshot> {
    state.latest.lock().unwrap().clone()
}

/// Hand the front end whatever it should say about suona itself on startup.
///
/// Taken (not peeked) so it is announced exactly once, and fetched rather than
/// pushed so it cannot race the webview's own boot.
#[tauri::command]
pub fn take_startup_notice(state: tauri::State<AppState>) -> Option<Notice> {
    state.startup_notice.lock().unwrap().take()
}

/// Record how the previous run ended, for the front end to announce.
fn stage_startup_notice(app: &AppHandle, abnormal: Option<crash::AbnormalExit>) {
    let Some(exit) = abnormal else {
        return;
    };

    let where_to_look = crash::log_location()
        .map(|p| format!("详情见 {p}"))
        .unwrap_or_default();
    let notice = Notice {
        title: "suona 上次异常退出".to_string(),
        detail: format!("{} · {where_to_look}", exit.reason),
        severity: Severity::Warning,
    };

    // A system notification reaches the user even if they never open the list.
    let _ = app
        .notification()
        .builder()
        .title(&notice.title)
        .body(&notice.detail)
        .show();

    log_event(app, &format!("startup: previous run ended uncleanly: {}", exit.reason));
    if let Some(state) = app.try_state::<AppState>() {
        *state.startup_notice.lock().unwrap() = Some(notice);
    }
}

/// Environment-gated hooks used to verify behaviour that cannot be seen from
/// outside the process.  All are inert unless their variable is set.
///
/// * `SUONA_PANIC_AFTER_MS` — panic on a background thread, to exercise the
///   abnormal-exit reporting end to end.
/// * `SUONA_QUIT_AFTER_MS` — quit normally after a delay, proving a clean
///   shutdown is recorded as clean and never reported as abnormal.
/// * `SUONA_SELFTEST` — open the list unattended and report whether the
///   geometry watcher mistook the resize for a drag.
fn install_debug_hooks(handle: &AppHandle) {
    if let Some(ms) = env_millis("SUONA_PANIC_AFTER_MS") {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            panic!("deliberate background panic (SUONA_PANIC_AFTER_MS)");
        });
    }

    if let Some(ms) = env_millis("SUONA_QUIT_AFTER_MS") {
        let handle = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            handle.exit(0);
        });
    }

    // Debug hook: walk the dock -> open list -> close list cycle and report
    // where the pet ends up.  The regression it guards (the pet never coming
    // back to its edge) is not visible from outside the process.
    if std::env::var("SUONA_DOCKTEST").is_ok() {
        let probe = handle.clone();
        std::thread::spawn(move || {
            let dock_of = |h: &AppHandle| h.state::<AppState>().dock();
            std::thread::sleep(Duration::from_millis(2500));

            // Park it on a side, as dragging to that edge would.
            set_dock(&probe, Dock::Left);
            std::thread::sleep(Duration::from_millis(700));
            eprintln!("SUONA_DOCKTEST: docked to a side -> {:?}", dock_of(&probe));

            // Clicking the bell: the list opens with the pet still docked.
            let _ = set_expanded(probe.clone(), true);
            std::thread::sleep(Duration::from_millis(700));
            eprintln!(
                "SUONA_DOCKTEST: list open   -> {:?} (want Some(Left): stays docked)",
                dock_of(&probe)
            );

            // Closing the list by any route lands here.
            let _ = set_expanded(probe.clone(), false);
            std::thread::sleep(Duration::from_millis(900));
            let after = dock_of(&probe);
            eprintln!(
                "SUONA_DOCKTEST: list closed -> {after:?} (want Some(Left)) {}",
                if after == Dock::Left { "PASS" } else { "FAIL" }
            );

            // Leave the desktop as we found it.
            set_dock(&probe, Dock::None);
            std::thread::sleep(Duration::from_millis(400));
            probe.exit(0);
        });
    }

    // Debug hook: reproduce the exact window rectangle from the log that the
    // dock guard used to refuse, and report what happens now.
    if std::env::var("SUONA_EDGETEST").is_ok() {
        let probe = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            let Some(win) = probe.get_webview_window("pet") else {
                return;
            };
            // `rect=(-426,68,760x1484)` — dragged past the left edge of the
            // screen, which is what a normal drag-to-the-edge looks like.
            let _ = win.set_size(Size::Logical(LogicalSize::new(380.0, 742.0)));
            let _ = win.set_position(PhysicalPosition::new(-426, 68));
            std::thread::sleep(Duration::from_millis(500));
            eprintln!(
                "SUONA_EDGETEST: placed at (-426,68), at_position={:?}",
                win.outer_position()
            );

            evaluate_dock(&probe);
            std::thread::sleep(Duration::from_millis(600));
            let dock = probe.state::<AppState>().dock();
            eprintln!(
                "SUONA_EDGETEST: -> {dock:?} (want Some(Left)) {}",
                if dock == Dock::Left { "PASS" } else { "FAIL" }
            );

            set_dock(&probe, Dock::None);
            std::thread::sleep(Duration::from_millis(300));
            probe.exit(0);
        });
    }

    // Debug hook: check that a click and a drag are told apart.
    if std::env::var("SUONA_CLICKTEST").is_ok() {
        let probe = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));

            // Nothing has moved: a tap.
            let quiet = take_was_drag(probe.state::<AppState>());
            eprintln!("SUONA_CLICKTEST: still window  -> was_drag={quiet} (want false)");

            // Now shove the window, as dragging it would.
            let Some(win) = probe.get_webview_window("pet") else {
                return;
            };
            let Ok(start) = win.outer_position() else {
                return;
            };
            let _ = win.set_position(PhysicalPosition::new(start.x + 240, start.y + 120));
            // Read before the settle clears it — the DOM's click arrives at
            // mouseup, which is likewise before the settle.
            std::thread::sleep(Duration::from_millis(150));
            let moved = take_was_drag(probe.state::<AppState>());
            let again = take_was_drag(probe.state::<AppState>());
            eprintln!("SUONA_CLICKTEST: window moved  -> was_drag={moved} (want true)");
            eprintln!(
                "SUONA_CLICKTEST: second ask   -> was_drag={again} (want false, consuming) {}",
                if !quiet && moved && !again { "PASS" } else { "FAIL" }
            );

            probe.exit(0);
        });
    }

    if std::env::var("SUONA_SELFTEST").is_ok() {
        let probe = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            eprintln!("SUONA_SELFTEST: opening the list");
            let _ = set_expanded(probe.clone(), true);

            std::thread::sleep(Duration::from_millis(3000));
            let state = probe.state::<AppState>();
            let expanded = state.expanded.load(Ordering::Relaxed);
            let dock = state.dock();
            drop(state);
            eprintln!(
                "SUONA_SELFTEST: 3s later expanded={expanded} dock={dock:?} \
                 (want expanded=true dock=None)"
            );
        });
    }
}

fn env_millis(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse::<u64>().ok()
}

/// Watch for panics on threads that outlive them.
///
/// A panic on the main thread takes the process with it, but a background
/// thread only dies quietly — the poller or the geometry watcher could be gone
/// while the pet still looks perfectly healthy.  Nothing else would ever
/// mention it, so this surfaces it while the app is still running.
pub fn spawn_crash_watchdog(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));

        let Some(reason) = crash::take_panic() else {
            continue;
        };

        log_event(&app, &format!("panic: {reason}"));

        let notice = Notice {
            title: "suona 内部出错了".to_string(),
            detail: match crash::log_location() {
                Some(p) => format!("{reason} · 详情见 {p}"),
                None => reason,
            },
            severity: Severity::Error,
        };
        let _ = app
            .notification()
            .builder()
            .title(&notice.title)
            .body(&notice.detail)
            .show();
        let _ = app.emit(NOTICE_CHANNEL, &notice);
    });
}

// ── pet geometry, docking and aiming ───────────────────────────────────────

/// The monitor the pet currently lives on, in physical pixels.
#[derive(Clone, Copy)]
struct Screen {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
}

fn screen_of(win: &tauri::WebviewWindow) -> Option<Screen> {
    let monitor = monitor_for(win)?;
    let pos = monitor.position();
    let size = monitor.size();
    Some(Screen {
        x: pos.x as f64,
        y: pos.y as f64,
        w: size.width as f64,
        h: size.height as f64,
        scale: monitor.scale_factor(),
    })
}

/// The display a window should be measured against.
///
/// Deliberately keyed on the window's **top-left**, not its centre: every
/// position we compute is anchored there, so the origin's display is the one
/// whose scale factor is meaningful.  Using the centre picks the wrong display
/// whenever a window straddles two, and on a mixed-DPI desk that silently
/// corrupts every logical/pixel conversion that follows.
fn monitor_for(win: &tauri::WebviewWindow) -> Option<tauri::Monitor> {
    let pos = win.outer_position().ok()?;

    let monitors = win.available_monitors().ok()?;
    let containing = monitors.into_iter().find(|m| {
        let origin = m.position();
        let size = m.size();
        pos.x >= origin.x
            && pos.y >= origin.y
            && pos.x < origin.x + size.width as i32
            && pos.y < origin.y + size.height as i32
    });

    // Fall back to the platform's own answer if the origin sits in a gap.
    containing.or_else(|| win.current_monitor().ok().flatten())
}

/// Ask macOS which sides are clear of the menu bar and the Dock.
///
/// Must run on the main thread.  Off it, the cached answer is left alone
/// rather than replaced with a guess.
pub fn sample_usable_sides(app: &AppHandle) {
    let Some(insets) = crate::screens::insets() else {
        return;
    };
    let policy = insets.policy();
    log_event(
        app,
        &format!(
            "screens: insets top={:.0} bottom={:.0} left={:.0} right={:.0} \
             -> dockable top={} bottom={} left={} right={} (top inset {:.0})",
            insets.top,
            insets.bottom,
            insets.left,
            insets.right,
            policy.top,
            policy.bottom,
            policy.left,
            policy.right,
            policy.top_inset,
        ),
    );
    if let Some(state) = app.try_state::<AppState>() {
        *state.edge_policy.lock().unwrap() = policy;
    }
}

/// Every attached display, as geometry the placement code can reason about.
fn displays_of(win: &tauri::WebviewWindow) -> Vec<pet::Display> {
    win.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let origin = m.position();
            let size = m.size();
            pet::Display {
                x: origin.x as f64,
                y: origin.y as f64,
                w: size.width as f64,
                h: size.height as f64,
                margin: 0.0,
            }
        })
        .collect()
}

/// Whether the window lies across more than one display.
///
/// Only *that* makes docking unsafe, because the placement would use one
/// display's scale factor while the window partly lives on another.  A window
/// hanging off the outer edge of a single display is fine — that is simply what
/// pushing the pet against the edge of the screen looks like, and treating it
/// as a straddle made the left and bottom edges impossible to dock to.
fn spans_displays(win: &tauri::WebviewWindow, pos: (i32, i32), size: (u32, u32)) -> bool {
    let left = pos.0 as f64;
    let top = pos.1 as f64;
    pet::spans_displays(
        (left, top, left + size.0 as f64, top + size.1 as f64),
        &displays_of(win),
    )
}

/// Resolve the monitor without enumerating screens on every tick.
///
/// `current_monitor` walks the display list, which is far too expensive to do
/// on every drag sample.  The cached answer stays valid while the window is
/// comfortably inside it, so we only pay for a real lookup when the pet is
/// dragged toward — or across — a screen boundary.
fn screen_for(win: &tauri::WebviewWindow, cache: &mut Option<Screen>) -> Option<Screen> {
    if let Some(cached) = cache {
        if let Ok(pos) = win.outer_position() {
            if let Ok(size) = win.outer_size() {
                let (x, y) = (pos.x as f64, pos.y as f64);
                let (right, bottom) = (x + size.width as f64, y + size.height as f64);
                let inside =
                    x >= cached.x && y >= cached.y && right <= cached.x + cached.w && bottom <= cached.y + cached.h;
                if inside {
                    return Some(*cached);
                }
            }
        }
    }
    let fresh = screen_of(win);
    *cache = fresh;
    fresh
}

/// Logical window size for a given state.
fn window_size(dock: Dock, size: PetSize, expanded: bool) -> (f64, f64) {
    let d = pet::dims(size);
    match dock {
        Dock::None => (d.w, if expanded { d.expanded_h } else { d.h }),
        Dock::Left | Dock::Right => (d.strip, d.side),
        Dock::Top | Dock::Bottom => (d.side, d.strip),
    }
}

/// Push the current pet state to the front end.
pub fn emit_pet(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut pet = state.pet();
    let d = pet::dims(pet.size);
    // The front end positions the pet from this, so it has to reflect whether
    // the list is open — the same number the window was sized around.
    pet.strip = if state.expanded.load(Ordering::Relaxed) && pet.dock != Dock::None {
        d.peek
    } else {
        d.strip
    };
    drop(state);
    let _ = app.emit(PET_CHANNEL, &pet);
}

/// Declare that the next stretch of window movement is ours, not the user's.
///
/// Without this, resizing the window for the expanded list looks exactly like
/// a drag: the origin jumps 390px upward, which lands the top edge inside the
/// snap threshold and docks the pet to the top of the screen — collapsing the
/// list the user just opened.
fn hush_moves(app: &AppHandle, millis: u64) {
    if let Some(state) = app.try_state::<AppState>() {
        *state.quiet_until.lock().unwrap() = Instant::now() + Duration::from_millis(millis);
    }
}

/// Append one line to the diagnostics log.
///
/// The dock/expand state machine reacts to window geometry, which is invisible
/// from the outside; this is the only practical way to see why it decided what
/// it decided.
fn log_event(app: &AppHandle, message: &str) {
    // Same resolution as the crash log, so both honour SUONA_CONFIG_DIR and the
    // two trails always sit side by side.
    let Some(dir) = crash::config_dir().or_else(|| app.path().app_config_dir().ok()) else {
        return;
    };
    let path = dir.join("events.log");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "{} {message}",
            chrono::Local::now().format("%H:%M:%S%.3f")
        );
    }
}

/// Recompute the free-floating bell angle so it faces the middle of the screen.
fn refresh_angle(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pet") else {
        return;
    };
    let mut cache = None;
    let Some(screen) = screen_for(&win, &mut cache) else {
        return;
    };
    update_aim(app, &win, &screen);
}

/// Aim the bell at the screen centre, given an already-resolved monitor.
///
/// Returns whether the angle actually moved, so callers can avoid pointless
/// IPC.  Only called while free-floating: docked poses are edge-aligned.
fn update_aim(app: &AppHandle, win: &tauri::WebviewWindow, screen: &Screen) -> bool {
    let state = app.state::<AppState>();
    let size = state.pet().size;
    if state.dock() != Dock::None {
        return false;
    }
    drop(state);

    let Ok(pos) = win.outer_position() else {
        return false;
    };

    let (lx, ly) = pet::pet_centre_in_window(size);
    let pet_cx = pos.x as f64 + lx * screen.scale;
    let pet_cy = pos.y as f64 + ly * screen.scale;

    let angle = pet::aim_at(
        screen.x + screen.w / 2.0 - pet_cx,
        screen.y + screen.h / 2.0 - pet_cy,
    );

    let state = app.state::<AppState>();
    let mut pet = state.pet.lock().unwrap();
    // A degree is below what the eye resolves here, and skipping sub-degree
    // changes keeps a drag from flooding the webview with updates.
    if (pet.angle - angle).abs() < 1.0 {
        return false;
    }
    pet.angle = angle;
    drop(pet);
    emit_pet(app);
    true
}

/// Where the pet's visible centre currently sits on screen.
///
/// The two layouts park the pet in different places inside the window: free,
/// it hangs near the bottom below the bubble; docked, the strip is symmetric
/// about it.  Using the wrong one makes the pet leap when the window changes
/// shape.
fn current_pet_centre(
    win: &tauri::WebviewWindow,
    screen: &Screen,
    layout: Dock,
    size: PetSize,
) -> Option<(f64, f64)> {
    let pos = win.outer_position().ok()?;
    let cur = win.outer_size().ok()?;
    Some(match layout {
        Dock::None => {
            let (lx, ly) = pet::pet_centre_in_window(size);
            (
                pos.x as f64 + lx * screen.scale,
                pos.y as f64 + ly * screen.scale,
            )
        }
        _ => (
            pos.x as f64 + cur.width as f64 / 2.0,
            pos.y as f64 + cur.height as f64 / 2.0,
        ),
    })
}

/// Move and resize the window for a dock edge (or back to free floating).
///
/// `from` describes how the window is laid out *right now*, which is what the
/// anchor has to be computed against; `to` is where it is going.
fn apply_dock_geometry(app: &AppHandle, from: Dock, to: Dock) -> Result<(), String> {
    // Resizing and repositioning here is our doing; the watcher must not read
    // the jump as the user dragging the pet to a new edge.
    hush_moves(app, 700);

    let win = app
        .get_webview_window("pet")
        .ok_or_else(|| "pet window missing".to_string())?;
    let screen = screen_of(&win).ok_or_else(|| "no monitor".to_string())?;
    let size = app.state::<AppState>().pet().size;

    let (pet_cx, pet_cy) = current_pet_centre(&win, &screen, from, size)
        .ok_or_else(|| "pet window unreadable".to_string())?;

    // Only guard the *dock* direction.  Snapping a straddling window to an
    // edge would compute its new rectangle from one display's scale factor
    // while it partly lives on another — the path that parked the pet in dead
    // space between screens.  Undocking is safe: a docked strip is a thin
    // slice of a single display.
    if to != Dock::None {
        let (Ok(cur_pos), Ok(cur_size)) = (win.outer_position(), win.outer_size()) else {
            return Err("pet window unreadable".to_string());
        };
        if spans_displays(&win, (cur_pos.x, cur_pos.y), (cur_size.width, cur_size.height)) {
            return Err("window spans two displays".to_string());
        }
    }

    let policy = *app.state::<AppState>().edge_policy.lock().unwrap();

    let (w, h) = window_size(
        to,
        size,
        // Keep the list open across a size change; a docked strip has no room
        // for it either way.
        to == Dock::None && app.state::<AppState>().expanded.load(Ordering::Relaxed),
    );
    let w_px = (w * screen.scale).round() as i32;
    let h_px = (h * screen.scale).round() as i32;

    let max_x = (screen.x + screen.w - w_px as f64).max(screen.x);
    let max_y = (screen.y + screen.h - h_px as f64).max(screen.y);

    // Popping out of an edge has to clear the snap threshold, otherwise the
    // dock watcher would immediately tuck it back in.
    let inset = pet::UNDOCK_INSET * screen.scale;

    let x = match to {
        // Docked edges are flush; everything else keeps the pet's column.
        Dock::Left => screen.x,
        Dock::Right => max_x,
        _ => (pet_cx - w_px as f64 / 2.0).clamp(screen.x + inset, max_x),
    };
    let y = match to {
        // Clear of the measured menu bar.  A hardcoded guess is what put the
        // pet half-behind it before: the real height was larger.
        Dock::Top => screen.y + policy.top_inset * screen.scale,
        Dock::Bottom => max_y,
        // Left/right strips keep the pet's row.
        _ => (pet_cy - h_px as f64 / 2.0).clamp(screen.y + inset, max_y),
    };

    // When the strip is horizontal it is the vertical gap that matters, and
    // vice versa; nudge the cross axis clear of the edge it came from too.
    let (x, y) = match to {
        Dock::Left | Dock::Right => (x, y.max(screen.y + inset)),
        Dock::Top | Dock::Bottom => (x.max(screen.x + inset), y),
        Dock::None => (x, y),
    };

    win.set_size(Size::Logical(LogicalSize::new(w, h)))
        .map_err(|e| e.to_string())?;
    win.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
        .map_err(|e| e.to_string())?;

    // Let the layout settle before the angle is recomputed.
    if to == Dock::None {
        refresh_angle(app);
    }
    Ok(())
}

/// Switch dock state, resizing the window and telling the front end.
pub fn set_dock(app: &AppHandle, dock: Dock) {
    let state = app.state::<AppState>();
    let previous = state.dock();
    if previous == dock {
        return;
    }

    // Move the window *first*.  Committing a dock the window never reached
    // leaves the pet drawn with an edge transform while sitting in the middle
    // of the screen — which reads as it having jumped somewhere for no reason.
    // If the move is refused, the state must stay as it was.
    if let Err(reason) = apply_dock_geometry(app, previous, dock) {
        log_event(
            app,
            &format!("dock: {previous:?} -> {dock:?} refused ({reason})"),
        );
        return;
    }

    {
        let mut pet = state.pet.lock().unwrap();
        pet.dock = dock;
        pet.angle = pet::dock_angle(dock);
    }
    // The event list cannot fit in a docked strip.
    let docking = dock != Dock::None;
    if docking {
        state.expanded.store(false, Ordering::Relaxed);
    }
    drop(state);

    log_event(app, &format!("dock: {previous:?} -> {dock:?}"));
    // Only when *entering* a dock.  Emitting this on the way out raced the
    // "undock then open the list" flow and snapped the panel shut again the
    // instant it appeared.
    if docking {
        let _ = app.emit("suona://collapse", ());
    }
    emit_pet(app);
}

/// Decide whether a finished drag should dock the pet to a screen edge.
fn evaluate_dock(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pet") else {
        return;
    };
    let state = app.state::<AppState>();
    let current = state.dock();
    drop(state);

    let (Some(screen), Ok(pos), Ok(cur)) =
        (screen_of(&win), win.outer_position(), win.outer_size())
    else {
        return;
    };

    // Docking while the window straddles two displays would compute its new
    // geometry from one display's scale factor while it partly lives on
    // another — which is how the pet ended up parked in dead space between
    // screens.  Skip the snap rather than guess.
    // Only a window lying across two displays is unsafe to place.  One hanging
    // off the outer edge is the normal look of pushing the pet to the edge of
    // the screen, and refusing that made left and bottom undockable.
    if spans_displays(&win, (pos.x, pos.y), (cur.width, cur.height)) {
        log_event(
            app,
            &format!(
                "evaluate: rect=({},{},{}x{}) spans two displays — not docking",
                pos.x, pos.y, cur.width, cur.height
            ),
        );
        return;
    }

    let policy = *app.state::<AppState>().edge_policy.lock().unwrap();
    let dock = pet::edge_for(
        pos.x as f64,
        pos.y as f64,
        cur.width as f64,
        cur.height as f64,
        screen.x,
        screen.y,
        screen.w,
        screen.h,
        screen.scale,
        policy,
    );

    log_event(
        app,
        &format!(
            "evaluate: rect=({},{},{}x{}) screen=({:.0},{:.0},{}x{})@{}x current={current:?} -> {dock:?}",
            pos.x, pos.y, cur.width, cur.height,
            screen.x, screen.y, screen.w as i32, screen.h as i32, screen.scale,
        ),
    );
    let _ = policy;

    if dock != current {
        set_dock(app, dock);
    }
}

/// One background thread owns everything derived from the window position.
///
/// This used to run inside the window's `Moved` event, which fires hundreds of
/// times per drag.  Each call enumerated the displays and pushed an IPC message
/// to the webview, all on the main thread — the window could barely follow the
/// cursor.  Sampling from here instead leaves the main thread free to drag,
/// caps the update rate at [`GEOMETRY_TICK_MS`], and still catches the resting
/// position so the snap decision is made on the real final coordinates.
pub fn spawn_geometry_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_pos: Option<(i32, i32)> = None;
        let mut drag_origin: Option<(i32, i32)> = None;
        let mut travelled = 0i32;
        let mut last_change = Instant::now();
        let mut moved_since_eval = false;
        let mut screen_cache: Option<Screen> = None;

        loop {
            std::thread::sleep(Duration::from_millis(GEOMETRY_TICK_MS));

            let Some(win) = app.get_webview_window("pet") else {
                continue;
            };
            let Ok(pos) = win.outer_position() else {
                continue;
            };
            let current = (pos.x, pos.y);

            // Our own repositioning is not a drag.  Track the new position so
            // no stale delta survives, but do not start a gesture from it.
            let ours = Instant::now()
                < *app.state::<AppState>().quiet_until.lock().unwrap();
            if ours {
                last_pos = Some(current);
                drag_origin = None;
                travelled = 0;
                moved_since_eval = false;
                continue;
            }

            if last_pos != Some(current) {
                if let Some(previous) = last_pos {
                    // How far the window has actually been dragged, measured
                    // from where the gesture started.
                    let origin = *drag_origin.get_or_insert(previous);
                    let distance = (current.0 - origin.0).abs() + (current.1 - origin.1).abs();
                    travelled = travelled.max(distance);
                }
                last_pos = Some(current);
                last_change = Instant::now();
                moved_since_eval = true;

                if let Some(screen) = screen_for(&win, &mut screen_cache) {
                    update_aim(&app, &win, &screen);
                }

                // Publish the verdict while the gesture is still in flight:
                // the DOM's click arrives on mouseup, before the settle below.
                // The threshold is decided here so the scale factor never has
                // to leak into the front end.
                let scale = screen_cache.map(|s| s.scale).unwrap_or(1.0);
                let threshold = (MIN_DRAG_TRAVEL * scale).round();
                app.state::<AppState>()
                    .dragged
                    .store(travelled as f64 >= threshold, Ordering::Relaxed);
                continue;
            }

            // Position has settled: this is where a drag actually ended, so it
            // is the moment to decide whether the pet should tuck into an edge.
            if moved_since_eval
                && last_change.elapsed().as_millis() as u64 >= DRAG_SETTLE_MS
            {
                moved_since_eval = false;
                let scale = screen_cache.map(|s| s.scale).unwrap_or(1.0);
                let threshold = (MIN_DRAG_TRAVEL * scale).round() as i32;
                // The pet is also the window's drag handle, so a plain click
                // can jitter it a pixel or two.  Treating that as a drag would
                // re-evaluate docking and tuck the pet away the instant the
                // list opened.
                if travelled >= threshold {
                    evaluate_dock(&app);
                }
                drag_origin = None;
                travelled = 0;
                app.state::<AppState>().dragged.store(false, Ordering::Relaxed);
            }
        }
    });
}

#[tauri::command]
pub fn refresh(state: tauri::State<AppState>) -> Snapshot {
    let agents = state.agents.lock().unwrap().clone();
    scan(DEFAULT_WINDOW_DAYS, &agents)
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Identifiers for the context menu items.
const MENU_CONFIG: &str = "config";
const MENU_QUIT: &str = "quit";

/// Show the pet's right-click menu at the cursor.
///
/// A native menu rather than an HTML one: it can extend past the window, so it
/// is never clipped by the 380px strip the pet lives in, and it looks like
/// every other macOS menu.
#[tauri::command]
pub fn show_pet_menu(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("pet")
        .ok_or_else(|| "pet window missing".to_string())?;

    let config = MenuItem::with_id(&app, MENU_CONFIG, "修改配置", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let quit = MenuItem::with_id(&app, MENU_QUIT, "退出程序", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let gap = PredefinedMenuItem::separator(&app).map_err(|e| e.to_string())?;

    let menu = Menu::with_items(&app, &[&config, &gap, &quit]).map_err(|e| e.to_string())?;
    window.popup_menu(&menu).map_err(|e| e.to_string())
}

// ── agent configuration ────────────────────────────────────────────────────

/// Everything the settings screen needs to render.
#[derive(Serialize, Clone)]
pub struct ConfigSnapshot {
    pub agents: Vec<AgentConfigView>,
}

fn config_snapshot(state: &AppState) -> ConfigSnapshot {
    ConfigSnapshot {
        agents: state.agents.lock().unwrap().views(),
    }
}

/// Whether the gesture that just ended was a drag rather than a click.
///
/// A drag is delivered to the page as a `click` too — and pointer coordinates
/// *relative to the window* barely change while dragging, because the window
/// follows the cursor.  So the page cannot tell them apart; the window's own
/// travel can.
///
/// Consuming: each click asks once, so a stale verdict cannot swallow the next
/// genuine click.
#[tauri::command]
pub fn take_was_drag(state: tauri::State<AppState>) -> bool {
    state.dragged.swap(false, Ordering::Relaxed)
}

#[tauri::command]
pub fn get_agent_config(state: tauri::State<AppState>) -> ConfigSnapshot {
    config_snapshot(&state)
}

/// Switch an agent on or off.
///
/// This is what "removing" an agent means in suona: nothing on disk is touched,
/// it simply stops being read and reported, and can be switched back at will.
#[tauri::command]
pub fn set_agent_enabled(
    app: AppHandle,
    state: tauri::State<AppState>,
    key: String,
    enabled: bool,
) -> Result<ConfigSnapshot, String> {
    let agent = Agent::from_key(&key).ok_or_else(|| format!("未知的 agent：{key}"))?;
    state.agents.lock().unwrap().set_enabled(agent, enabled);
    persist_settings(&app);
    log_event(&app, &format!("config: {key} enabled={enabled}"));
    Ok(config_snapshot(&state))
}

/// Point an agent at a different directory.  An empty path restores the default.
#[tauri::command]
pub fn set_agent_path(
    app: AppHandle,
    state: tauri::State<AppState>,
    key: String,
    path: String,
) -> Result<ConfigSnapshot, String> {
    let agent = Agent::from_key(&key).ok_or_else(|| format!("未知的 agent：{key}"))?;

    // Reject what cannot be a directory rather than silently accepting a typo
    // and reporting "not found" forever.
    let trimmed = path.trim();
    if !trimmed.is_empty() && !trimmed.starts_with('/') && !trimmed.starts_with('~') {
        return Err("请填绝对路径（以 / 或 ~ 开头）".to_string());
    }

    state.agents.lock().unwrap().set_path(agent, trimmed);
    persist_settings(&app);
    log_event(&app, &format!("config: {key} path={trimmed}"));
    Ok(config_snapshot(&state))
}

/// Write the current pet + agent configuration to `settings.json`.
fn persist_settings(app: &AppHandle) {
    let pet = app.state::<AppState>().pet();
    let agents = app.state::<AppState>().agents.lock().unwrap().clone();
    save_settings(
        app,
        &Settings {
            size: pet.size,
            sound: pet.sound,
            agents,
        },
    );
}

/// Resize the window around a docked pet so the list can sit beside it.
///
/// The docked edge stays put; only the inward extent changes.
fn set_docked_panel(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
    dock: Dock,
    expanded: bool,
) -> Result<(), String> {
    let screen = screen_of(win).ok_or_else(|| "no monitor".to_string())?;
    let state = app.state::<AppState>();
    let size = state.pet().size;
    let policy = *state.edge_policy.lock().unwrap();
    drop(state);

    let (w, h) = if expanded {
        pet::docked_panel_size(dock, size)
    } else {
        window_size(dock, size, false)
    };
    let w_px = (w * screen.scale).round() as i32;
    let h_px = (h * screen.scale).round() as i32;

    let pos = win.outer_position().map_err(|e| e.to_string())?;
    let max_x = (screen.x + screen.w - w_px as f64).max(screen.x);
    let max_y = (screen.y + screen.h - h_px as f64).max(screen.y);

    // Anchored on the docked edge; the cross axis holds its place so the pet
    // does not slide along the edge when the list opens.
    let x = match dock {
        Dock::Left => screen.x,
        Dock::Right => max_x,
        _ => (pos.x as f64).clamp(screen.x, max_x),
    };
    let y = match dock {
        Dock::Top => screen.y + policy.top_inset * screen.scale,
        Dock::Bottom => max_y,
        _ => (pos.y as f64).clamp(screen.y, max_y),
    };

    win.set_size(Size::Logical(LogicalSize::new(w, h)))
        .map_err(|e| e.to_string())?;
    win.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
        .map_err(|e| e.to_string())?;

    app.state::<AppState>()
        .expanded
        .store(expanded, Ordering::Relaxed);
    log_event(
        app,
        &format!("panel while docked: {dock:?} expanded={expanded} ({}x{})", w as i32, h as i32),
    );
    emit_pet(app);
    Ok(())
}

/// Open or close the full event list.
///
/// The window grows upward and its origin moves up by the same amount, so the
/// pet stays pinned to the same spot on screen.  The origin is clamped to the
/// monitor's usable top so the list never slides under the menu bar.
#[tauri::command]
pub fn set_expanded(app: AppHandle, expanded: bool) -> Result<(), String> {
    // Growing the window is our doing.  Without this the watcher reads the
    // origin jump as a drag and re-evaluates docking.
    hush_moves(&app, 700);

    let win = app
        .get_webview_window("pet")
        .ok_or_else(|| "pet window missing".to_string())?;

    // A docked pet keeps its edge: the window grows *inward* and the list
    // appears beside the pet, which merely pokes out a little further.  It
    // used to be pulled out to the middle of the screen and parked under the
    // panel, which is a lot of movement for something that never asked to go
    // anywhere.
    let dock = app.state::<AppState>().dock();
    if dock != Dock::None {
        return set_docked_panel(&app, &win, dock, expanded);
    }
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    let pos = win.outer_position().map_err(|e| e.to_string())?;
    let size = win.outer_size().map_err(|e| e.to_string())?;

    let width = size.width as f64 / scale;
    let bottom = pos.y + size.height as i32;

    let (min_y, max_height) = match win.current_monitor().ok().flatten() {
        Some(monitor) => {
            let origin = monitor.position();
            let screen = monitor.size();
            (
                // Leave room for the menu bar.
                origin.y
                    + (app.state::<AppState>().edge_policy.lock().unwrap().top_inset * scale)
                        as i32,
                screen.height as f64 / scale - 48.0,
            )
        }
        None => (0, 1e9),
    };

    let size = app.state::<AppState>().pet().size;
    let (_, collapsed_h) = window_size(Dock::None, size, false);
    let (_, expanded_h) = window_size(Dock::None, size, true);

    let height = if expanded { expanded_h } else { collapsed_h }.min(max_height);
    let target_y = (bottom - (height * scale).round() as i32).max(min_y);

    win.set_size(Size::Logical(LogicalSize::new(width, height)))
        .map_err(|e| e.to_string())?;
    win.set_position(PhysicalPosition::new(pos.x, target_y))
        .map_err(|e| e.to_string())?;

    // Clicking the pet cannot be relied on to focus the window: Tauri's drag
    // region calls preventDefault() on mousedown.  Without a genuine focus
    // there is no blur later, and "click elsewhere to dismiss" would never
    // fire.  Only taken when the user deliberately opens the list.
    if expanded {
        let _ = win.set_focus();
    }

    app.state::<AppState>()
        .expanded
        .store(expanded, Ordering::Relaxed);
    log_event(
        &app,
        &format!(
            "expand: {expanded} (window {:.0}x{:.0} at y={target_y})",
            width, height
        ),
    );

    Ok(())
}

#[tauri::command]
pub fn get_pet_state(state: tauri::State<AppState>) -> PetState {
    state.pet()
}

/// Switch between the three size levels, resizing the window around the pet.
#[tauri::command]
pub fn set_pet_size(app: AppHandle, size: PetSize) -> Result<PetState, String> {
    let state = app.state::<AppState>();
    {
        let mut pet = state.pet.lock().unwrap();
        if pet.size == size {
            return Ok(pet.clone());
        }
        pet.size = size;
    }
    let dock = state.dock();
    drop(state);

    // Re-apply geometry for the new size.  The layout shape is unchanged, so
    // the anchor is computed against the same dock state we are going to.
    if let Err(e) = apply_dock_geometry(&app, dock, dock) {
        return Err(e);
    }
    emit_pet(&app);

    persist_settings(&app);
    Ok(app.state::<AppState>().pet())
}

/// Sound effects for reports.  Off by default; persisted once toggled.
#[tauri::command]
pub fn set_sound(app: AppHandle, enabled: bool) -> PetState {
    {
        let state = app.state::<AppState>();
        state.pet.lock().unwrap().sound = enabled;
    }
    let current = app.state::<AppState>().pet();
    persist_settings(&app);
    emit_pet(&app);
    current
}


#[tauri::command]
pub fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Register or unregister suona as a login item.  Returns the state actually
/// in effect afterwards, so the UI never shows an optimistic toggle.
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    let outcome = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    outcome.map_err(|e| e.to_string())?;
    Ok(manager.is_enabled().unwrap_or(enabled))
}

/// Launch the desktop pet.
pub fn run() {
    // Before anything else: from here on, a panic anywhere leaves a trail.
    crash::install_panic_hook();
    // How did the last run end?  Read before this run claims the session.
    let abnormal = crash::begin_session();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .setup(move |app| {
            let handle = app.handle().clone();
            let settings = load_settings(&handle);
            app.manage(AppState::new(settings));

            restore_position(&handle);
            apply_initial_size(&handle);
            // The Dock may sit on either side, so ask before deciding where
            // the pet is allowed to hide.  Main thread, hence here.
            sample_usable_sides(&handle);
            spawn_poller(handle.clone());
            spawn_position_saver(handle.clone());
            spawn_geometry_watcher(handle.clone());
            spawn_crash_watchdog(handle.clone());
            stage_startup_notice(&handle, abnormal);

            // Tell the front end the starting size / angle once it is up.
            let emit_handle = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                refresh_angle(&emit_handle);
                emit_pet(&emit_handle);
            });

            install_debug_hooks(&handle);
            Ok(())
        })
        .on_menu_event(|app, event| {
            let id: &str = event.id().as_ref();
            match id {
                MENU_QUIT => {
                    log_event(app, "quit requested from the context menu");
                    app.exit(0);
                }
                MENU_CONFIG => {
                    log_event(app, "configuration requested from the context menu");
                    let _ = app.emit("suona://config", ());
                }
                _ => {}
            }
        })
        .on_window_event(|window, event| {
            let app = window.app_handle().clone();
            match event {
                tauri::WindowEvent::Focused(true) => {
                    app.state::<AppState>().focused.store(true, Ordering::Relaxed);
                    // Cheap, and a natural moment to notice the Dock having
                    // been moved to a different side.
                    sample_usable_sides(&app);
                }
                // Clicking anywhere outside the pet takes focus away, which is
                // the cue to dismiss the list — our window only ever sees its
                // own bounds, so there is no "clicked elsewhere" event.
                tauri::WindowEvent::Focused(false) => {
                    let was_focused =
                        app.state::<AppState>().focused.swap(false, Ordering::Relaxed);
                    // Only a real focus loss counts.  A blur arriving without a
                    // preceding focus (which a never-activated pet window does
                    // emit) must not close a list the user just opened.
                    if was_focused && app.state::<AppState>().expanded.swap(false, Ordering::Relaxed)
                    {
                        log_event(&app, "collapse: window lost focus");
                        // Shrink the window back too, or it would keep
                        // swallowing clicks over an area that is now invisible.
                        let _ = set_expanded(app.clone(), false);
                        let _ = app.emit("suona://collapse", ());
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            take_startup_notice,
            refresh,
            quit_app,
            show_pet_menu,
            take_was_drag,
            get_agent_config,
            set_agent_enabled,
            set_agent_path,
            set_expanded,
            get_autostart,
            set_autostart,
            get_pet_state,
            set_pet_size,
            set_sound,
        ])
        .build(tauri::generate_context!())
        .expect("suona failed to start")
        // `run` rather than `Builder::run` so there is somewhere to mark a
        // clean shutdown.  Whatever skips this — a panic, a kill, power loss —
        // leaves the session flag set, and the next launch reports it.
        .run(|_handle, event| {
            if let tauri::RunEvent::Exit = event {
                crash::end_session();
            }
        });
}

impl AgentEvent {
    /// Fallback phrasing when an event carries no detail.
    pub fn kind_label(&self) -> &'static str {
        match self.severity {
            Severity::Error => "出现问题",
            Severity::Warning => "需要注意",
            Severity::Success => "已完成",
            Severity::Info => "有更新",
        }
    }
}
