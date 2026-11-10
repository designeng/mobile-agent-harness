//! Issue → plan → (develop → gate → device QA → review)* → commit/PR.

use crate::agent::{self, AgentSpec};
use crate::config::Config;
use crate::db::{Db, NewRun};
use crate::device::{self, Adb};
use crate::{github, mcp, shell};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PLANNER: &str = include_str!("../prompts/planner.md");
const DEVELOPER: &str = include_str!("../prompts/developer.md");
const QA: &str = include_str!("../prompts/qa.md");
const REVIEWER: &str = include_str!("../prompts/reviewer.md");

/// Device/environment failure: not the issue's fault, so a queue should stop rather than mark it failed.
#[derive(Debug)]
pub struct EnvError(pub String);

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EnvError {}

pub struct Run {
    pub cfg: Config,
    pub dir: PathBuf,
    pub worktree: PathBuf,
    pub cost: f64,
    db: Db,
    id: i64,
    /// Fix-loop iteration recorded on stages; 0 outside the loop.
    iter: u32,
}

impl Run {
    /// Creates the artifacts dir and the `runs` row.
    pub(crate) fn start(cfg: Config, kind: &str, issue: Option<u64>, title: Option<&str>) -> Result<Run> {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = match issue {
            Some(n) => format!("{ts}-issue-{n}"),
            None => format!("{ts}-{kind}"),
        };
        let dir = cfg.state_dir().join("runs").join(&name);
        std::fs::create_dir_all(&dir)?;
        let db = Db::open(&cfg.state_dir())?;
        let worktree = cfg.app.repo.clone();
        let id = db.create_run(&NewRun { name: &name, kind, issue, title, dir: &dir, worktree: &worktree })?;
        tracing::info!(run = %name, "run started");
        Ok(Run { cfg, dir, worktree, cost: 0.0, db, id, iter: 0 })
    }

    /// Records the run's outcome; passes the result through.
    pub(crate) fn finish<T>(&self, res: Result<T>) -> Result<T> {
        let err = res.as_ref().err().map(|e| format!("{e:#}"));
        if let Err(e) = self.db.finish_run(self.id, err.as_deref()) {
            tracing::error!("recording run outcome: {e:#}");
        }
        res
    }

    pub(crate) fn set_summary(&self, summary: &str) -> Result<()> {
        self.db.set_summary(self.id, summary)
    }

    fn stage(&self, name: &str) -> Result<i64> {
        self.db.stage_start(self.id, self.iter, name)
    }

    fn end(&self, stage: i64, ok: bool, log: Option<&Path>, detail: Option<&str>) -> Result<()> {
        self.db.stage_end(stage, ok, log, detail)
    }

    /// Runs an agent as its own stage; the transcript goes to `log` in the run dir.
    pub(crate) async fn agent(&mut self, spec: AgentSpec, log: &str) -> Result<agent::AgentResult> {
        let role = spec.role;
        let path = self.dir.join(log);
        let stage = self.stage(role)?;
        let call = self.db.agent_start(self.id, stage, role, spec.model.as_deref(), &path)?;
        let res = agent::run(spec, &path).await;
        self.db.agent_end(call, &res)?;
        self.end(stage, matches!(&res, Ok(r) if r.error.is_none()), None, None)?;
        let res = res?;
        self.cost += res.cost_usd;
        if let Some(e) = res.error {
            anyhow::bail!(e);
        }
        Ok(res)
    }

    /// MCP config handed to the QA agent: mobile-mcp for UI + our device server.
    fn write_mcp_config(&self, serial: &str) -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let a = &self.cfg.app;
        let mm = &self.cfg.agents.mobile_mcp;
        let cfg = json!({
            "mcpServers": {
                "mobile": { "command": mm[0], "args": &mm[1..] },
                "device": {
                    "command": exe,
                    "args": [
                        "mcp", "--serial", serial,
                        "--adb", self.cfg.device.adb, "--maestro", self.cfg.device.maestro,
                        "--package", a.package,
                        "--apk", self.worktree.join(&a.apk),
                        "--flows-dir", self.worktree.join(&a.flows_dir),
                        "--artifacts", self.dir,
                    ]
                }
            }
        });
        let path = self.dir.join("mcp.json");
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
        Ok(path)
    }
}

/// Permission rules letting an agent write only inside the flows dir (absolute `//` form).
fn flows_write_rules(flows_abs: &Path) -> [String; 2] {
    let p = flows_abs.display();
    [format!("Edit(/{p}/**)"), format!("Write(/{p}/**)")]
}

