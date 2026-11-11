//! `harness status`: run state from `.harness/harness.db`.

use crate::config::Config;
use crate::db::{Db, RunRow};
use anyhow::Result;
use std::time::{Duration, SystemTime};

#[derive(PartialEq, Clone, Copy)]
enum State {
    Running,
    Done,
    Failed,
    /// Stopped by an environment problem; resumable.
    Interrupted,
    Crashed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Running => "RUNNING",
            State::Done => "DONE",
            State::Failed => "FAILED",
            State::Interrupted => "PAUSED",
            State::Crashed => "CRASHED",
        }
    }
}

struct RunInfo {
    name: String,
    state: State,
    stage: String,
    iter: u32,
    cost: f64,
    idle: Option<Duration>,
    verdict: String,
    head: String,
}

pub fn show(cfg: &Config, run: Option<&str>, last: usize, json: bool) -> Result<()> {
    let db_path = cfg.state_dir().join("harness.db");
    if !db_path.exists() {
        println!("no runs yet ({} does not exist)", db_path.display());
        return Ok(());
    }
    let db = Db::open(&cfg.state_dir())?;
    let infos: Vec<RunInfo> = db.recent_runs(run, last)?.into_iter().map(info).collect();

    if infos.is_empty() {
        println!("no runs{}", run.map(|r| format!(" matching '{r}'")).unwrap_or_default());
        return Ok(());
    }
    if json {
        let v: Vec<serde_json::Value> = infos
            .iter()
            .map(|i| {
                serde_json::json!({
                    "run": i.name, "state": i.state.label().to_lowercase(), "stage": i.stage,
                    "iteration": i.iter, "cost_usd": i.cost, "verdict": i.verdict,
                    "idle_secs": i.idle.map(|d| d.as_secs()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for i in &infos {
        let idle = i.idle.map(fmt_dur).unwrap_or_else(|| "?".into());
        println!("{:<8} {}", i.state.label(), i.name);
        println!("         stage: {} | iteration: {} | cost: ${:.2} | last activity: {idle} ago", i.stage, i.iter, i.cost);
        if !i.verdict.is_empty() {
            println!("         {}", i.verdict);
        }
        if !i.head.is_empty() {
            println!("         {}", i.head);
        }
    }
    Ok(())
}

fn info(r: RunRow) -> RunInfo {
    let state = match r.status.as_str() {
        "done" => State::Done,
        "failed" => State::Failed,
        "interrupted" => State::Interrupted,
        _ if crate::shell::pid_alive(r.pid) => State::Running,
        _ => State::Crashed,
    };
    let mut stage = match (&r.stage, state) {
        (_, State::Done) => "finished".to_string(),
        (None, _) => "starting".to_string(),
        (Some((name, iter, finished)), _) => {
            let label = stage_label(name);
            let n = if *iter > 0 { format!(" (#{iter})") } else { String::new() };
            if *finished { format!("{label} done{n}") } else { format!("{label}{n}") }
        }
    };
    let issue = r.name.rsplit_once("-issue-").map(|(_, n)| n.to_string());
    match (state, &issue) {
        (State::Crashed | State::Interrupted, Some(n)) => {
            let why = if state == State::Crashed { "process gone" } else { "environment problem" };
            stage.push_str(&format!(" ({why}; continue with `harness run {n}` or `harness queue`)"));
        }
        (State::Crashed, None) => stage.push_str(" (process gone, see .harness/run-*.log)"),
        _ => {}
    }

    // A live agent writes to its transcript long before the DB hears about it.
    let db_time = r.last_activity.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(SystemTime::from);
    let file_time = r.live_transcript.as_deref().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
    let idle = db_time.max(file_time).and_then(|t| t.elapsed().ok());

    let mut verdict = Vec::new();
    if let Some((v, bugs)) = &r.qa {
        verdict.push(format!("QA: {v} ({bugs} bugs)"));
    }
    if let Some((approve, n)) = r.review {
        verdict.push(format!("review: {} ({n} blocking)", if approve { "approved" } else { "changes requested" }));
    }
    let head = match state {
        State::Failed | State::Interrupted => r.error.as_deref().and_then(|e| e.lines().next()).unwrap_or("").to_string(),
        _ => match (&r.pr_url, r.summary.as_deref().and_then(|s| s.lines().next())) {
            (Some(url), Some(h)) => format!("{h} | {url}"),
            (_, h) => h.unwrap_or("").to_string(),
        },
    };
    RunInfo { name: r.name, state, stage, iter: r.iter, cost: r.cost, idle, verdict: verdict.join(" | "), head }
}

fn stage_label(name: &str) -> &str {
    match name {
        "planner" => "planning",
        "developer" => "developing",
        "build" => "build gate",
        "unit" => "unit tests",
        "apk" => "apk check",
        "install" => "installing on device",
        "regression" => "regression flows",
        "qa" => "device QA",
        "reviewer" => "code review",
        "commit" => "commit",
        "pr" => "opening PR",
        other => other,
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 { format!("{s}s") } else if s < 5400 { format!("{}m", s / 60) } else { format!("{}h", s / 3600) }
}
