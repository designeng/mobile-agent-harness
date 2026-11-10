use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub app: AppCfg,
    #[serde(default)]
    pub device: DeviceCfg,
    pub github: GithubCfg,
    #[serde(default)]
    pub agents: AgentsCfg,
    #[serde(default)]
    pub queue: QueueCfg,
    #[serde(default)]
    pub skills: SkillsCfg,
    /// Directory containing harness.toml; relative paths resolve against it.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppCfg {
    /// Path to the Android app git repository.
    pub repo: PathBuf,
    /// Application id, e.g. com.example.app
    pub package: String,
    /// Build command, run inside the task worktree.
    pub build: String,
    /// APK path relative to the worktree root.
    pub apk: PathBuf,
    /// Optional unit test command, run inside the worktree.
    pub unit_tests: Option<String>,
    /// Maestro flows directory relative to the worktree root.
    #[serde(default = "d_flows")]
    pub flows_dir: PathBuf,
    #[serde(default = "d_branch")]
    pub base_branch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCfg {
    /// adb serial; if omitted, the single connected device is used.
    pub serial: Option<String>,
    #[serde(default = "d_adb")]
    pub adb: String,
    #[serde(default = "d_maestro")]
    pub maestro: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubCfg {
    /// owner/name
    pub repo: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentsCfg {
    pub model: Option<String>,
    /// Cheaper model for planner/reviewer, falls back to `model`.
    pub light_model: Option<String>,
    #[serde(default = "d_iters")]
    pub max_fix_iterations: u32,
    /// Command (argv) that launches mobile-mcp.
    #[serde(default = "d_mobile_mcp")]
    pub mobile_mcp: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueCfg {
    /// Only open issues with this label are eligible for `harness queue`.
    #[serde(default = "d_ready_label")]
    pub label: String,
    /// Stop starting new issues once runs since the queue started cost this much.
    pub budget_usd: Option<f64>,
    /// Stop after this many issues fail in a row.
    #[serde(default = "d_max_failures")]
    pub max_consecutive_failures: u32,
}

/// Claude Code skills handed to agents, per role. Each name is a folder `<dir>/<name>/SKILL.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsCfg {
    /// Skills library, relative to harness.toml.
    #[serde(default = "d_skills_dir")]
    pub dir: PathBuf,
    /// Given to every role.
    #[serde(default)]
    pub all: Vec<String>,
    #[serde(default)]
    pub planner: Vec<String>,
    #[serde(default)]
    pub developer: Vec<String>,
    /// Also used by `harness explore`.
    #[serde(default)]
    pub qa: Vec<String>,
    #[serde(default)]
    pub reviewer: Vec<String>,
    #[serde(default)]
    pub picker: Vec<String>,
}

impl Default for SkillsCfg {
    fn default() -> Self {
        Self {
            dir: d_skills_dir(),
            all: vec![],
            planner: vec![],
            developer: vec![],
            qa: vec![],
            reviewer: vec![],
            picker: vec![],
        }
    }
}

impl Default for QueueCfg {
    fn default() -> Self {
        Self { label: d_ready_label(), budget_usd: None, max_consecutive_failures: d_max_failures() }
    }
}

impl Default for DeviceCfg {
    fn default() -> Self {
        Self { serial: None, adb: d_adb(), maestro: d_maestro() }
    }
}

impl Default for AgentsCfg {
    fn default() -> Self {
        Self { model: None, light_model: None, max_fix_iterations: d_iters(), mobile_mcp: d_mobile_mcp() }
    }
}

fn d_flows() -> PathBuf { ".maestro".into() }
fn d_branch() -> String { "main".into() }
fn d_adb() -> String { "adb".into() }
fn d_maestro() -> String { "~/.maestro/maestro/bin/maestro".into() }
fn d_iters() -> u32 { 3 }
fn d_ready_label() -> String { "agent-ready".into() }
fn d_max_failures() -> u32 { 2 }
fn d_skills_dir() -> PathBuf { "skills".into() }
fn d_mobile_mcp() -> Vec<String> {
    ["npx", "-y", "@mobilenext/mobile-mcp@1.0.16"].map(String::from).to_vec()
}

pub fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        cfg.base_dir = std::fs::canonicalize(if base.as_os_str().is_empty() { ".".into() } else { base })?;
        if cfg.app.repo.is_relative() {
            cfg.app.repo = cfg.base_dir.join(&cfg.app.repo);
        }
        cfg.device.maestro = expand_home(&cfg.device.maestro);
        if cfg.skills.dir.is_relative() {
            cfg.skills.dir = cfg.base_dir.join(&cfg.skills.dir);
        }
        Ok(cfg)
    }

    /// State dir for worktrees and run artifacts.
    pub fn state_dir(&self) -> PathBuf {
        self.base_dir.join(".harness")
    }

    pub fn dev_model(&self) -> Option<&str> {
        self.agents.model.as_deref()
    }

    pub fn light_model(&self) -> Option<&str> {
        self.agents.light_model.as_deref().or(self.dev_model())
    }

    /// Skill names for a role (`all` first, deduplicated).
    pub fn skills_for(&self, role: &str) -> Vec<&str> {
        let s = &self.skills;
        let role_list = match role {
            "planner" => &s.planner,
            "developer" => &s.developer,
            "qa" => &s.qa,
            "reviewer" => &s.reviewer,
            "picker" => &s.picker,
            _ => &s.all, // unknown role: `all` only (deduped below)
        };
        let mut out: Vec<&str> = vec![];
        for n in s.all.iter().chain(role_list) {
            if !out.contains(&n.as_str()) {
                out.push(n);
            }
        }
        out
    }

    /// Checks every configured skill exists; returns the distinct names.
    pub fn check_skills(&self) -> Result<Vec<String>> {
        let s = &self.skills;
        let mut names: Vec<String> = vec![];
        for n in [&s.all, &s.planner, &s.developer, &s.qa, &s.reviewer, &s.picker].into_iter().flatten() {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        for n in &names {
            self.skill_path(n)?;
        }
        Ok(names)
    }

    /// Folder of a skill in the library; errors if it has no SKILL.md.
    pub fn skill_path(&self, name: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..",
            "bad skill name {name:?}"
        );
        let p = self.skills.dir.join(name);
        anyhow::ensure!(p.join("SKILL.md").is_file(), "skill {name:?}: no {}", p.join("SKILL.md").display());
        Ok(p)
    }
}
