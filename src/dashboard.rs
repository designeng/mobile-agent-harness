//! `harness dashboard`: local web board of issues by harness state (Vue SPA + JSON API).
//!
//! Data comes from `.harness/harness.db` and, for issues not run yet, from open GitHub
//! issues (cached briefly, since every poll would otherwise call `gh`).

use crate::config::Config;
use crate::db::{Db, RunRow};
use crate::github::{self, ListedIssue};
use crate::queue;
use crate::status::{self, State};
use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, Query, State as AxState};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;

const INDEX: &str = include_str!("../dashboard/index.html");
const APP_JS: &str = include_str!("../dashboard/app.js");
const STYLE: &str = include_str!("../dashboard/style.css");

/// How long a `gh issue list` result is reused.
const GH_TTL: Duration = Duration::from_secs(30);
/// Runs considered for the board.
const MAX_RUNS: usize = 1000;
/// Tail of a log/transcript returned by the file endpoint.
const FILE_TAIL: u64 = 64 * 1024;

type Issues = Result<Arc<Vec<ListedIssue>>, String>;

struct App {
    cfg: Config,
    /// Last `gh issue list` result and when it was fetched.
    gh: Mutex<Option<(Instant, Issues)>>,
}

type Shared = Arc<App>;

pub async fn serve(cfg: Config, port: u16) -> Result<()> {
    let app = Arc::new(App { cfg, gh: Mutex::new(None) });
    let router = Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", INDEX) }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }))
        .route("/style.css", get(|| async { asset("text/css; charset=utf-8", STYLE) }))
        .route("/api/board", get(board))
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/file", get(run_file))
        .with_state(app);
    // Local only: the API exposes issue text, plans and logs.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    println!("dashboard: http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn asset(ctype: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{:#}", self.0)}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

/// Runs a DB read off the async runtime (rusqlite blocks; `pid_alive` spawns `kill`).
async fn with_db<T: Send + 'static>(app: &Shared, f: impl FnOnce(&Db) -> Result<T> + Send + 'static) -> Result<T> {
    let dir = app.cfg.state_dir();
    tokio::task::spawn_blocking(move || {
        if !dir.join("harness.db").exists() {
            anyhow::bail!("no runs yet ({} does not exist)", dir.join("harness.db").display());
        }
        f(&Db::open(&dir)?)
    })
    .await?
}

async fn open_issues(app: &Shared) -> Issues {
    let mut cache = app.gh.lock().await;
    if let Some((_, res)) = cache.as_ref().filter(|(at, _)| at.elapsed() < GH_TTL) {
        return res.clone();
    }
    let res = github::list_open_issues(&app.cfg.github.repo).await.map(Arc::new).map_err(|e| format!("{e:#}"));
    *cache = Some((Instant::now(), res.clone()));
    res
}

async fn board(AxState(app): AxState<Shared>) -> Result<Json<Value>, ApiError> {
    let runs = match with_db(&app, |db| Ok(db.recent_runs(None, MAX_RUNS)?.into_iter().map(RunView::from).collect::<Vec<_>>())).await {
        Ok(r) => r,
        Err(e) if e.to_string().starts_with("no runs yet") => vec![],
        Err(e) => return Err(e.into()),
    };
    let issues = open_issues(&app).await;
    let cfg = &app.cfg;
    let tasks = build_tasks(&runs, issues.as_deref().map(|v| v.as_slice()).ok(), &cfg.queue.label, &cfg.github.repo);

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let total: f64 = runs.iter().map(|r| r.row.cost).sum();
    let today_cost: f64 = runs.iter().filter(|r| r.row.started_at.starts_with(&today)).map(|r| r.row.cost).sum();
    let active = runs.iter().rev().find(|r| r.state == State::Running).map(|r| r.row.name.clone());
    Ok(Json(json!({
        "repo": cfg.github.repo,
        "ready_label": cfg.queue.label,
        "budget_usd": cfg.queue.budget_usd,
        "github_error": issues.err(),
        "active_run": active,
        "cost": {"total": total, "today": today_cost},
        "tasks": tasks,
        "generated_at": chrono::Utc::now().to_rfc3339(),
    })))
}

struct RunView {
    row: RunRow,
    state: State,
    idle_secs: Option<u64>,
}

impl From<RunRow> for RunView {
    fn from(row: RunRow) -> Self {
        let state = status::run_state(&row.status, row.pid);
        // A live agent writes its transcript long before the DB hears about it.
        let db_time = row.last_activity.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(SystemTime::from);
        let file_time = row.live_transcript.as_deref().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
        let idle_secs = db_time.max(file_time).and_then(|t| t.elapsed().ok()).map(|d| d.as_secs());
        RunView { row, state, idle_secs }
    }
}

