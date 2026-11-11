//! `harness queue`: picks open issues labelled ready and runs them one by one.
//!
//! Filtering (labels, dependencies) is deterministic; the LLM only orders the
//! remaining candidates and skips ones agents can't finish. The device is single,
//! so issues run strictly in sequence; the pick is redone after every issue.

use crate::agent::AgentSpec;
use crate::config::Config;
use crate::db::Db;
use crate::github::{self, ListedIssue};
use crate::pipeline::{self, EnvError, Run};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt::Write as _;

const PICKER: &str = include_str!("../prompts/picker.md");

const IN_PROGRESS: &str = "agent:in-progress";
const DONE: &str = "agent:done";
const FAILED: &str = "agent:failed";
/// Issues with any of these are never picked automatically.
const EXCLUDED: [&str; 4] = [IN_PROGRESS, DONE, FAILED, "blocked"];
/// Issue body excerpt sent to the picker.
const BODY_CHARS: usize = 1500;

pub struct QueueOpts {
    pub max_issues: Option<u32>,
    pub budget_usd: Option<f64>,
    pub pr: bool,
    /// Only show the pick, run nothing.
    pub dry_run: bool,
}

struct Pick {
    order: Vec<(u64, String)>,
    skip: Vec<(u64, String)>,
}

pub async fn run(cfg: Config, opts: QueueOpts) -> Result<()> {
    let repo = cfg.github.repo.clone();
    let budget = opts.budget_usd.or(cfg.queue.budget_usd);
    let db = Db::open(&cfg.state_dir())?;
    let since = db.cost_marker()?;
    if !opts.dry_run {
        for (name, color, desc) in [
            (IN_PROGRESS, "fbca04", "Harness agents are working on this"),
            (DONE, "0e8a16", "Harness opened a PR for this"),
            (FAILED, "d93f0b", "Harness could not finish this; see the comment"),
        ] {
            github::ensure_label(&repo, name, color, desc).await?;
        }
    }

    let mut skipped: HashSet<u64> = HashSet::new();
    let mut report: Vec<String> = vec![];
    let mut started = 0u32;
    let mut fails_in_row = 0u32;
    let stop = loop {
        if opts.max_issues.is_some_and(|m| started >= m) {
            break format!("reached --max-issues {started}");
        }
        let spent = db.cost_since(since)?;
        if let Some(b) = budget.filter(|b| spent >= *b) {
            break format!("budget ${b:.2} spent (${spent:.2})");
        }

        let all = github::list_open_issues(&repo).await?;
        // Stopped runs (crash, reboot, environment problem) go first and continue where they
        // stopped; a crashed one may still carry the in-progress label, so bypass the filter.
        let open: HashSet<u64> = all.iter().filter(|i| !i.has_label(DONE) && !i.has_label(FAILED)).map(|i| i.number).collect();
        let resumable = db.resumable_issues()?.into_iter().find(|n| open.contains(n) && !skipped.contains(n));
        if let Some(n) = resumable.filter(|_| !opts.dry_run) {
            started += 1;
            match run_one(&cfg, &db, n, opts.pr, "resuming a stopped run", &mut report, &mut fails_in_row).await? {
                Some(stop) => break stop,
                None => continue,
            }
        }
        if let Some(n) = resumable.filter(|_| opts.dry_run) {
            println!("Resume first: #{n} (stopped run)");
        }
        let cands = candidates(&all, &cfg.queue.label, &skipped);
        if cands.is_empty() {
            break "no eligible issues left".into();
        }
        let pick = if cands.len() == 1 {
            Pick { order: vec![(cands[0].number, "only candidate".into())], skip: vec![] }
        } else {
            pick(&cfg, &cands).await?
        };
        for (n, why) in &pick.skip {
            tracing::info!(issue = n, "picker skipped: {why}");
            report.push(format!("#{n} skipped by picker: {why}"));
            skipped.insert(*n);
        }
        if opts.dry_run {
            println!("Run order:");
            for (n, why) in &pick.order {
                println!("  #{n} — {why}");
            }
            if !pick.skip.is_empty() {
                println!("Skipped:");
                for (n, why) in &pick.skip {
                    println!("  #{n} — {why}");
                }
            }
            println!("pick cost: ${:.3}", db.cost_since(since)?);
            return Ok(());
        }
        let Some((next, why)) = pick.order.first().cloned() else {
            break "picker skipped every candidate".into();
        };

        started += 1;
        if let Some(stop) = run_one(&cfg, &db, next, opts.pr, &why, &mut report, &mut fails_in_row).await? {
            break stop;
        }
    };

    println!("\nQueue stopped: {stop}");
    for line in &report {
        println!("  {line}");
    }
    println!("total cost: ${:.2}", db.cost_since(since)?);
    Ok(())
}