fn plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "steps": {"type": "array", "items": {"type": "string"}},
            "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
            "qa_scenarios": {"type": "array", "items": {"type": "string"},
                "description": "User-level scenarios to verify on the device"}
        },
        "required": ["summary", "steps", "acceptance_criteria", "qa_scenarios"]
    })
}

fn qa_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "verdict": {"enum": ["pass", "fail", "blocked"],
                "description": "blocked = could not test (device/env problem), not an app bug"},
            "criteria": {"type": "array", "items": {"type": "object", "properties": {
                "criterion": {"type": "string"}, "met": {"type": "boolean"}, "evidence": {"type": "string"}
            }, "required": ["criterion", "met", "evidence"]}},
            "bugs": {"type": "array", "items": {"type": "object", "properties": {
                "title": {"type": "string"},
                "steps": {"type": "array", "items": {"type": "string"}},
                "expected": {"type": "string"}, "actual": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "string"}},
                "logs": {"type": "string"}
            }, "required": ["title", "steps", "expected", "actual"]}},
            "flows_written": {"type": "array", "items": {"type": "string"}},
            "notes": {"type": "string"}
        },
        "required": ["verdict", "criteria", "bugs", "flows_written"]
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "approve": {"type": "boolean"},
            "blocking_comments": {"type": "array", "items": {"type": "string"}},
            "pr_title": {"type": "string"},
            "pr_body": {"type": "string"}
        },
        "required": ["approve", "blocking_comments", "pr_title", "pr_body"]
    })
}

fn render_plan(p: &Value) -> String {
    let list = |k: &str| {
        p[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| format!("- {s}\n")).collect::<String>()).unwrap_or_default()
    };
    format!(
        "## Summary\n{}\n\n## Steps\n{}\n## Acceptance criteria\n{}\n## QA scenarios\n{}",
        p["summary"].as_str().unwrap_or(""),
        list("steps"),
        list("acceptance_criteria"),
        list("qa_scenarios")
    )
}

fn render_qa_feedback(qa: &Value) -> String {
    let mut s = String::from("QA on the real device FAILED.\n\n");
    for c in qa["criteria"].as_array().into_iter().flatten().filter(|c| c["met"] == false) {
        let _ = writeln!(s, "- Criterion not met: {} — {}", c["criterion"].as_str().unwrap_or(""), c["evidence"].as_str().unwrap_or(""));
    }
    for b in qa["bugs"].as_array().into_iter().flatten() {
        let _ = writeln!(s, "\n### Bug: {}\nSteps:", b["title"].as_str().unwrap_or(""));
        for st in b["steps"].as_array().into_iter().flatten() {
            let _ = writeln!(s, "  1. {}", st.as_str().unwrap_or(""));
        }
        let _ = writeln!(s, "Expected: {}\nActual: {}", b["expected"].as_str().unwrap_or(""), b["actual"].as_str().unwrap_or(""));
        if let Some(l) = b["logs"].as_str().filter(|l| !l.is_empty()) {
            let _ = writeln!(s, "Logs:\n```\n{l}\n```");
        }
    }
    if let Some(n) = qa["notes"].as_str() {
        let _ = writeln!(s, "\nNotes: {n}");
    }
    s
}

async fn prepare_worktree(cfg: &Config, issue: u64) -> Result<(PathBuf, String)> {
    let repo = &cfg.app.repo;
    let branch = format!("harness/issue-{issue}");
    let wt = cfg.state_dir().join("worktrees").join(format!("issue-{issue}"));
    if !wt.exists() {
        std::fs::create_dir_all(wt.parent().unwrap())?;
        let _ = shell::check("git", &["fetch", "origin", &cfg.app.base_branch], repo).await;
        let base = match shell::check("git", &["rev-parse", "--verify", &format!("origin/{}", cfg.app.base_branch)], repo).await {
            Ok(_) => format!("origin/{}", cfg.app.base_branch),
            Err(_) => cfg.app.base_branch.clone(),
        };
        shell::check("git", &["worktree", "add", "-B", &branch, &wt.to_string_lossy(), &base], repo).await?;
        // Untracked SDK path is needed for Gradle builds.
        let lp = repo.join("local.properties");
        if lp.exists() {
            std::fs::copy(&lp, wt.join("local.properties"))?;
        }
    }
    Ok((wt, branch))
}

