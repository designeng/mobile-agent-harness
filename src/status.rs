//! `harness status`: derive run state from the artifacts in `.harness/runs/`.
//! No separate state file: the files the pipeline already writes are the source of truth.

use crate::config::Config;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Runs without a pid file (made before it existed) count as live if touched this recently.
const LEGACY_LIVE_WINDOW: Duration = Duration::from_secs(30 * 60);

struct RunInfo {
    name: String,
    state: State,
    stage: String,
    iter: u32,
    cost: f64,
    last_activity: Option<SystemTime>,
    verdict: String,
    head: String,
}

#[derive(PartialEq, Clone, Copy)]
enum State {
    Running,
    Done,
    Failed,
    Crashed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Running => "RUNNING",
            State::Done => "DONE",
            State::Failed => "FAILED",
            State::Crashed => "CRASHED",
        }
    }
}

pub fn show(cfg: &Config, run: Option<&str>, last: usize, json: bool) -> Result<()> {
    let runs_dir = cfg.state_dir().join("runs");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&runs_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort(); // names start with a timestamp
    if let Some(r) = run {
        dirs.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(r)));
    }
    let skip = dirs.len().saturating_sub(last);
    let infos: Vec<RunInfo> = dirs.iter().skip(skip).map(|d| inspect(d)).collect();

    if infos.is_empty() {
        println!("no runs in {}", runs_dir.display());
        return Ok(());
    }
    if json {
        let v: Vec<Value> = infos.iter().map(|i| serde_json::json!({
            "run": i.name, "state": i.state.label().to_lowercase(), "stage": i.stage,
            "iteration": i.iter, "cost_usd": i.cost, "verdict": i.verdict,
            "idle_secs": i.last_activity.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for i in &infos {
        let idle = i.last_activity.and_then(|t| t.elapsed().ok()).map(fmt_dur).unwrap_or_else(|| "?".into());
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

fn inspect(dir: &Path) -> RunInfo {
    let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut files: Vec<(String, SystemTime)> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.metadata().ok()?.modified().ok()?))).collect())
        .unwrap_or_default();
    files.sort_by_key(|(_, t)| *t);
    let last_activity = files.last().map(|(_, t)| *t);

    let iter = files.iter().filter_map(|(n, _)| n.strip_prefix("developer-")?.strip_suffix(".jsonl")?.parse::<u32>().ok()).max().unwrap_or(0);
    let cost: f64 = files.iter().filter(|(n, _)| n.ends_with(".jsonl")).map(|(n, _)| transcript_cost(&dir.join(n))).sum();
    let stage = files.iter().rev().find_map(|(n, _)| stage_of(n)).unwrap_or_else(|| "starting".into());

    let summary = std::fs::read_to_string(dir.join("summary.md")).ok();
    let head = summary.as_deref().and_then(|s| s.lines().next()).unwrap_or("").to_string();
    let live = match std::fs::read_to_string(dir.join("run.pid")).ok().and_then(|p| p.trim().parse::<u32>().ok()) {
        Some(pid) => pid_alive(pid),
        None => last_activity.and_then(|t| t.elapsed().ok()).is_some_and(|d| d < LEGACY_LIVE_WINDOW),
    };
    let state = match &summary {
        Some(s) if s.starts_with("FAILED") => State::Failed,
        Some(_) => State::Done,
        None if live => State::Running,
        None => State::Crashed,
    };
    let stage = if state == State::Crashed { format!("{stage} (process gone, see .harness/run-*.log)") } else { stage };

    // Latest QA verdict / review decision.
    let latest = |prefix: &str| files.iter().rev().find(|(n, _)| n.starts_with(prefix) && n.ends_with(".json")).map(|(n, _)| n.clone());
    let read = |f: Option<String>| f.and_then(|f| Some((f.clone(), serde_json::from_str::<Value>(&std::fs::read_to_string(dir.join(&f)).ok()?).ok()?)));
    let mut verdict = Vec::new();
    if let Some((_, q)) = read(latest("qa")) {
        let bugs = q["bugs"].as_array().map_or(0, |b| b.len());
        verdict.push(format!("QA: {} ({bugs} bugs)", q["verdict"].as_str().unwrap_or("?")));
    }
    if let Some((_, r)) = read(latest("review")) {
        let n = r["blocking_comments"].as_array().map_or(0, |b| b.len());
        verdict.push(format!("review: {} ({n} blocking)", if r["approve"] == true { "approved" } else { "changes requested" }));
    }
    RunInfo { name, state, stage, iter, cost, last_activity, verdict: verdict.join(" | "), head }
}

fn stage_of(file: &str) -> Option<String> {
    let num = |p: &str, s: &str| file.strip_prefix(p)?.strip_suffix(s)?.parse::<u32>().ok();
    Some(match file {
        "summary.md" => "finished".into(),
        "planner.jsonl" => "planning".into(),
        "plan.md" => "planned".into(),
        "qa.jsonl" => "exploring on device".into(),
        "qa.json" => "finished".into(),
        _ => {
            if let Some(n) = num("developer-", ".jsonl") { format!("developing (#{n})") }
            else if let Some(n) = num("developer-", ".md") { format!("developer done, gate (#{n})") }
            else if let Some(n) = num("build-", ".log") { format!("build gate (#{n})") }
            else if let Some(n) = num("unit-", ".log") { format!("unit tests (#{n})") }
            else if let Some(n) = num("qa-", ".jsonl") { format!("device QA (#{n})") }
            else if let Some(n) = num("qa-", ".json") { format!("QA done (#{n})") }
            else if let Some(n) = num("review-", ".jsonl") { format!("code review (#{n})") }
            else if let Some(n) = num("review-", ".json") { format!("review done (#{n})") }
            else { return None }
        }
    })
}

/// Cost from the transcript's final `result` event (cheap check first to skip big lines).
fn transcript_cost(path: &Path) -> f64 {
    let Ok(text) = std::fs::read_to_string(path) else { return 0.0 };
    text.lines()
        .rev()
        .filter(|l| l.contains("\"total_cost_usd\""))
        .find_map(|l| {
            let v = serde_json::from_str::<Value>(l).ok()?;
            (v["type"] == "result").then(|| v["total_cost_usd"].as_f64()).flatten()
        })
        .unwrap_or(0.0)
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 { format!("{s}s") } else if s < 5400 { format!("{}m", s / 60) } else { format!("{}h", s / 3600) }
}
