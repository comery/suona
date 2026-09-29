//! Data-source adapters.
//!
//! Each submodule knows one agent's on-disk layout and nothing else.  They all
//! return the same [`AgentEvent`] vocabulary via [`collect_all`].

pub mod claude;
pub mod codex;
pub mod hermes;

use crate::agents::AgentSettings;
use crate::model::{Agent, AgentEvent, AgentSummary, Severity};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Shared configuration and helpers handed to every collector.
pub struct Ctx {
    /// Ignore activity older than this (epoch millis).
    pub since: i64,
    /// How many recent items per source to inspect.
    pub scan_limit: usize,
    /// Base directory per agent, already resolved from the user's settings.
    /// A disabled agent is simply absent, which is how it stops being read.
    dirs: BTreeMap<Agent, PathBuf>,
}

impl Ctx {
    pub fn new(since: i64, scan_limit: usize, agents: &AgentSettings) -> Self {
        Self {
            since,
            scan_limit,
            dirs: agents.active_dirs(),
        }
    }

    /// The configured directory for an agent, or `None` when it is switched off.
    pub fn dir(&self, agent: Agent) -> Option<&Path> {
        self.dirs.get(&agent).map(PathBuf::as_path)
    }
}

/// Run every collector and return the merged event list plus per-agent rollups.
pub fn collect_all(ctx: &Ctx) -> (Vec<AgentEvent>, Vec<AgentSummary>) {
    let mut events = Vec::new();

    let summaries = vec![
        hermes::collect(ctx, &mut events),
        codex::collect(ctx, &mut events),
        claude::collect(ctx, &mut events),
    ];

    // Newest first, then by severity so that ties surface the alarming one.
    events.sort_by(|a, b| {
        b.at.cmp(&a.at)
            .then_with(|| b.severity.rank().cmp(&a.severity.rank()))
    });

    (events, summaries)
}

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

/// Parse the several RFC3339 spellings these tools emit into epoch millis.
pub fn parse_millis(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // Hermes writes naive local timestamps in a couple of places.
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    None
}

/// Epoch seconds (codex `started_at`) to millis.
pub fn secs_to_millis(secs: i64) -> i64 {
    secs.saturating_mul(1000)
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// filesystem
// ---------------------------------------------------------------------------

/// Read at most `max_bytes` from the end of a file — enough for a JSONL tail
/// without slurping multi-megabyte transcripts.
pub fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        f.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;
    // Drop the first, likely partial, line when we started mid-file.
    if start > 0 {
        if let Some(nl) = buf.find('\n') {
            buf.drain(..=nl);
        }
    }
    Some(buf)
}

/// Read the first `max_bytes` of a file.  Codex writes its `session_meta`
/// record at offset zero, so the head is where identity lives.
pub fn read_head(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes as usize];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    // Drop a trailing partial line so the caller only sees whole records.
    let text = String::from_utf8_lossy(&buf).into_owned();
    match text.rfind('\n') {
        Some(cut) => Some(text[..cut].to_string()),
        None => Some(text),
    }
}

/// Parse a JSONL file's head into values.
pub fn read_jsonl_head(path: &Path, max_bytes: u64) -> Vec<serde_json::Value> {
    let Some(text) = read_head(path, max_bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Parse a JSONL file's tail into values, silently skipping malformed lines.
pub fn read_jsonl_tail(path: &Path, max_bytes: u64) -> Vec<serde_json::Value> {
    let Some(text) = read_tail(path, max_bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Newest files under `dir` (recursively, one level of glob-free matching)
/// filtered by extension, most recently modified first.
pub fn newest_files(dir: &Path, ext: &str, limit: usize) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    walk(dir, ext, &mut found);
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().take(limit).map(|(_, p)| p).collect()
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            walk(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            out.push((mtime, path));
        }
    }
}

/// File mtime as epoch millis.
pub fn mtime_millis(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// Build a rollup, deriving the counts the pet quotes in its status line.
pub fn summarize(
    agent: Agent,
    detected: bool,
    noun: &str,
    healthy: usize,
    failing: usize,
    paused: usize,
    last_activity: Option<i64>,
) -> AgentSummary {
    let headline = if !detected {
        "未检测到".to_string()
    } else if healthy + failing == 0 && paused == 0 {
        // Installed but nothing recorded yet.  "0 个会话 · 全部正常" would
        // claim health we have no evidence for; say what is actually true.
        "已安装 · 暂无记录".to_string()
    } else {
        let total = healthy + failing;
        let mut parts = vec![format!("{total} {noun}")];
        if failing > 0 {
            parts.push(format!("{failing} 个异常"));
        }
        if paused > 0 {
            parts.push(format!("{paused} 个暂停"));
        }
        if failing == 0 && paused == 0 {
            parts.push("全部正常".to_string());
        }
        parts.join(" · ")
    };

    AgentSummary {
        agent,
        label: agent.label().to_string(),
        detected,
        enabled: true,
        headline,
        healthy,
        failing,
        paused,
        last_activity,
    }
}

/// Rollup for an agent the user switched off, so the UI can say so rather than
/// pretending it is missing.
pub fn disabled(agent: Agent) -> AgentSummary {
    AgentSummary {
        agent,
        label: agent.label().to_string(),
        detected: false,
        enabled: false,
        headline: "已停用".to_string(),
        healthy: 0,
        failing: 0,
        paused: 0,
        last_activity: None,
    }
}

/// Convenience: severity from an execution/status string.
pub fn severity_for_status(status: &str) -> Severity {
    match status {
        "ok" | "completed" | "success" | "succeeded" => Severity::Success,
        "failed" | "error" | "errored" | "crashed" => Severity::Error,
        "delivery_failed" | "partial" | "timeout" | "timed_out" => Severity::Warning,
        _ => Severity::Info,
    }
}