/// Runs (or resumes) one issue and labels it by the outcome. `Some(reason)` = stop the queue.
async fn run_one(cfg: &Config, db: &Db, next: u64, pr: bool, why: &str, report: &mut Vec<String>, fails_in_row: &mut u32) -> Result<Option<String>> {
    let repo = &cfg.github.repo;
    tracing::info!(issue = next, "queue: next issue — {why}");
    github::edit_labels(repo, next, &[IN_PROGRESS], &[]).await?;
    let before = db.cost_marker()?;
    let res = pipeline::run_issue(cfg.clone(), next, pr, false).await;
    let cost = db.cost_since(before)?;
    match res {
        Ok(()) => {
            *fails_in_row = 0;
            github::edit_labels(repo, next, &[DONE], &[IN_PROGRESS]).await?;
            report.push(format!("#{next} done (${cost:.2})"));
        }
        Err(e) if e.downcast_ref::<EnvError>().is_some() => {
            // Not the issue's fault: the run stays resumable; give the issue back and stop.
            github::edit_labels(repo, next, &[], &[IN_PROGRESS]).await?;
            report.push(format!("#{next} paused by environment (${cost:.2}); the next queue/run continues it"));
            return Ok(Some(format!("environment problem: {e:#}")));
        }
        Err(e) => {
            *fails_in_row += 1;
            github::edit_labels(repo, next, &[FAILED], &[IN_PROGRESS]).await?;
            let msg = format!(
                "Harness could not finish this issue (cost ${cost:.2}).\n\n```\n{}\n```\n\n\
                 Clarify the issue and remove the `{FAILED}` label to let the queue retry it.",
                truncate(&format!("{e:#}"), 3000)
            );
            if let Err(ce) = github::comment(repo, next, &msg).await {
                tracing::warn!("commenting on #{next}: {ce:#}");
            }
            report.push(format!("#{next} failed (${cost:.2})"));
            if *fails_in_row >= cfg.queue.max_consecutive_failures {
                return Ok(Some(format!("{fails_in_row} failures in a row")));
            }
        }
    }
    Ok(None)
}

/// Eligible issues: ready label, no status/blocked label, no open dependencies, not skipped earlier.
fn candidates<'a>(all: &'a [ListedIssue], ready: &str, skipped: &HashSet<u64>) -> Vec<&'a ListedIssue> {
    let open: HashSet<u64> = all.iter().map(|i| i.number).collect();
    let mut c: Vec<&ListedIssue> = all
        .iter()
        .filter(|i| i.has_label(ready))
        .filter(|i| !EXCLUDED.iter().any(|l| i.has_label(l)))
        .filter(|i| !skipped.contains(&i.number))
        .filter(|i| dependencies(&i.body).iter().all(|d| !open.contains(d) || *d == i.number))
        .collect();
    c.sort_by_key(|i| i.number);
    c
}