/// Runs a shell gate as a stage, saving its output to `<name>-<iter>.log`.
async fn gate_step(run: &Run, name: &str, cmd: &str) -> Result<shell::Out> {
    tracing::info!("gate: {name}");
    let stage = run.stage(name)?;
    let out = shell::sh(cmd, &run.worktree, Duration::from_secs(1800)).await;
    let log = run.dir.join(format!("{name}-{}.log", run.iter));
    match &out {
        Ok(o) => {
            std::fs::write(&log, format!("{}\n{}", o.stdout, o.stderr))?;
            run.end(stage, o.ok, Some(&log), None)?;
        }
        Err(e) => run.end(stage, false, None, Some(&format!("{e:#}")))?,
    }
    out
}

/// Deterministic gate: build + unit tests. Returns feedback for the developer on failure.
async fn gate(run: &Run) -> Result<Option<String>> {
    let cfg = &run.cfg;
    let b = gate_step(run, "build", &cfg.app.build).await?;
    if !b.ok {
        return Ok(Some(format!("The build FAILED (`{}`):\n```\n{}\n```", cfg.app.build, b.tail(6000))));
    }
    if let Some(t) = &cfg.app.unit_tests {
        let u = gate_step(run, "unit", t).await?;
        if !u.ok {
            return Ok(Some(format!("Unit tests FAILED (`{t}`):\n```\n{}\n```", u.tail(6000))));
        }
    }
    if !run.worktree.join(&cfg.app.apk).exists() {
        let msg = format!("Build succeeded but APK not found at {}", cfg.app.apk.display());
        let stage = run.stage("apk")?;
        run.end(stage, false, None, Some(&msg))?;
        return Ok(Some(msg));
    }
    Ok(None)
}

/// Runs every saved Maestro flow, one stage per flow. Returns failure descriptions.
async fn regression(run: &Run, adb: &Adb) -> Result<Vec<String>> {
    let flows = run.worktree.join(&run.cfg.app.flows_dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&flows)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
        .collect();
    files.sort();
    let mut fails = vec![];
    for f in files {
        tracing::info!("regression: {}", f.display());
        let stage = run.stage("regression")?;
        let res = mcp::run_maestro(adb, &run.cfg.device.maestro, &f, &run.dir).await;
        let flow = f.display().to_string();
        run.end(stage, res.is_ok(), None, Some(&flow))?;
        if let Err(e) = res {
            fails.push(format!("Regression flow {flow} failed:\n{e}"));
        }
    }
    Ok(fails)
}

pub async fn run_issue(cfg: Config, issue_no: u64, open_pr: bool) -> Result<()> {
    let mut run = Run::start(cfg, "issue", Some(issue_no), None)?;
    let res = issue_inner(&mut run, issue_no, open_pr).await;
    run.finish(res)
}

