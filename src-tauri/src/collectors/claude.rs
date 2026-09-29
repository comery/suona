//! Claude Code collector.
//!
//! Layout: `~/.claude/projects/<path-slug>/<session-uuid>.jsonl`, with nested
//! `subagents/agent-*.jsonl` files for delegated work.
//!
//! Sessions here have no explicit "finished" record — the transcript simply
//! stops growing.  We therefore treat a session as live while its file was
//! modified within the last few minutes, and report token usage from the tail
//! of the transcript.

use super::{mtime_millis, newest_files, now_millis, parse_millis, read_jsonl_tail, summarize, Ctx,
    disabled,};
use crate::model::{short_project, Agent, AgentEvent, AgentSummary, EventKind, Severity};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Transcripts reach several megabytes; only the recent window matters.
const TAIL_BYTES: u64 = 1024 * 1024;

/// A session counts as live if written to this recently.
const LIVE_WINDOW_MS: i64 = 5 * 60 * 1000;

pub fn collect(ctx: &Ctx, out: &mut Vec<AgentEvent>) -> AgentSummary {
    // Absent from the context means the user switched it off; nothing is read.
    let Some(base) = ctx.dir(Agent::ClaudeCode) else {
        return disabled(Agent::ClaudeCode);
    };
    let projects = base.join("projects");
    if !projects.exists() {
        return summarize(Agent::ClaudeCode, false, "个会话", 0, 0, 0, None);
    }

    let files = session_files(&projects, ctx.scan_limit);
    let mut completed = 0usize;
    let mut live = 0usize;
    let mut last_activity: Option<i64> = None;
    let mut emitted = 0usize;

    for file in files {
        let Some(session) = scan_session(&file) else {
            continue;
        };
        last_activity = Some(last_activity.map_or(session.at, |p| p.max(session.at)));
        if session.at < ctx.since {
            continue;
        }
        emitted += 1;

        let (severity, kind, headline) = if session.live {
            live += 1;
            (Severity::Info, EventKind::SessionStarted, "正在工作")
        } else {
            completed += 1;
            (Severity::Success, EventKind::SessionCompleted, "会话已结束")
        };

        let mut detail = Vec::new();
        if session.prompts > 0 {
            detail.push(format!("{} 次提问", session.prompts));
        }
        if let Some(p) = session.project.as_deref().and_then(short_project) {
            detail.push(p);
        }
        if session.output_tokens > 0 {
            detail.push(format!("输出 {} tokens", session.output_tokens));
        }
        if let Some(m) = session.model.as_deref() {
            detail.push(m.to_string());
        }

        let ev = AgentEvent::new(
            format!("claude:session:{}", session.id),
            Agent::ClaudeCode,
            kind,
            severity,
            format!("{}：{headline}", session.title),
            session.at,
        )
        .with_detail(detail.join(" · "))
        .with_project(session.project.clone())
        .with_meta("session_id", session.id.clone())
        .with_meta("path", file.display().to_string())
        .with_meta("subagents", session.subagents.to_string());

        out.push(ev);
    }

    let detected = emitted > 0 || projects.exists();
    summarize(
        Agent::ClaudeCode,
        detected,
        "个会话",
        completed,
        live,
        0,
        last_activity,
    )
}

struct Session {
    id: String,
    title: String,
    project: Option<String>,
    model: Option<String>,
    prompts: usize,
    output_tokens: u64,
    subagents: usize,
    live: bool,
    at: i64,
}

/// Top-level session files only — `subagents/` transcripts are counted, not
/// reported as sessions of their own.
fn session_files(projects: &Path, limit: usize) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(projects) else {
        return all;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let mut found = newest_files(&dir, "jsonl", limit);
        // newest_files recurses; keep only direct children of the project dir.
        found.retain(|p| p.parent() == Some(dir.as_path()));
        all.extend(found);
    }
    // Rank across all projects by modification time.
    all.sort_by_key(|p| std::cmp::Reverse(mtime_millis(p).unwrap_or(0)));
    all.truncate(limit);
    all
}

fn scan_session(path: &Path) -> Option<Session> {
    let id = path.file_stem()?.to_str()?.to_string();
    let records = read_jsonl_tail(path, TAIL_BYTES);
    if records.is_empty() {
        return None;
    }

    let mut title: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    let mut project: Option<String> = None;
    let mut model: Option<String> = None;
    let mut prompts = 0usize;
    let mut output_tokens = 0u64;
    let mut at: Option<i64> = None;

    for rec in &records {
        if let Some(ts) = rec.get("timestamp").and_then(|v| v.as_str()).and_then(parse_millis) {
            at = Some(at.map_or(ts, |p| p.max(ts)));
        }
        if project.is_none() {
            if let Some(cwd) = rec.get("cwd").and_then(|v| v.as_str()) {
                project = Some(cwd.to_string());
            }
        }

        match rec.get("type").and_then(|t| t.as_str()) {
            // Latest title wins — Claude rewrites it as the topic sharpens.
            Some("ai-title") => {
                if let Some(t) = rec.get("aiTitle").and_then(|v| v.as_str()) {
                    if !t.trim().is_empty() {
                        title = Some(t.to_string());
                    }
                }
            }
            Some("last-prompt") => {
                if let Some(p) = rec.get("lastPrompt").and_then(|v| v.as_str()) {
                    last_prompt = Some(p.to_string());
                }
            }
            Some("user") => {
                // A real prompt is a plain string; tool results arrive as blocks.
                let content = rec.get("message").and_then(|m| m.get("content"));
                if matches!(content, Some(Value::String(_))) {
                    prompts += 1;
                }
            }
            Some("assistant") => {
                let msg = rec.get("message");
                if let Some(m) = msg.and_then(|m| m.get("model")).and_then(|v| v.as_str()) {
                    model = Some(m.to_string());
                }
                if let Some(out) = msg
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    output_tokens += out;
                }
            }
            _ => {}
        }
    }

    let mtime = mtime_millis(path).unwrap_or_else(now_millis);
    let at = at.unwrap_or(mtime).max(0);

    let title = title
        .or_else(|| last_prompt.map(|p| truncate(&p, 24)))
        .or_else(|| project.as_deref().and_then(short_project))
        .unwrap_or_else(|| "Claude Code 会话".to_string());

    Some(Session {
        id,
        title,
        project,
        model,
        prompts,
        output_tokens,
        subagents: count_subagents(path),
        live: (now_millis() - mtime) < LIVE_WINDOW_MS,
        at,
    })
}

/// `…/<uuid>.jsonl` may sit beside a `…/<uuid>/subagents/` directory.
fn count_subagents(path: &Path) -> usize {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return 0;
    };
    let Some(parent) = path.parent() else {
        return 0;
    };
    let dir = parent.join(stem).join("subagents");
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().count())
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or("").trim();
    if one_line.chars().count() <= max {
        one_line.to_string()
    } else {
        let mut t: String = one_line.chars().take(max).collect();
        t.push('…');
        t
    }
}