/// Issue numbers after "depends on" / "blocked by" (same line), e.g. "Depends on #3, #5".
fn dependencies(body: &str) -> Vec<u64> {
    let mut out = vec![];
    for line in body.to_lowercase().lines() {
        for key in ["depends on", "blocked by"] {
            let Some(i) = line.find(key) else { continue };
            for part in line[i + key.len()..].split('#').skip(1) {
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse() {
                    out.push(n);
                }
            }
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn pick_schema() -> Value {
    let item = json!({"type": "object", "properties": {
        "number": {"type": "integer"}, "reason": {"type": "string"}
    }, "required": ["number", "reason"]});
    json!({
        "type": "object",
        "properties": {"order": {"type": "array", "items": item}, "skip": {"type": "array", "items": item}},
        "required": ["order", "skip"]
    })
}

/// One light-model call that orders the candidates; recorded as a `pick` run (cost shows in status).
async fn pick(cfg: &Config, cands: &[&ListedIssue]) -> Result<Pick> {
    let mut list = String::new();
    for i in cands {
        let labels: Vec<&str> = i.labels.iter().map(|l| l.name.as_str()).collect();
        let _ = write!(list, "\n## #{} {}\nlabels: {}\n\n{}\n", i.number, i.title, labels.join(", "), truncate(i.body.trim(), BODY_CHARS));
    }
    let mut run = Run::start(cfg.clone(), "pick", None, Some("queue: pick next issue"))?;
    let res = async {
        let mut spec = AgentSpec::new("picker", PICKER, format!("# Candidate issues\n{list}"), &cfg.app.repo);
        spec.model = cfg.light_model().map(String::from);
        spec.disallowed_tools = ["Bash", "Edit", "Write", "Read", "Glob", "Grep", "WebFetch", "WebSearch", "Task"].map(String::from).to_vec();
        spec.json_schema = Some(pick_schema());
        let v = run.agent(spec, "picker.jsonl").await?.structured.context("picker returned nothing")?;
        let known: HashSet<u64> = cands.iter().map(|i| i.number).collect();
        let parse = |k: &str| -> Vec<(u64, String)> {
            let mut seen = HashSet::new();
            v[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| Some((x["number"].as_u64()?, x["reason"].as_str().unwrap_or("").to_string())))
                .filter(|(n, _)| known.contains(n) && seen.insert(*n))
                .collect()
        };
        let order = parse("order");
        let in_order: HashSet<u64> = order.iter().map(|(n, _)| *n).collect();
        let skip = parse("skip").into_iter().filter(|(n, _)| !in_order.contains(n)).collect();
        let pick = Pick { order, skip };
        let summary = pick.order.iter().map(|(n, w)| format!("#{n} {w}")).collect::<Vec<_>>().join("\n");
        run.set_summary(&format!("order:\n{summary}"))?;
        Ok(pick)
    }
    .await;
    run.finish(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Label;

    fn issue(number: u64, body: &str, labels: &[&str]) -> ListedIssue {
        ListedIssue {
            number,
            title: format!("t{number}"),
            body: body.into(),
            labels: labels.iter().map(|l| Label { name: (*l).into() }).collect(),
        }
    }

    #[test]
    fn parses_dependencies() {
        assert_eq!(dependencies("Depends on #3, #5\nsee #9"), vec![3, 5]);
        assert_eq!(dependencies("text\nBlocked by #12."), vec![12]);
        assert!(dependencies("relates to #4").is_empty());
    }

    #[test]
    fn filters_candidates() {
        let all = vec![
            issue(1, "", &["agent-ready"]),
            issue(2, "depends on #1", &["agent-ready"]),
            issue(3, "depends on #99", &["agent-ready"]), // #99 closed
            issue(4, "", &["agent-ready", "agent:failed"]),
            issue(5, "", &[]),
            issue(6, "", &["agent-ready"]),
        ];
        let skipped = HashSet::from([6]);
        let got: Vec<u64> = candidates(&all, "agent-ready", &skipped).iter().map(|i| i.number).collect();
        assert_eq!(got, vec![1, 3]);
    }
}
