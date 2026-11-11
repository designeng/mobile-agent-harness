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
    /// Keep the Claude session on disk so it can be resumed later (developer only:
    /// other roles start fresh every time and would just clutter the session store).
    pub persist_session: bool,
    /// Session id for a new persisted session; assigned up front so it is known
    /// (and recorded) even if the process is killed mid-way.
    pub session_id: Option<String>,
    /// Continue this earlier session instead of starting a new one; `prompt` is the next user turn.
    pub resume: Option<String>,
}

#[derive(Debug, Default)]
pub struct AgentResult {
    pub text: String,
    pub structured: Option<Value>,
    pub cost_usd: f64,
    pub model: Option<String>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub session_id: Option<String>,
    /// Set when the session ended with `is_error`; cost and usage are still filled in.
    pub error: Option<String>,
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
            persist_session: false,
            session_id: None,
            resume: None,
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
        .args(["--append-system-prompt", &spec.system])
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !spec.persist_session {
        cmd.arg("--no-session-persistence");
    }
    match (&spec.resume, &spec.session_id) {
        (Some(id), _) => {
            cmd.args(["--resume", id]);
        }
        (None, Some(id)) => {
            cmd.args(["--session-id", id]);
        }
        _ => {}
    }
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
    let mut model: Option<String> = None;
    let mut session_id: Option<String> = None;
    while let Some(line) = lines.next_line().await? {
        log_file.write_all(line.as_bytes()).await?;
        log_file.write_all(b"\n").await?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                model = v["model"].as_str().map(String::from);
                session_id = v["session_id"].as_str().map(String::from);
            }
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
    let usage = &res["usage"];
    let error = res["is_error"]
        .as_bool()
        .unwrap_or(false)
        .then(|| format!("{} agent failed ({}): {}", spec.role, res["subtype"], res["result"]));
    if error.is_none() {
        tracing::info!(role = spec.role, cost_usd = format!("{cost:.3}"), "agent done");
    }
    Ok(AgentResult {
        text: res["result"].as_str().unwrap_or_default().to_string(),
        structured: res.get("structured_output").filter(|v| !v.is_null()).cloned(),
        cost_usd: cost,
        model: model.or(spec.model),
        session_id: res["session_id"].as_str().map(String::from).or(session_id),
        duration_ms: res["duration_ms"].as_i64(),
        num_turns: res["num_turns"].as_i64(),
        input_tokens: usage["input_tokens"].as_i64(),
        output_tokens: usage["output_tokens"].as_i64(),
        cache_read_tokens: usage["cache_read_input_tokens"].as_i64(),
        cache_write_tokens: usage["cache_creation_input_tokens"].as_i64(),
        error,
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

/// Random UUID v4 for `--session-id` (no uuid crate needed for one call per agent).
pub fn new_session_id() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_id_is_uuid_v4() {
        let id = super::new_session_id().unwrap();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
        assert_ne!(id, super::new_session_id().unwrap());
    }
}