async fn issue_inner(run: &mut Run, issue_no: u64, open_pr: bool) -> Result<()> {
    let issue = github::fetch_issue(&run.cfg.github.repo, issue_no).await?;
    let issue_md = issue.as_markdown();
    run.db.set_issue(run.id, &issue.title, &issue_md)?;
    let (worktree, branch) = prepare_worktree(&run.cfg, issue_no).await?;
    run.db.set_worktree(run.id, &worktree, &branch)?;
    run.worktree = worktree;
    tracing::info!(dir = %run.dir.display(), worktree = %run.worktree.display(), "worktree ready");
    let adb = Adb::resolve(&run.cfg.device.adb, run.cfg.device.serial.as_deref())
        .await
        .map_err(|e| EnvError(format!("device: {e:#}")))?;

    let contract = format!(
        "\n# Build contract (the harness enforces this)\n- Build: `{}` (run from the repo root)\n- APK expected at: `{}`\n- Unit tests: `{}`\n- Must run on a real device WITHOUT a dev server (JS bundle embedded in the APK).\n",
        run.cfg.app.build,
        run.cfg.app.apk.display(),
        run.cfg.app.unit_tests.as_deref().unwrap_or("(none)")
    );

    // 1. Plan
    let mut spec = AgentSpec::new("planner", PLANNER, format!("{issue_md}\n{contract}\nProduce the plan."), &run.worktree);
    spec.model = run.cfg.light_model().map(String::from);
    spec.allowed_tools = vec!["Read".into(), "Glob".into(), "Grep".into()];
    spec.json_schema = Some(plan_schema());
    let plan = run.agent(spec, "planner.jsonl").await?.structured.context("planner returned no plan")?;
    let plan_md = render_plan(&plan);
    run.db.set_plan(run.id, &plan, &plan_md)?;

    let mcp_cfg = run.write_mcp_config(&adb.serial)?;
    let flows_rel = run.cfg.app.flows_dir.display().to_string();
    let mut feedback: Option<String> = None;
    let mut review: Option<Value> = None;

    for iter in 1..=run.cfg.agents.max_fix_iterations {
        run.iter = iter;
        tracing::info!(iter, "iteration");
        // 2. Develop
        let mut prompt = format!("{issue_md}\n\n# Plan\n{plan_md}\n{contract}");
        if let Some(f) = &feedback {
            let _ = write!(prompt, "\n# Feedback from the previous iteration — fix this\n{f}\n");
        }
        let mut spec = AgentSpec::new("developer", DEVELOPER, prompt, &run.worktree);
        spec.model = run.cfg.dev_model().map(String::from);
        spec.allowed_tools = ["Read", "Edit", "Write", "Glob", "Grep", "Bash"].map(String::from).to_vec();
        spec.disallowed_tools = [
            "Bash(git push:*)", "Bash(git commit:*)", "Bash(git reset:*)", "Bash(git checkout:*)",
            "Bash(adb:*)", "Bash(maestro:*)", "Bash(rm -rf:*)", "WebFetch", "WebSearch",
        ]
        .map(String::from)
        .to_vec();
        run.agent(spec, &format!("developer-{iter}.jsonl")).await?;

        // 3. Deterministic gate
        if let Some(f) = gate(run).await? {
            tracing::warn!("gate failed");
            feedback = Some(f);
            continue;
        }

        // 4. Device: install, regression, exploratory QA — under an exclusive lease.
        let qa = {
            let _lease = device::acquire_lease(&adb.serial, &format!("issue-{issue_no}")).await?;
            let stage = run.stage("install")?;
            let apk = run.worktree.join(&run.cfg.app.apk);
            let inst = adb.install_confirmed(&apk, Duration::from_secs(120)).await?;
            run.end(stage, inst.ok, None, (!inst.ok).then(|| inst.tail(3000)).as_deref())?;
            if !inst.ok {
                // The APK was built fine: this is a device/environment problem, not
                // something the developer agent can fix — don't burn an iteration on it.
                return Err(EnvError(format!("APK install failed on the device (environment problem):\n{}", inst.tail(3000))).into());
            }
            let reg_fails = regression(run, &adb).await?;
            let _ = adb.logcat_clear().await;
            let mut prompt = format!(
                "{issue_md}\n\n# Plan\n{plan_md}\n\nThe new build is already installed. \
                 Save Maestro flows into `{flows_rel}/`.\n"
            );
            if !reg_fails.is_empty() {
                let _ = write!(prompt, "\n# Saved regression flows that failed on this build\n{}\n", reg_fails.join("\n\n"));
            }
            let mut spec = AgentSpec::new("qa", QA, prompt, &run.worktree);
            spec.model = run.cfg.dev_model().map(String::from);
            spec.mcp_config = Some(mcp_cfg.clone());
            spec.allowed_tools = [
                "mcp__mobile".to_string(),
                "mcp__device".to_string(),
                "Read".into(), "Glob".into(), "Grep".into(),
            ]
            .to_vec();
            spec.allowed_tools.extend(flows_write_rules(&run.worktree.join(&run.cfg.app.flows_dir)));
            spec.disallowed_tools = vec!["Bash".into(), "WebFetch".into(), "WebSearch".into()];
            spec.json_schema = Some(qa_schema());
            let qa = run.agent(spec, &format!("qa-{iter}.jsonl")).await?.structured.context("qa returned no verdict")?;
            run.db.save_qa(run.id, iter, &qa)?;
            qa
        };

        match qa["verdict"].as_str() {
            Some("pass") => {}
            Some("blocked") => return Err(EnvError(format!("QA blocked (environment problem): {}", qa["notes"])).into()),
            _ => {
                feedback = Some(render_qa_feedback(&qa));
                continue;
            }
        }

        // 5. Review
        let mut spec = AgentSpec::new(
            "reviewer",
            REVIEWER,
            format!("{issue_md}\n\n# Plan\n{plan_md}\n\nReview the uncommitted changes in this worktree (`git status`, `git diff`)."),
            &run.worktree,
        );
        spec.model = run.cfg.light_model().map(String::from);
        spec.allowed_tools = ["Read", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)", "Bash(git log:*)"]
            .map(String::from)
            .to_vec();
        spec.json_schema = Some(review_schema());
        let r = run.agent(spec, &format!("review-{iter}.jsonl")).await?.structured.context("reviewer returned nothing")?;
        run.db.save_review(run.id, iter, &r)?;
        if r["approve"] == true {
            review = Some(r);
            break;
        }
        let comments: Vec<&str> = r["blocking_comments"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).collect();
        feedback = Some(format!("Code review requested changes:\n- {}", comments.join("\n- ")));
    }
    run.iter = 0;

    let max = run.cfg.agents.max_fix_iterations;
    let Some(review) = review else {
        run.db.set_summary(run.id, &format!("Last feedback:\n\n{}\n", feedback.unwrap_or_default()))?;
        anyhow::bail!("not done after {max} iterations (cost ${:.2}); see `harness status issue-{issue_no}`", run.cost);
    };

    // 6. Commit (+ optional PR)
    let stage = run.stage("commit")?;
    let title = review["pr_title"].as_str().unwrap_or(&issue.title).to_string();
    shell::check("git", &["add", "-A"], &run.worktree).await?;
    let _ = std::fs::remove_file(run.worktree.join("local.properties"));
    shell::check("git", &["commit", "-m", &format!("{title}\n\nCloses #{issue_no}")], &run.worktree).await?;
    run.end(stage, true, None, None)?;
    let mut summary = format!("DONE: {title}\nbranch: {branch}\ncost: ${:.2}\n", run.cost);
    if open_pr {
        let stage = run.stage("pr")?;
        let body = format!("{}\n\nCloses #{issue_no}\n\nVerified on a real device by the harness QA agent. Run artifacts: `{}`", review["pr_body"].as_str().unwrap_or(""), run.dir.display());
        let url = github::open_pr(&run.worktree, &run.cfg.github.repo, &run.cfg.app.base_branch, &branch, &title, &body).await?;
        run.end(stage, true, None, Some(&url))?;
        run.db.set_pr(run.id, &url)?;
        let _ = writeln!(summary, "PR: {url}");
    }
    run.db.set_summary(run.id, &summary)?;
    println!("{summary}");
    Ok(())
}

