//! Per-agent configuration: where each tool lives, and whether suona watches it.
//!
//! suona is strictly read-only.  "Removing" an agent means switching it off
//! here — it stops being collected and reported, and nothing on disk is
//! touched.  Flipping the switch back restores it completely.

use crate::model::Agent;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One agent's entry in `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSetting {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Base directory.  Empty means "use the built-in default", which keeps
    /// settings portable across machines with different layouts.
    #[serde(default)]
    pub path: String,
}

fn default_enabled() -> bool {
    true
}

impl Default for AgentSetting {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
        }
    }
}

/// The whole agent section of the settings file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSettings {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSetting>,
}

/// How an agent's configured directory looks right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectStatus {
    /// The directory holds a recognisable file for this tool.
    Detected,
    /// The directory exists but looks like it belongs to something else.
    Incomplete,
    /// Nothing at that path.
    Missing,
    /// Switched off; not checked.
    Disabled,
}

/// What the configuration UI shows for one agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentConfigView {
    pub key: String,
    pub label: String,
    pub enabled: bool,
    /// Effective directory, with `$HOME` shortened to `~` for readability.
    pub path: String,
    pub default_path: String,
    /// True when the user has pointed this agent somewhere of their own.
    pub custom: bool,
    pub status: DetectStatus,
    /// One line explaining the status.
    pub detail: String,
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Expand a leading `~` so users can type paths the way they think of them.
pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return home();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(trimmed)
}

/// Shorten a path under `$HOME` back to `~/…` for display and editing.
pub fn shorten_home(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        Ok(_) => "~".to_string(),
        Err(_) => path.display().to_string(),
    }
}

impl AgentSettings {
    /// The directory to read this agent from, using the default when unset.
    pub fn dir(&self, agent: Agent) -> PathBuf {
        match self.agents.get(agent.key()) {
            Some(setting) if !setting.path.trim().is_empty() => expand_tilde(&setting.path),
            _ => home().join(agent.default_dir()),
        }
    }

    pub fn is_enabled(&self, agent: Agent) -> bool {
        self.agents
            .get(agent.key())
            .map(|s| s.enabled)
            .unwrap_or(true)
    }

    /// Base directories for every agent suona should currently collect.
    pub fn active_dirs(&self) -> BTreeMap<Agent, PathBuf> {
        Agent::ALL
            .into_iter()
            .filter(|a| self.is_enabled(*a))
            .map(|a| (a, self.dir(a)))
            .collect()
    }

    pub fn set_enabled(&mut self, agent: Agent, enabled: bool) {
        self.agents.entry(agent.key().to_string()).or_default().enabled = enabled;
    }

    /// Point an agent at a directory.  An empty string restores the default.
    pub fn set_path(&mut self, agent: Agent, path: &str) {
        self.agents.entry(agent.key().to_string()).or_default().path = path.trim().to_string();
    }

    pub fn is_custom(&self, agent: Agent) -> bool {
        self.agents
            .get(agent.key())
            .map(|s| !s.path.trim().is_empty())
            .unwrap_or(false)
    }

    /// Describe every agent for the configuration UI.
    pub fn views(&self) -> Vec<AgentConfigView> {
        Agent::ALL
            .into_iter()
            .map(|agent| {
                let dir = self.dir(agent);
                let enabled = self.is_enabled(agent);
                let (status, detail) = if enabled {
                    probe(agent, &dir)
                } else {
                    (DetectStatus::Disabled, "已停用，不再汇报".to_string())
                };
                AgentConfigView {
                    key: agent.key().to_string(),
                    label: agent.label().to_string(),
                    enabled,
                    path: shorten_home(&dir),
                    default_path: shorten_home(&home().join(agent.default_dir())),
                    custom: self.is_custom(agent),
                    status,
                    detail,
                }
            })
            .collect()
    }
}

