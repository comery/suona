//! Hermes cron collector.
//!
//! Two files describe the same thing from different angles, so we join them:
//!
//! * `~/.hermes/cron/jobs.json` — job identity, schedule, enabled/paused state.
//! * `~/.hermes/cron/executions.db` — one row per firing, with the status
//!   transition `claimed → running → completed|failed`.
//!
//! The database is the source of truth for "did my scheduled work actually
//! finish"; jobs.json supplies the human-readable name and cadence.

use super::{
    mtime_millis, now_millis, parse_millis, severity_for_status, summarize, Ctx,
    disabled,};
use crate::model::{Agent, AgentEvent, AgentSummary, EventKind, Severity};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Identity and cadence of one scheduled job.
#[derive(Debug, Default, Clone)]
struct JobMeta {
    name: String,
    schedule: String,
    enabled: bool,
    /// Hermes' own verdict on the last firing — authoritative, and the only
    /// signal available for runs older than the execution window.
    last_status: Option<String>,
    failure_streak: i64,
    workdir: Option<String>,
}

/// One firing from `executions.db`.
#[derive(Debug, Clone)]
struct Execution {
    job_id: String,
    status: String,
    at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    error: Option<String>,
    delivery_outcome: Option<String>,
}

pub fn collect(ctx: &Ctx, out: &mut Vec<AgentEvent>) -> AgentSummary {
    // Absent from the context means the user switched it off; nothing is read.
    let Some(base) = ctx.dir(Agent::Hermes) else {
        return disabled(Agent::Hermes);
    };
    let cron_dir = base.join("cron");
    let jobs_file = cron_dir.join("jobs.json");
    let db_file = cron_dir.join("executions.db");

    let detected = jobs_file.exists() || db_file.exists();
    if !detected {
        return summarize(Agent::Hermes, false, "个任务", 0, 0, 0, None);
    }

    let jobs = load_jobs(&jobs_file);
    let executions = load_executions(&db_file, ctx);
    let incidents = load_incidents(&db_file, ctx);

    let mut failing = 0usize;
    let mut healthy = 0usize;
    let mut last_activity: Option<i64> = None;
    let mut seen_runs: HashMap<String, i64> = HashMap::new();

    // --- per-run events, newest first, capped ---------------------------------
    let mut runs: Vec<&Execution> = executions.iter().filter(|e| e.at >= ctx.since).collect();
    runs.sort_by(|a, b| b.at.cmp(&a.at));

    for exec in runs.into_iter().take(ctx.scan_limit) {
        let meta = jobs.get(&exec.job_id);
        let name = meta
            .map(|m| m.name.clone())
            .unwrap_or_else(|| exec.job_id.clone());

        let (kind, severity, headline) = classify(exec);
        last_activity = Some(last_activity.map_or(exec.at, |p| p.max(exec.at)));

        if matches!(severity, Severity::Error | Severity::Warning) {
            failing += 1;
        } else {
            healthy += 1;
        }
        seen_runs.insert(exec.job_id.clone(), exec.at);

        let mut detail = String::new();
        if let (Some(s), Some(f)) = (exec.started_at.or(Some(exec.at)), exec.finished_at) {
            detail = humanize_duration(f - s);
        }
        if let Some(err) = exec.error.as_deref().filter(|e| !e.is_empty()) {
            detail = first_line(err, 160);
        } else if exec.delivery_outcome.as_deref() == Some("failed") {
            detail = "结果投递失败".to_string();
        }

        let mut ev = AgentEvent::new(
            format!("hermes:exec:{}", exec.job_id),
            Agent::Hermes,
            kind,
            severity,
            // The bubble shows the title alone, so the job name must live here.
            format!("{name}：{headline}"),
            exec.at,
        )
        .with_detail(detail)
        .with_project(meta.and_then(|m| m.workdir.clone()))
        .with_meta("job", name)
        .with_meta("status", exec.status.clone());

        if let Some(m) = meta {
            ev = ev.with_meta("schedule", m.schedule.clone());
            if m.failure_streak > 0 {
                ev = ev.with_meta("failure_streak", m.failure_streak.to_string());
            }
        }

        // Stable per-firing id: same run must not re-announce on every poll.
        ev.id = format!("hermes:exec:{}:{}", exec.job_id, exec.at);
        out.push(ev);
    }

    // --- recorded incidents ---------------------------------------------------
    for (id, job_id, state, failure_type, last_seen, error, output_file) in incidents {
        let name = jobs
            .get(&job_id)
            .map(|m| m.name.clone())
            .unwrap_or(job_id.clone());
        let severity = if state == "closed" {
            Severity::Success
        } else {
            Severity::Error
        };
        let mut ev = AgentEvent::new(
            format!("hermes:incident:{id}"),
            Agent::Hermes,
            EventKind::Incident,
            severity,
            format!("{} 出现异常", name),
            last_seen.unwrap_or_else(now_millis),
        )
        .with_detail(first_line(&error, 200))
        .with_meta("state", state)
        .with_meta("failure_type", failure_type);
        if let Some(f) = output_file {
            ev = ev.with_meta("output_file", f);
        }
        out.push(ev);
    }

    // --- rollup ---------------------------------------------------------------
    // A job is "failing" if its most recent run did not land cleanly.  The
    // execution row is the richer signal; jobs.json's own `last_status` covers
    // runs that have already aged out of the execution window.
    let mut job_failing = 0usize;
    let mut job_ok = 0usize;
    let mut job_paused = 0usize;

    for (id, meta) in &jobs {
        if !meta.enabled {
            job_paused += 1;
            continue;
        }

        let latest = executions
            .iter()
            .filter(|e| &e.job_id == id)
            .max_by_key(|e| e.at);

        let bad_from_db = latest
            .map(|e| matches!(classify(e).1, Severity::Error | Severity::Warning))
            .unwrap_or(false);
        let bad_from_jobs = matches!(
            meta.last_status.as_deref(),
            Some("failed" | "error" | "delivery_failed")
        ) || meta.failure_streak > 0;

        if bad_from_db || bad_from_jobs {
            job_failing += 1;
        } else {
            job_ok += 1;
        }
        // Note: a failing `last_status` with no execution row in the window is
        // reported through the rollup only.  `last_error` is sticky — Hermes
        // does not clear it on success — so turning it into an event would
        // resurrect months-old failures as if they had just happened.
    }

    let _ = (failing, healthy);
    if let Some(m) = mtime_millis(&db_file) {
        last_activity = Some(last_activity.map_or(m, |p| p.max(m)));
    }

    summarize(
        Agent::Hermes,
        true,
        "个任务",
        job_ok,
        job_failing,
        job_paused,
        last_activity,
    )
}

