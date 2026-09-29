//! Codex collector.
//!
//! Layout:
//! * `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — one file per session.
//!   Identity (`session_meta`) sits at offset zero; turn lifecycle events
//!   (`event_msg: task_started` / `task_complete`) are appended as work happens.
//! * `~/.codex/session_index.jsonl` — maps session id to a human thread name.

use super::{
    newest_files, now_millis, parse_millis, read_jsonl_head, read_jsonl_tail, secs_to_millis,
    summarize, Ctx,
    disabled,};
use crate::model::{short_project, Agent, AgentEvent, AgentSummary, EventKind, Severity};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

const HEAD_BYTES: u64 = 64 * 1024;
const TAIL_BYTES: u64 = 512 * 1024;

pub fn collect(ctx: &Ctx, out: &mut Vec<AgentEvent>) -> AgentSummary {
    // Absent from the context means the user switched it off; nothing is read.
    let Some(codex_dir) = ctx.dir(Agent::Codex) else {
        return disabled(Agent::Codex);
    };
    if !codex_dir.exists() {
        return summarize(Agent::Codex, false, "个会话", 0, 0, 0, None);
    }

    let names = load_thread_names(&codex_dir.join("session_index.jsonl"));
    let sessions_dir = codex_dir.join("sessions");
    let files = newest_files(&sessions_dir, "jsonl", ctx.scan_limit);

    let mut completed = 0usize;
    let mut unfinished = 0usize;
    let mut last_activity: Option<i64> = None;
    let mut emitted = 0usize;

    for file in files {
        let Some(session) = scan_session(&file, &names) else {
            continue;
        };
        last_activity = Some(last_activity.map_or(session.at, |p| p.max(session.at)));
        if session.at < ctx.since {
            continue;
        }
        emitted += 1;

        let (severity, headline) = if session.in_flight {
            unfinished += 1;
            (Severity::Info, "会话进行中")
        } else {
            completed += 1;
            (Severity::Success, "会话已结束")
        };

        let mut detail = Vec::new();
        if session.turns > 0 {
            detail.push(format!("{} 轮", session.turns));
        }
        if let Some(p) = session.project.as_deref().and_then(short_project) {
            detail.push(p);
        }
        if let Some(v) = session.cli_version.as_deref() {
            detail.push(format!("v{v}"));
        }

        let ev = AgentEvent::new(
            format!("codex:session:{}", session.id),
            Agent::Codex,
            if session.in_flight {
                EventKind::SessionStarted
            } else {
                EventKind::SessionCompleted
            },
            severity,
            format!("{}：{headline}", session.title),
            session.at,
        )
        .with_detail(detail.join(" · "))
        .with_project(session.project.clone())
        .with_meta("session_id", session.id.clone())
        .with_meta("path", file.display().to_string());

        out.push(ev);
    }

    let detected = emitted > 0 || sessions_dir.exists();
    summarize(
        Agent::Codex,
        detected,
        "个会话",
        completed,
        unfinished,
        0,
        last_activity,
    )
}

struct Session {
    id: String,
    title: String,
    project: Option<String>,
    cli_version: Option<String>,
    turns: usize,
    in_flight: bool,
    at: i64,
}

fn scan_session(path: &Path, names: &HashMap<String, String>) -> Option<Session> {
    let head = read_jsonl_head(path, HEAD_BYTES);
    let meta = head
        .iter()
        .find(|r| r.get("type").and_then(|t| t.as_str()) == Some("session_meta"))?
        .get("payload")?;

    let id = meta
        .get("session_id")
        .or_else(|| meta.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let project = meta.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
    let cli_version = meta
        .get("cli_version")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Walk the tail to count turns and work out whether the last one closed.
    let tail = read_jsonl_tail(path, TAIL_BYTES);
    let mut starts = 0usize;
    let mut completes = 0usize;
    let mut last_ts: Option<i64> = None;
    let mut last_was_complete = false;
    let mut open_turn = false;

    for rec in &tail {
        if let Some(ts) = rec.get("timestamp").and_then(|v| v.as_str()).and_then(parse_millis) {
            last_ts = Some(last_ts.map_or(ts, |p| p.max(ts)));
        }
        if rec.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(payload) = rec.get("payload") else {
            continue;
        };
        match payload.get("type").and_then(|t| t.as_str()) {
            Some("task_started") => {
                starts += 1;
                open_turn = true;
                last_was_complete = false;
                if let Some(started) = payload.get("started_at").and_then(|v| v.as_i64()) {
                    let ts = secs_to_millis(started);
                    last_ts = Some(last_ts.map_or(ts, |p| p.max(ts)));
                }
            }
            Some("task_complete") => {
                completes += 1;
                open_turn = false;
                last_was_complete = true;
            }
            _ => {}
        }
    }

    let at = last_ts.or_else(|| super::mtime_millis(path)).unwrap_or_else(now_millis);

    let title = names
        .get(&id)
        .cloned()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            project
                .as_deref()
                .and_then(short_project)
                .unwrap_or_else(|| "Codex 会话".to_string())
        });

    Some(Session {
        id,
        title,
        project,
        cli_version,
        turns: starts.max(completes),
        // A session is "in flight" only if it has an unclosed turn AND has
        // been touched recently — otherwise it just means the window was closed.
        in_flight: open_turn && !last_was_complete && (now_millis() - at) < 15 * 60 * 1000,
        at,
    })
}

fn load_thread_names(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            v.get("id").and_then(|x| x.as_str()),
            v.get("thread_name").and_then(|x| x.as_str()),
        ) {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}