/// Ad-hoc exploratory QA on whatever build is installed — no issue, no code changes.
pub async fn explore(cfg: Config, goal: String) -> Result<()> {
    let mut run = Run::start(cfg, "explore", None, Some(&goal))?;
    let res = explore_inner(&mut run, &goal).await;
    run.finish(res)
}

async fn explore_inner(run: &mut Run, goal: &str) -> Result<()> {
    let adb = Adb::resolve(&run.cfg.device.adb, run.cfg.device.serial.as_deref()).await?;
    let mcp_cfg = run.write_mcp_config(&adb.serial)?;
    let _lease = device::acquire_lease(&adb.serial, "explore").await?;
    let _ = adb.logcat_clear().await;
    let mut spec = AgentSpec::new(
        "qa",
        QA,
        format!("# Exploratory task\n{goal}\n\nThe app is already installed. Do NOT write Maestro flows unless asked."),
        &run.worktree,
    );
    spec.model = run.cfg.dev_model().map(String::from);
    spec.mcp_config = Some(mcp_cfg);
    spec.allowed_tools = vec!["mcp__mobile".into(), "mcp__device".into(), "Read".into(), "Glob".into(), "Grep".into()];
    if goal.to_lowercase().contains("flow") {
        let flows = run.worktree.join(&run.cfg.app.flows_dir);
        std::fs::create_dir_all(&flows)?;
        spec.allowed_tools.extend(flows_write_rules(&flows));
    }
    // Note: a deny rule on Edit would also block Write, so writes are limited by the allow-list only.
    spec.disallowed_tools = vec!["Bash".into(), "WebFetch".into(), "WebSearch".into()];
    spec.json_schema = Some(qa_schema());
    let qa = run.agent(spec, "qa.jsonl").await?.structured.context("no verdict")?;
    run.db.save_qa(run.id, 0, &qa)?;
    let verdict = qa["verdict"].as_str().unwrap_or("?");
    run.db.set_summary(run.id, &format!("QA: {verdict}"))?;
    println!("{}\n\nartifacts: {}\ncost: ${:.2}", serde_json::to_string_pretty(&qa)?, run.dir.display(), run.cost);
    Ok(())
}