/// Decide what an execution row means to the user.
fn classify(exec: &Execution) -> (EventKind, Severity, String) {
    match exec.status.as_str() {
        "failed" => (
            EventKind::JobFailed,
            Severity::Error,
            "定时任务失败".to_string(),
        ),
        "completed" if exec.delivery_outcome.as_deref() == Some("failed") => (
            EventKind::JobDeliveryFailed,
            Severity::Warning,
            "任务完成但投递失败".to_string(),
        ),
        "completed" => (
            EventKind::JobCompleted,
            Severity::Success,
            "定时任务完成".to_string(),
        ),
        "running" | "claimed" => (
            EventKind::JobScheduled,
            Severity::Info,
            "定时任务执行中".to_string(),
        ),
        other => (
            EventKind::JobCompleted,
            severity_for_status(other),
            "定时任务状态未知".to_string(),
        ),
    }
}

fn humanize_duration(ms: i64) -> String {
    let secs = (ms.max(0)) / 1000;
    if secs < 60 {
        format!("耗时 {secs}s")
    } else if secs < 3600 {
        format!("耗时 {}m{}s", secs / 60, secs % 60)
    } else {
        format!("耗时 {}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max).collect();
        t.push('…');
        t
    }
}

// ---------------------------------------------------------------------------
// loaders
// ---------------------------------------------------------------------------

fn load_jobs(path: &Path) -> HashMap<String, JobMeta> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    let Some(arr) = root.get("jobs").and_then(|j| j.as_array()) else {
        return map;
    };

    for job in arr {
        let Some(id) = job.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let schedule = job
            .get("schedule")
            .and_then(|s| s.get("display").or_else(|| s.get("expr")))
            .and_then(|v| v.as_str())
            .or_else(|| job.get("schedule_display").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        map.insert(
            id.to_string(),
            JobMeta {
                name: job
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(id)
                    .to_string(),
                schedule,
                enabled: job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                last_status: job
                    .get("last_status")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                failure_streak: job
                    .get("failure_streak")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                workdir: job.get("workdir").and_then(|v| v.as_str()).map(str::to_string),
            },
        );
    }
    map
}

/// Open the cron database read-only.
///
/// SQLite acquires locks and touches journal files even for a read-only open,
/// so opening another tool's live database directly can fail.  Worse, such an
/// open often *appears* to succeed and only errors on first use — so every
/// candidate handle is probed before we trust it, and a snapshot copy of the
/// database is used as the fallback.
fn open_db(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;

    if let Ok(conn) = Connection::open_with_flags(path, flags) {
        let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
        if probe(&conn) {
            return Some(conn);
        }
    }

    let scratch = std::env::temp_dir().join("suona-db");
    std::fs::create_dir_all(&scratch).ok()?;
    let dest = scratch.join(path.file_name()?);
    std::fs::copy(path, &dest).ok()?;

    // A WAL database is only consistent together with its sidecar files.
    for suffix in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", path.display()));
        if src.exists() {
            let dst = PathBuf::from(format!("{}{suffix}", dest.display()));
            let _ = std::fs::copy(&src, &dst);
        }
    }

    let conn = Connection::open_with_flags(&dest, flags).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
    probe(&conn).then_some(conn)
}