/// Look for this agent's own files at `dir`.
pub fn probe(agent: Agent, dir: &Path) -> (DetectStatus, String) {
    if !dir.exists() {
        return (
            DetectStatus::Missing,
            format!("目录不存在：{}", dir.display()),
        );
    }
    if !dir.is_dir() {
        return (DetectStatus::Incomplete, "这个路径不是目录".to_string());
    }

    for marker in agent.markers() {
        if dir.join(marker).exists() {
            return (DetectStatus::Detected, format!("找到 {marker}"));
        }
    }

    (
        DetectStatus::Incomplete,
        "目录存在，但没有该工具的数据".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_round_trips() {
        let home = home();
        assert_eq!(expand_tilde("~/somewhere"), home.join("somewhere"));
        assert_eq!(expand_tilde("~"), home);
        // An absolute path is left alone.
        assert_eq!(expand_tilde("/tmp/x"), PathBuf::from("/tmp/x"));

        // And formatting puts the tilde back, so the field reads naturally.
        assert_eq!(shorten_home(&home.join("somewhere")), "~/somewhere");
        assert_eq!(shorten_home(&home), "~");
        assert_eq!(shorten_home(Path::new("/tmp/x")), "/tmp/x");
    }

    #[test]
    fn defaults_are_used_until_overridden() {
        let mut settings = AgentSettings::default();
        assert_eq!(
            settings.dir(Agent::Hermes),
            home().join(".hermes"),
            "an unset agent must fall back to its default location"
        );
        assert!(settings.is_enabled(Agent::Hermes));
        assert!(!settings.is_custom(Agent::Hermes));

        settings.set_path(Agent::Hermes, "~/custom-hermes");
        assert_eq!(settings.dir(Agent::Hermes), home().join("custom-hermes"));
        assert!(settings.is_custom(Agent::Hermes));

        // Clearing the field restores the default rather than pointing at "".
        settings.set_path(Agent::Hermes, "");
        assert_eq!(settings.dir(Agent::Hermes), home().join(".hermes"));
        assert!(!settings.is_custom(Agent::Hermes));
    }

    #[test]
    fn disabling_removes_an_agent_from_collection() {
        let mut settings = AgentSettings::default();
        assert_eq!(settings.active_dirs().len(), Agent::ALL.len());

        settings.set_enabled(Agent::Codex, false);
        let active = settings.active_dirs();
        assert!(!active.contains_key(&Agent::Codex));
        assert_eq!(active.len(), Agent::ALL.len() - 1);

        // And it is reported as disabled rather than missing.
        let view = settings
            .views()
            .into_iter()
            .find(|v| v.key == "codex")
            .unwrap();
        assert!(!view.enabled);
        assert_eq!(view.status, DetectStatus::Disabled);
    }

    #[test]
    fn probe_reports_missing_and_detected() {
        let tmp = std::env::temp_dir().join(format!("suona-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let (status, _) = probe(Agent::Codex, &tmp);
        assert_eq!(status, DetectStatus::Missing);

        std::fs::create_dir_all(tmp.join("sessions")).unwrap();
        let (status, detail) = probe(Agent::Codex, &tmp);
        assert_eq!(status, DetectStatus::Detected);
        assert!(detail.contains("sessions"), "got: {detail}");

        // A directory holding someone else's data must not read as detected.
        let other = std::env::temp_dir().join(format!("suona-probe-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(probe(Agent::Codex, &other).0, DetectStatus::Incomplete);

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// The "重新检测" button depends on this: `views()` must look at the disk
    /// every time it is called, not cache the first answer.  If someone later
    /// adds caching for speed, installing a tool while the settings page is
    /// open would silently stop being noticed — and this test would fail.
    #[test]
    fn rescanning_notices_a_newly_installed_agent() {
        let dir = std::env::temp_dir().join(format!("suona-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut settings = AgentSettings::default();
        settings.set_path(Agent::Codex, &dir.display().to_string());

        let status_of = |s: &AgentSettings| {
            s.views()
                .into_iter()
                .find(|v| v.key == "codex")
                .expect("codex view")
                .status
        };

        assert_eq!(status_of(&settings), DetectStatus::Missing);

        // The user installs the tool between two presses of the button.
        std::fs::create_dir_all(dir.join("sessions")).unwrap();

        assert_eq!(
            status_of(&settings),
            DetectStatus::Detected,
            "detection must be re-run, not remembered"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
