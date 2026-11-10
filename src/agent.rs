//! Runs one agent role as a headless `claude -p` session.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub struct AgentSpec {
    pub role: &'static str,
    pub system: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub mcp_config: Option<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
    pub json_schema: Option<Value>,
}

#[derive(Debug)]
pub struct AgentResult {
    pub text: String,
    pub structured: Option<Value>,
    pub cost_usd: f64,
}

impl AgentSpec {
    pub fn new(role: &'static str, system: &str, prompt: String, cwd: &Path) -> Self {
        Self {
            role,
            system: system.to_string(),
            prompt,
            cwd: cwd.to_path_buf(),
            model: None,
            allowed_tools: vec![],
            disallowed_tools: vec![],
            mcp_config: None,
            add_dirs: vec![],
            json_schema: None,
        }
    }
}

/// Streams the transcript (stream-json) into `log` and returns the final result.
pub async fn run(spec: AgentSpec, log: &Path) -> Result<AgentResult> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .args(["--output-format", "stream-json", "--verbose"])
        // Tools outside the allow-list are denied, never prompted.
        .args(["--permission-mode", "dontAsk"])
        // Ignore the user's global plugins/hooks: roles must be reproducible.
        .args(["--setting-sources", "project"])
        .arg("--strict-mcp-config")
        .arg("--no-session-persistence")
        .args(["--append-system-prompt", &spec.system])
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(m) = &spec.model {
        cmd.args(["--model", m]);
    }
    if !spec.allowed_tools.is_empty() {
        cmd.arg("--allowedTools").args(&spec.allowed_tools);
    }
    if !spec.disallowed_tools.is_empty() {
        cmd.arg("--disallowedTools").args(&spec.disallowed_tools);
    }
    if let Some(c) = &spec.mcp_config {
        cmd.arg("--mcp-config").arg(c);
    }
    for d in &spec.add_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    if let Some(s) = &spec.json_schema {
        cmd.args(["--json-schema", &s.to_string()]);
    }

    tracing::info!(role = spec.role, "agent start");
    let mut child = cmd.spawn().context("spawning claude")?;
    // Prompt goes via stdin: avoids argv limits and option parsing surprises.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(spec.prompt.as_bytes()).await?;
    drop(stdin);

    let mut log_file = tokio::fs::File::create(log).await?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut result: Option<Value> = None;
    while let Some(line) = lines.next_line().await? {
        log_file.write_all(line.as_bytes()).await?;
        log_file.write_all(b"\n").await?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("assistant") => log_progress(spec.role, &v),
            Some("result") => result = Some(v),
            _ => {}
        }
    }
    let status = child.wait().await?;
    let Some(res) = result else {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = e.read_to_string(&mut err).await;
        }
        bail!("{} agent exited ({status}) without a result: {}", spec.role, err.trim());
    };
    let cost = res["total_cost_usd"].as_f64().unwrap_or(0.0);
    if res["is_error"].as_bool().unwrap_or(false) {
        bail!("{} agent failed ({}): {}", spec.role, res["subtype"], res["result"]);
    }
    tracing::info!(role = spec.role, cost_usd = format!("{cost:.3}"), "agent done");
    Ok(AgentResult {
        text: res["result"].as_str().unwrap_or_default().to_string(),
        structured: res.get("structured_output").filter(|v| !v.is_null()).cloned(),
        cost_usd: cost,
    })
}

fn log_progress(role: &str, v: &Value) {
    let Some(blocks) = v["message"]["content"].as_array() else { return };
    for b in blocks {
        if b["type"] == "tool_use" {
            let name = b["name"].as_str().unwrap_or("?");
            let input = b["input"].to_string();
            let short: String = input.chars().take(120).collect();
            tracing::info!(role, "→ {name} {short}");
        }
    }
}