impl RunView {
    fn json(&self) -> Value {
        let r = &self.row;
        let first = |s: &Option<String>| s.as_deref().and_then(|s| s.lines().find(|l| !l.trim().is_empty())).map(String::from);
        json!({
            "id": r.id,
            "name": r.name,
            "kind": r.kind,
            "state": self.state.label().to_lowercase(),
            "stage": r.stage.as_ref().map(|(name, iter, finished)| json!({
                "name": name, "label": status::stage_label(name), "iter": iter, "finished": finished,
            })),
            "iter": r.iter,
            "cost": r.cost,
            "complexity": r.complexity,
            "qa": r.qa.as_ref().map(|(v, bugs)| json!({"verdict": v, "bugs": bugs})),
            "review": r.review.map(|(approve, n)| json!({"approve": approve, "blocking": n})),
            "pr_url": r.pr_url,
            "error": first(&r.error),
            "summary": first(&r.summary),
            "started_at": r.started_at,
            "finished_at": r.finished_at,
            "idle_secs": self.idle_secs,
        })
    }
}

/// Board columns, left to right.
const COLUMNS: [&str; 7] = ["backlog", "blocked", "ready", "running", "paused", "failed", "done"];

/// One task per issue (open on GitHub or run by the harness) plus one per explore run.
fn build_tasks(runs: &[RunView], issues: Option<&[ListedIssue]>, ready: &str, repo: &str) -> Vec<Value> {
    // Issue runs grouped by issue; `runs` is oldest first, so the last one is the newest.
    let mut by_issue: BTreeMap<u64, Vec<&RunView>> = BTreeMap::new();
    for r in runs.iter().filter(|r| r.row.kind == "issue") {
        if let Some(n) = r.row.issue {
            by_issue.entry(n).or_default().push(r);
        }
    }
    let open: HashSet<u64> = issues.unwrap_or_default().iter().map(|i| i.number).collect();
    let mut tasks = vec![];

    for issue in issues.unwrap_or_default() {
        let runs = by_issue.remove(&issue.number).unwrap_or_default();
        let deps: Vec<u64> = queue::dependencies(&issue.body).into_iter().filter(|d| open.contains(d) && *d != issue.number).collect();
        let column = issue_column(runs.last().copied(), Some(issue), &deps, ready);
        tasks.push(task(issue.number, &issue.title, column, &runs, Some(issue), &deps, repo, true));
    }
    // Issues known only from runs: closed on GitHub, or GitHub unreachable.
    for (n, runs) in by_issue {
        let last = runs.last().copied().expect("non-empty group");
        let title = last.row.title.clone().unwrap_or_else(|| format!("Issue #{n}"));
        tasks.push(task(n, &title, issue_column(Some(last), None, &[], ready), &runs, None, &[], repo, issues.is_none()));
    }
    for r in runs.iter().filter(|r| r.row.kind == "explore") {
        tasks.push(json!({
            "key": format!("run-{}", r.row.id),
            "kind": "explore",
            "title": r.row.title.clone().unwrap_or_else(|| "Exploratory QA".into()),
            "column": run_column(r.state),
            "run": r.json(),
            "run_ids": [r.row.id],
            "labels": [],
            "open": true,
        }));
    }
    tasks.sort_by_key(|t| COLUMNS.iter().position(|c| t["column"] == *c).unwrap_or(0));
    tasks
}

fn run_column(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Interrupted | State::Crashed => "paused",
        State::Failed => "failed",
        State::Done => "done",
    }
}

/// Where an issue stands, following the queue's rules: live and stopped runs first, then
/// status labels, then eligibility (a failed issue relabelled ready is up for a retry).
fn issue_column(last: Option<&RunView>, issue: Option<&ListedIssue>, open_deps: &[u64], ready: &str) -> &'static str {
    let has = |l: &str| issue.is_some_and(|i| i.has_label(l));
    if let Some(r) = last.filter(|r| matches!(r.state, State::Running | State::Interrupted | State::Crashed)) {
        return run_column(r.state);
    }
    if has(queue::FAILED) {
        return "failed";
    }
    if has(queue::DONE) || last.is_some_and(|r| r.state == State::Done) {
        return "done";
    }
    if issue.is_none() {
        return last.map(|r| run_column(r.state)).unwrap_or("backlog");
    }
    if has(ready) {
        if has("blocked") || has(queue::IN_PROGRESS) || !open_deps.is_empty() {
            return "blocked";
        }
        return "ready";
    }
    if last.is_some_and(|r| r.state == State::Failed) {
        return "failed";
    }
    "backlog"
}

