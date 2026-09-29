//! Unified event model.
//!
//! Every collector — regardless of which agent it reads from — normalises its
//! findings into [`AgentEvent`].  That is the single vocabulary the pet speaks.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which local agent an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Hermes,
    Codex,
    ClaudeCode,
}

impl Agent {
    pub fn key(self) -> &'static str {
        match self {
            Agent::Hermes => "hermes",
            Agent::Codex => "codex",
            Agent::ClaudeCode => "claude",
        }
    }

    /// Human-facing name, used in bubbles and notifications.
    pub fn label(self) -> &'static str {
        match self {
            Agent::Hermes => "Hermes",
            Agent::Codex => "Codex",
            Agent::ClaudeCode => "Claude Code",
        }
    }

    pub const ALL: [Agent; 3] = [Agent::Hermes, Agent::Codex, Agent::ClaudeCode];

    /// Where this agent keeps its data by default, relative to `$HOME`.
    pub fn default_dir(self) -> &'static str {
        match self {
            Agent::Hermes => ".hermes",
            Agent::Codex => ".codex",
            Agent::ClaudeCode => ".claude",
        }
    }

    /// Paths, relative to the agent's directory, whose presence means the tool
    /// is really installed there.
    ///
    /// Several per agent on purpose: a fresh install may not have written any
    /// session files yet, and calling that "not installed" would be wrong.
    pub fn markers(self) -> &'static [&'static str] {
        match self {
            Agent::Hermes => &["cron/jobs.json", "cron/executions.db", "config.yaml"],
            Agent::Codex => &["sessions", "session_index.jsonl"],
            Agent::ClaudeCode => &["projects"],
        }
    }

    pub fn from_key(key: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.key() == key)
    }
}

/// How much the pet cares.  Drives bubble colour and whether a system
/// notification is raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Routine activity, worth mentioning only when the user looks.
    Info,
    /// Something finished successfully.
    Success,
    /// Something needs attention but is not broken.
    Warning,
    /// Something failed.
    Error,
}

impl Severity {
    /// The pet only taps the user on the shoulder for these.
    pub fn deserves_notification(self) -> bool {
        matches!(self, Severity::Warning | Severity::Error)
    }

    /// Higher sorts first when the pet decides what to say.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Error => 3,
            Severity::Warning => 2,
            Severity::Success => 1,
            Severity::Info => 0,
        }
    }
}

/// What happened.  Kept deliberately small and cross-agent so the pet's
/// phrasing stays consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A scheduled job ran to completion.
    JobCompleted,
    /// A scheduled job errored out.
    JobFailed,
    /// The job's work succeeded but its result could not be delivered.
    JobDeliveryFailed,
    /// A scheduled job is due soon.
    JobScheduled,
    /// A scheduled job is disabled or paused.
    JobPaused,
    /// An interactive agent session began.
    SessionStarted,
    /// An interactive agent session finished.
    SessionCompleted,
    /// A recorded cron incident.
    Incident,
}

/// One normalised fact about an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Stable identity, used for de-duplication across polls.
    pub id: String,
    pub agent: Agent,
    pub kind: EventKind,
    pub severity: Severity,
    /// Bubble headline — short, already human-readable.
    pub title: String,
    /// One supporting line.  May be empty.
    pub detail: String,
    /// Working directory or thread name, when known.
    pub project: Option<String>,
    /// Epoch milliseconds.
    pub at: i64,
    /// Free-form extras the UI may surface on click.
    pub meta: BTreeMap<String, String>,
}

impl AgentEvent {
    pub fn new(
        id: impl Into<String>,
        agent: Agent,
        kind: EventKind,
        severity: Severity,
        title: impl Into<String>,
        at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            agent,
            kind,
            severity,
            title: title.into(),
            detail: String::new(),
            project: None,
            at,
            meta: BTreeMap::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    pub fn with_meta(mut self, key: &str, value: impl Into<String>) -> Self {
        self.meta.insert(key.to_string(), value.into());
        self
    }

    /// Short project label — the last path segment, which is what a human
    /// actually recognises.
    pub fn project_short(&self) -> Option<String> {
        self.project.as_deref().and_then(short_project)
    }
}

/// Turn `/Users/x/Desktop/Development/NOVOPlasty-rs` into `NOVOPlasty-rs`.
pub fn short_project(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let seg = trimmed.rsplit('/').next().unwrap_or(trimmed);
    // Claude stores `-Users-carpe-Desktop-foo` style slugs; take the tail.
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

/// Per-agent rollup shown when the user clicks the pet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub agent: Agent,
    pub label: String,
    /// True when the agent's data directory exists on this machine.
    pub detected: bool,
    /// False when the user switched this agent off in the configuration.
    pub enabled: bool,
    /// e.g. "6 个任务 · 1 个异常"
    pub headline: String,
    pub healthy: usize,
    pub failing: usize,
    /// Scheduled work that is switched off — state, not news.
    pub paused: usize,
    pub last_activity: Option<i64>,
}

/// Everything the front end needs for one render tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: i64,
    pub summaries: Vec<AgentSummary>,
    /// Newest first.
    pub events: Vec<AgentEvent>,
}
