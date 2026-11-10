use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    /// Last `n` bytes of combined output, for feeding back to agents.
    pub fn tail(&self, n: usize) -> String {
        let all = format!("{}\n{}", self.stdout, self.stderr);
        let all = all.trim();
        if all.len() <= n {
            return all.to_string();
        }
        let mut start = all.len() - n;
        while !all.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &all[start..])
    }
}

/// Run a shell command line via `sh -c`.
pub async fn sh(cmdline: &str, cwd: &Path, timeout: Duration) -> Result<Out> {
    exec("sh", &["-c", cmdline], cwd, timeout).await
}

pub async fn exec(program: &str, args: &[&str], cwd: &Path, timeout: Duration) -> Result<Out> {
    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("MAESTRO_CLI_NO_ANALYTICS", "1")
        .env("MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("{program} timed out after {timeout:?}"))??;
    Ok(Out {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Like `exec`, but fails on non-zero exit and returns trimmed stdout.
pub async fn check(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let out = exec(program, args, cwd, Duration::from_secs(300)).await?;
    anyhow::ensure!(out.ok, "{program} {} failed: {}", args.join(" "), out.tail(2000));
    Ok(out.stdout.trim().to_string())
}