/// Force a real read so a lazily-failing handle is detected here.
fn probe(conn: &Connection) -> bool {
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .is_ok()
}

fn load_executions(path: &Path, ctx: &Ctx) -> Vec<Execution> {
    let Some(conn) = open_db(path) else {
        return Vec::new();
    };

    let sql = "SELECT job_id, status, claimed_at, started_at, finished_at, error, \
               delivery_outcome FROM executions ORDER BY claimed_at DESC LIMIT ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };

    // Look back a little further than `since` so the rollup is not blind.
    let cap = (ctx.scan_limit * 8).max(200) as i64;
    let rows = stmt.query_map([cap], |row| {
        let claimed: String = row.get(2)?;
        Ok(Execution {
            job_id: row.get(0)?,
            status: row.get(1)?,
            at: parse_millis(&claimed).unwrap_or(0),
            started_at: row
                .get::<_, Option<String>>(3)?
                .as_deref()
                .and_then(parse_millis),
            finished_at: row
                .get::<_, Option<String>>(4)?
                .as_deref()
                .and_then(parse_millis),
            error: row.get(5)?,
            delivery_outcome: row.get(6)?,
        })
    });

    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

type IncidentRow = (String, String, String, String, Option<i64>, String, Option<String>);

fn load_incidents(path: &Path, ctx: &Ctx) -> Vec<IncidentRow> {
    let Some(conn) = open_db(path) else {
        return Vec::new();
    };
    let sql = "SELECT id, job_id, state, failure_type, last_seen_at, error, output_file \
               FROM cron_incidents ORDER BY last_seen_at DESC LIMIT ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let cap = ctx.scan_limit as i64;
    let rows = stmt.query_map([cap], |row| {
        let last_seen: String = row.get(4)?;
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            parse_millis(&last_seen),
            row.get(5)?,
            row.get(6)?,
        ))
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}