#[allow(clippy::too_many_arguments)]
fn task(n: u64, title: &str, column: &str, runs: &[&RunView], issue: Option<&ListedIssue>, deps: &[u64], repo: &str, open: bool) -> Value {
    json!({
        "key": format!("issue-{n}"),
        "kind": "issue",
        "number": n,
        "title": title,
        "url": format!("https://github.com/{repo}/issues/{n}"),
        "open": open,
        "column": column,
        "labels": issue.map(|i| i.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "open_deps": deps,
        "run": runs.last().map(|r| r.json()),
        "run_ids": runs.iter().rev().map(|r| r.row.id).collect::<Vec<_>>(),
        "total_cost": runs.iter().fold(0.0, |a, r| a + r.row.cost),
    })
}

async fn run_detail(AxState(app): AxState<Shared>, UrlPath(id): UrlPath<i64>) -> Result<Response, ApiError> {
    Ok(match with_db(&app, move |db| db.run_detail(id)).await? {
        Some(v) => Json(v).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": format!("no run {id}")}))).into_response(),
    })
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// Tail of a log or transcript; only files the DB records for this run can be read.
async fn run_file(AxState(app): AxState<Shared>, UrlPath(id): UrlPath<i64>, Query(q): Query<FileQuery>) -> Result<Response, ApiError> {
    let detail = with_db(&app, move |db| db.run_detail(id)).await?;
    let known = detail.as_ref().is_some_and(|d| {
        let paths = |k: &str, f: &str| d[k].as_array().into_iter().flatten().filter_map(|x| x[f].as_str()).any(|p| p == q.path);
        paths("stages", "log_path") || paths("agent_calls", "transcript")
    });
    if !known {
        return Ok((StatusCode::NOT_FOUND, Json(json!({"error": "not a file of this run"}))).into_response());
    }
    let text = tokio::task::spawn_blocking(move || tail(std::path::Path::new(&q.path))).await??;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

fn tail(path: &std::path::Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(FILE_TAIL);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 { format!("… (last {} KB of {} KB)\n{}", FILE_TAIL / 1024, len / 1024, text) } else { text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Label;

    fn view(status: &str, pid: u32) -> RunView {
        let row = RunRow {
            id: 1,
            name: "r".into(),
            kind: "issue".into(),
            issue: Some(1),
            title: None,
            started_at: "2026-11-11T00:00:00.000Z".into(),
            finished_at: None,
            status: status.into(),
            pid,
            summary: None,
            error: None,
            pr_url: None,
            complexity: None,
            cost: 0.0,
            iter: 0,
            stage: None,
            last_activity: None,
            live_transcript: None,
            qa: None,
            review: None,
        };
        RunView { state: status::run_state(&row.status, row.pid), row, idle_secs: None }
    }

    fn issue(labels: &[&str]) -> ListedIssue {
        ListedIssue { number: 1, title: "t".into(), body: String::new(), labels: labels.iter().map(|l| Label { name: (*l).into() }).collect() }
    }

    #[test]
    fn columns_follow_queue_rules() {
        let me = std::process::id();
        let col = |run: Option<&RunView>, labels: &[&str], deps: &[u64]| issue_column(run, Some(&issue(labels)), deps, "agent-ready");
        assert_eq!(col(None, &[], &[]), "backlog");
        assert_eq!(col(None, &["agent-ready"], &[]), "ready");
        assert_eq!(col(None, &["agent-ready"], &[3]), "blocked");
        assert_eq!(col(None, &["agent-ready", "blocked"], &[]), "blocked");
        assert_eq!(col(Some(&view("running", me)), &["agent-ready", "agent:in-progress"], &[]), "running");
        assert_eq!(col(Some(&view("running", u32::MAX - 1)), &["agent-ready"], &[]), "paused");
        assert_eq!(col(Some(&view("interrupted", me)), &["agent-ready"], &[]), "paused");
        assert_eq!(col(Some(&view("failed", me)), &["agent-ready", "agent:failed"], &[]), "failed");
        // Failed label removed: the queue retries it.
        assert_eq!(col(Some(&view("failed", me)), &["agent-ready"], &[]), "ready");
        assert_eq!(col(Some(&view("failed", me)), &[], &[]), "failed");
        assert_eq!(col(Some(&view("done", me)), &["agent-ready"], &[]), "done");
        assert_eq!(issue_column(Some(&view("failed", me)), None, &[], "agent-ready"), "failed");
    }
}
