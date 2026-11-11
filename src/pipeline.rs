//! Issue → plan → (develop → gate → device QA → review)* → commit/PR.

use crate::agent::{self, AgentSpec};
use crate::config::Config;
use crate::db::{Db, NewRun, ResumableRun};
use crate::device::{self, Adb};
use crate::{github, mcp, shell};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

    /// Continues a stopped run (same id, dir and history) in this process.
    fn resume(cfg: Config, r: ResumableRun) -> Result<Run> {
        let db = Db::open(&cfg.state_dir())?;
        db.reopen_run(r.id)?;
        let worktree = cfg.app.repo.clone();
        Ok(Run { cfg, dir: r.dir, worktree, cost: r.cost, db, id: r.id, iter: 0 })
    }

    /// Records the run's outcome; passes the result through. Environment failures leave
    /// the run `interrupted` (resumable), anything else marks it `failed`.
    pub(crate) fn finish<T>(&self, res: Result<T>) -> Result<T> {
        let err = res.as_ref().err().map(|e| format!("{e:#}"));
        let interrupted = res.as_ref().err().is_some_and(|e| e.downcast_ref::<EnvError>().is_some());
        if let Err(e) = self.db.finish_run(self.id, err.as_deref(), interrupted) {
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
    pub(crate) async fn agent(&mut self, mut spec: AgentSpec, log: &str) -> Result<agent::AgentResult> {
        let role = spec.role;
        if let Some(dir) = self.skills_bundle(role)? {
            spec.add_dirs.push(dir);
        }
        let path = unique_path(&self.dir, log);
        let stage = self.stage(role)?;
        let session = spec.resume.as_deref().or(spec.session_id.as_deref());
        let call = self.db.agent_start(self.id, stage, role, spec.model.as_deref(), &path, session)?;
        let res = agent::run(spec, &path).await;
        self.db.agent_end(call, &res)?;
        self.end(stage, matches!(&res, Ok(r) if r.error.is_none()), None, None)?;
        // No result at all = the CLI died or could not start (network, auth, killed): an
        // environment problem, so the run stays resumable instead of failing the issue.
        let res = res.map_err(|e| EnvError(format!("{e:#}")))?;
        self.cost += res.cost_usd;
        if let Some(e) = res.error {
            anyhow::bail!(e);
        }
        Ok(res)
    }

    /// Copies the role's configured skills into `<run>/skills-<role>/.claude/skills/`, which
    /// `claude` loads from an `--add-dir`. The copy also records which skill versions ran.
    fn skills_bundle(&self, role: &str) -> Result<Option<PathBuf>> {
        let names = self.cfg.skills_for(role);
        if names.is_empty() {
            return Ok(None);
        }
        let root = self.dir.join(format!("skills-{role}"));
        let skills = root.join(".claude").join("skills");
        if skills.exists() {
            return Ok(Some(root)); // already built earlier in this run (fix-loop iteration)
        }
        for n in &names {
            let src = self.cfg.skill_path(n)?;
            copy_dir(&src, &skills.join(n)).with_context(|| format!("copying skill {n}"))?;
        }
        tracing::info!(role, skills = names.join(","), "skills attached");
        Ok(Some(root))
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

/// `dir/name`, or `dir/<stem>-r2.<ext>`, `-r3`… if taken (a resumed step must not
/// overwrite the transcript of the interrupted attempt).
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    (2..)
        .map(|i| dir.join(if ext.is_empty() { format!("{stem}-r{i}") } else { format!("{stem}-r{i}.{ext}") }))
        .find(|p| !p.exists())
        .unwrap()
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        // metadata() follows symlinks, so a linked file or folder is copied as its target.
        if std::fs::metadata(e.path())?.is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
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

/// Where an issue run stands; saved after every step so a stopped run continues from
/// the step it was in (`harness run N` / `harness queue` pick it up automatically).
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Checkpoint {
    /// Fix-loop iteration, 1-based.
    iter: u32,
    step: Step,
    /// What the developer must fix in this iteration (gate errors, QA bugs, review comments).
    #[serde(default)]
    feedback: Option<String>,
    /// Developer's Claude session: continued across iterations and after interruptions.
    #[serde(default)]
    dev_session: Option<String>,
    /// The developer turn was started but never finished (process died mid-way).
    #[serde(default)]
    dev_running: bool,
    /// Approved review (pr_title / pr_body), from `Commit` on.
    #[serde(default)]
    review: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
enum Step {
    Develop,
    Gate,
    Device,
    Review,
    Commit,
    Pr,
}

impl Checkpoint {
    fn new() -> Self {
        Checkpoint { iter: 1, step: Step::Develop, feedback: None, dev_session: None, dev_running: false, review: None }
    }

    /// Back to the developer with feedback.
    fn retry(&mut self, feedback: String) {
        self.iter += 1;
        self.step = Step::Develop;
        self.feedback = Some(feedback);
    }
}

impl Run {
    fn save(&self, cp: &Checkpoint) -> Result<()> {
        self.db.set_checkpoint(self.id, &serde_json::to_value(cp)?)
    }
}

/// `fresh`: ignore a stopped earlier run of this issue and start over (the worktree is kept).
pub async fn run_issue(cfg: Config, issue_no: u64, open_pr: bool, fresh: bool) -> Result<()> {
    let prev = if fresh { None } else { Db::open(&cfg.state_dir())?.resumable_run(issue_no)? };
    let (mut run, resume) = match prev {
        Some(r) => {
            let cp: Checkpoint = serde_json::from_value(r.checkpoint.clone().unwrap_or_default()).context("bad checkpoint")?;
            let plan_md = r.plan_md.clone().unwrap_or_default();
            tracing::info!(run = %r.name, iter = cp.iter, step = ?cp.step, spent = format!("{:.2}", r.cost), "resuming stopped run");
            (Run::resume(cfg, r)?, Some((plan_md, cp)))
        }
        None => (Run::start(cfg, "issue", Some(issue_no), None)?, None),
    };
    let res = issue_inner(&mut run, issue_no, open_pr, resume).await;
    run.finish(res)
}

async fn issue_inner(run: &mut Run, issue_no: u64, open_pr: bool, resume: Option<(String, Checkpoint)>) -> Result<()> {
    let issue = github::fetch_issue(&run.cfg.github.repo, issue_no).await.map_err(|e| EnvError(format!("github: {e:#}")))?;
    let issue_md = issue.as_markdown();
    run.db.set_issue(run.id, &issue.title, &issue_md)?;
    let (worktree, branch) = prepare_worktree(&run.cfg, issue_no).await?;
    run.db.set_worktree(run.id, &worktree, &branch)?;
    run.worktree = worktree;
    tracing::info!(dir = %run.dir.display(), worktree = %run.worktree.display(), "worktree ready");
    let adb = Adb::resolve(&run.cfg.device.adb, run.cfg.device.serial.as_deref())
        .await
        .map_err(|e| EnvError(format!("device: {e:#}")))?;
    // Fail before planning/building (minutes + $) if the device will reject the APK anyway.
    adb.check_usb_install_allowed().await.map_err(|e| EnvError(format!("device: {e:#}")))?;

    let contract = format!(
        "\n# Build contract (the harness enforces this)\n\
         - Build gate — the harness runs it from the repo root right after you finish and sends you any errors; \
         do NOT run it yourself: `{}`\n- APK expected at: `{}`\n- Unit tests: `{}`\n\
         - Must run on a real device WITHOUT a dev server (JS bundle embedded in the APK).\n",
        run.cfg.app.build,
        run.cfg.app.apk.display(),
        run.cfg.app.unit_tests.as_deref().unwrap_or("(none)")
    );

    // 1. Plan (once per run; a resumed run reuses it)
    let (plan_md, mut cp) = match resume {
        Some(r) => r,
        None => {
            let mut spec = AgentSpec::new("planner", PLANNER, format!("{issue_md}\n{contract}\nProduce the plan."), &run.worktree);
            spec.model = run.cfg.light_model().map(String::from);
            spec.allowed_tools = vec!["Read".into(), "Glob".into(), "Grep".into()];
            spec.json_schema = Some(plan_schema());
            let plan = run.agent(spec, "planner.jsonl").await?.structured.context("planner returned no plan")?;
            let plan_md = render_plan(&plan);
            run.db.set_plan(run.id, &plan, &plan_md)?;
            let cp = Checkpoint::new();
            run.save(&cp)?;
            (plan_md, cp)
        }
    };

    let mcp_cfg = run.write_mcp_config(&adb.serial)?;
    let flows_rel = run.cfg.app.flows_dir.display().to_string();
    let max = run.cfg.agents.max_fix_iterations;

    loop {
        if cp.iter > max {
            run.iter = 0;
            run.db.set_summary(run.id, &format!("Last feedback:\n\n{}\n", cp.feedback.clone().unwrap_or_default()))?;
            anyhow::bail!("not done after {max} iterations (cost ${:.2}); see `harness status issue-{issue_no}`", run.cost);
        }
        run.iter = if matches!(cp.step, Step::Commit | Step::Pr) { 0 } else { cp.iter };
        match cp.step {
            // 2. Develop
            Step::Develop => {
                tracing::info!(iter = cp.iter, "iteration");
                develop(run, &mut cp, &issue_md, &plan_md, &contract).await?;
                wip_commit(run, &format!("wip: issue #{issue_no} iteration {} (developer)", cp.iter)).await?;
                cp.step = Step::Gate;
            }

            // 3. Deterministic gate
            Step::Gate => match gate(run).await? {
                Some(f) => {
                    tracing::warn!("gate failed");
                    cp.retry(f);
                }
                None => cp.step = Step::Device,
            },

            // 4. Device: install, regression, exploratory QA — under an exclusive lease.
            Step::Device => {
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
                    let qa = run.agent(spec, &format!("qa-{}.jsonl", cp.iter)).await?.structured.context("qa returned no verdict")?;
                    run.db.save_qa(run.id, cp.iter, &qa)?;
                    qa
                };
                // QA may have saved new Maestro flows.
                wip_commit(run, &format!("wip: issue #{issue_no} iteration {} (qa flows)", cp.iter)).await?;
                match qa["verdict"].as_str() {
                    Some("pass") => cp.step = Step::Review,
                    Some("blocked") => return Err(EnvError(format!("QA blocked (environment problem): {}", qa["notes"])).into()),
                    _ => cp.retry(render_qa_feedback(&qa)),
                }
            }

            // 5. Review
            Step::Review => {
                let base = base_commit(run).await?;
                let mut spec = AgentSpec::new(
                    "reviewer",
                    REVIEWER,
                    format!(
                        "{issue_md}\n\n# Plan\n{plan_md}\n\nReview this branch's changes: `git diff {base}` \
                         (the branch holds work-in-progress commits; `git log {base}..HEAD` lists them)."
                    ),
                    &run.worktree,
                );
                spec.model = run.cfg.light_model().map(String::from);
                spec.allowed_tools = ["Read", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)", "Bash(git log:*)"]
                    .map(String::from)
                    .to_vec();
                spec.json_schema = Some(review_schema());
                let r = run.agent(spec, &format!("review-{}.jsonl", cp.iter)).await?.structured.context("reviewer returned nothing")?;
                run.db.save_review(run.id, cp.iter, &r)?;
                if r["approve"] == true {
                    cp.review = Some(r);
                    cp.step = Step::Commit;
                } else {
                    let comments: Vec<&str> = r["blocking_comments"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).collect();
                    cp.retry(format!("Code review requested changes:\n- {}", comments.join("\n- ")));
                }
            }

            // 6. Commit: squash the WIP commits into one (idempotent, so safe to repeat on resume).
            Step::Commit => {
                let stage = run.stage("commit")?;
                let title = commit_title(&cp, &issue.title);
                let base = base_commit(run).await?;
                shell::check("git", &["reset", "--soft", &base], &run.worktree).await?;
                stage_all(run).await?;
                shell::check("git", &["commit", "--no-verify", "-m", &format!("{title}\n\nCloses #{issue_no}")], &run.worktree).await?;
                run.end(stage, true, None, None)?;
                if !open_pr {
                    break;
                }
                cp.step = Step::Pr;
            }

            // 7. PR
            Step::Pr => {
                let stage = run.stage("pr")?;
                let title = commit_title(&cp, &issue.title);
                let pr_body = cp.review.as_ref().and_then(|r| r["pr_body"].as_str()).unwrap_or("");
                let body = format!("{pr_body}\n\nCloses #{issue_no}\n\nVerified on a real device by the harness QA agent. Run artifacts: `{}`", run.dir.display());
                let url = github::open_pr(&run.worktree, &run.cfg.github.repo, &run.cfg.app.base_branch, &branch, &title, &body)
                    .await
                    .map_err(|e| EnvError(format!("opening PR: {e:#}")))?;
                run.end(stage, true, None, Some(&url))?;
                run.db.set_pr(run.id, &url)?;
                break;
            }
        }
        run.save(&cp)?;
    }

    let title = commit_title(&cp, &issue.title);
    let mut summary = format!("DONE: {title}\nbranch: {branch}\ncost: ${:.2}\n", run.cost);
    if let Some(url) = run.db.pr_url(run.id)?.filter(|_| open_pr) {
        let _ = writeln!(summary, "PR: {url}");
    }
    run.db.set_summary(run.id, &summary)?;
    println!("{summary}");
    Ok(())
}

fn commit_title(cp: &Checkpoint, fallback: &str) -> String {
    cp.review.as_ref().and_then(|r| r["pr_title"].as_str()).unwrap_or(fallback).to_string()
}

/// The developer step. Continues the developer's own Claude session when there is one
/// (it remembers its earlier decisions), otherwise starts a new one with the full context.
async fn develop(run: &mut Run, cp: &mut Checkpoint, issue_md: &str, plan_md: &str, contract: &str) -> Result<()> {
    let log = format!("developer-{}.jsonl", cp.iter);
    if let Some(session) = cp.dev_session.clone() {
        let prompt = match (&cp.feedback, cp.dev_running) {
            (_, true) => "Your previous turn was interrupted (the harness was restarted). Check `git status` / `git diff` \
                          to see what is already done, then finish the task."
                .to_string(),
            (Some(f), false) => format!("# Feedback from the previous iteration — fix this\n{f}\n"),
            (None, false) => "Continue the task.".to_string(),
        };
        cp.dev_running = true;
        run.save(cp)?;
        let mut spec = developer_spec(run, prompt);
        spec.resume = Some(session.clone());
        let path = run.dir.join(&log);
        match run.agent(spec, &log).await {
            Ok(res) => {
                cp.dev_session = res.session_id.or(Some(session));
                cp.dev_running = false;
                return Ok(());
            }
            // Session gone (e.g. another machine, cleared ~/.claude): start over below.
            Err(e) if !transcript_has_turns(&path) => tracing::warn!("cannot resume developer session {session}: {e:#}; starting a new one"),
            Err(e) => return Err(e),
        }
    }
    let mut prompt = format!("{issue_md}\n\n# Plan\n{plan_md}\n{contract}");
    if let Some(f) = &cp.feedback {
        let _ = write!(prompt, "\n# Feedback from the previous iteration — fix this\n{f}\n");
    }
    let mut spec = developer_spec(run, prompt);
    let id = agent::new_session_id()?;
    spec.session_id = Some(id.clone());
    cp.dev_session = Some(id.clone());
    cp.dev_running = true;
    run.save(cp)?;
    let res = run.agent(spec, &log).await?;
    cp.dev_session = res.session_id.or(Some(id));
    cp.dev_running = false;
    Ok(())
}

fn developer_spec(run: &Run, prompt: String) -> AgentSpec {
    let mut spec = AgentSpec::new("developer", DEVELOPER, prompt, &run.worktree);
    spec.model = run.cfg.dev_model().map(String::from);
    spec.persist_session = true;
    spec.allowed_tools = ["Read", "Edit", "Write", "Glob", "Grep", "Bash"].map(String::from).to_vec();
    spec.disallowed_tools = [
        "Bash(git push:*)", "Bash(git commit:*)", "Bash(git reset:*)", "Bash(git checkout:*)",
        "Bash(adb:*)", "Bash(maestro:*)", "Bash(rm -rf:*)", "WebFetch", "WebSearch",
    ]
    .map(String::from)
    .to_vec();
    spec
}

fn transcript_has_turns(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.contains(r#""type":"assistant""#))
}

/// Stages everything except the copied (untracked, machine-specific) `local.properties`.
async fn stage_all(run: &Run) -> Result<()> {
    shell::check("git", &["add", "-A", "--", ".", ":(exclude)local.properties"], &run.worktree).await?;
    Ok(())
}

/// Commits the worktree state on the harness branch (if anything changed), so progress
/// survives restarts and each step's change is visible in `git log`. Squashed at the end.
async fn wip_commit(run: &Run, msg: &str) -> Result<()> {
    stage_all(run).await?;
    let staged = shell::exec("git", &["diff", "--cached", "--quiet"], &run.worktree, Duration::from_secs(60)).await?;
    if !staged.ok {
        shell::check("git", &["commit", "-q", "--no-verify", "-m", msg], &run.worktree).await?;
    }
    Ok(())
}

/// Where this branch forked from the base branch.
async fn base_commit(run: &Run) -> Result<String> {
    let base = &run.cfg.app.base_branch;
    match shell::check("git", &["merge-base", "HEAD", &format!("origin/{base}")], &run.worktree).await {
        Ok(sha) => Ok(sha),
        Err(_) => shell::check("git", &["merge-base", "HEAD", base], &run.worktree).await,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_roundtrip_and_retry() {
        let mut cp = Checkpoint::new();
        cp.step = Step::Device;
        cp.dev_session = Some("s".into());
        cp.retry("fix it".into());
        let v = serde_json::to_value(&cp).unwrap();
        assert_eq!(v["step"], "develop");
        let back: Checkpoint = serde_json::from_value(v).unwrap();
        assert_eq!((back.iter, back.step, back.feedback.as_deref(), back.dev_session.as_deref()), (2, Step::Develop, Some("fix it"), Some("s")));
        // Older checkpoints without optional fields still load.
        let old: Checkpoint = serde_json::from_value(json!({"iter": 1, "step": "gate"})).unwrap();
        assert_eq!(old.step, Step::Gate);
    }

    #[test]
    fn unique_path_keeps_earlier_transcripts() {
        let dir = std::env::temp_dir().join(format!("harness-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "developer-1.jsonl"), dir.join("developer-1.jsonl"));
        std::fs::write(dir.join("developer-1.jsonl"), "").unwrap();
        assert_eq!(unique_path(&dir, "developer-1.jsonl"), dir.join("developer-1-r2.jsonl"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
