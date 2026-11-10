//! Issue → plan → (develop → gate → device QA → review)* → commit/PR.

use crate::agent::{self, AgentSpec};
use crate::config::Config;
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

pub struct Run {
    pub cfg: Config,
    pub dir: PathBuf,
    pub worktree: PathBuf,
    pub cost: f64,
}

impl Run {
    fn save(&self, name: &str, content: &str) -> Result<()> {
        std::fs::write(self.dir.join(name), content).with_context(|| format!("writing {name}"))
    }

    async fn agent(&mut self, spec: AgentSpec, log: &str) -> Result<agent::AgentResult> {
        let res = agent::run(spec, &self.dir.join(log)).await?;
        self.cost += res.cost_usd;
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

/// Deterministic gate: build + unit tests. Returns feedback for the developer on failure.
async fn gate(cfg: &Config, wt: &Path, dir: &Path, iter: u32) -> Result<Option<String>> {
    let long = Duration::from_secs(1800);
    tracing::info!("gate: build");
    let b = shell::sh(&cfg.app.build, wt, long).await?;
    std::fs::write(dir.join(format!("build-{iter}.log")), format!("{}\n{}", b.stdout, b.stderr))?;
    if !b.ok {
        return Ok(Some(format!("The build FAILED (`{}`):\n```\n{}\n```", cfg.app.build, b.tail(6000))));
    }
    if let Some(t) = &cfg.app.unit_tests {
        tracing::info!("gate: unit tests");
        let u = shell::sh(t, wt, long).await?;
        std::fs::write(dir.join(format!("unit-{iter}.log")), format!("{}\n{}", u.stdout, u.stderr))?;
        if !u.ok {
            return Ok(Some(format!("Unit tests FAILED (`{t}`):\n```\n{}\n```", u.tail(6000))));
        }
    }
    let apk = wt.join(&cfg.app.apk);
    if !apk.exists() {
        return Ok(Some(format!("Build succeeded but APK not found at {}", cfg.app.apk.display())));
    }
    Ok(None)
}

/// Runs every saved Maestro flow. Returns failure descriptions.
async fn regression(cfg: &Config, wt: &Path, adb: &Adb, dir: &Path) -> Vec<String> {
    let flows = wt.join(&cfg.app.flows_dir);
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
        if let Err(e) = mcp::run_maestro(adb, &cfg.device.maestro, &f, dir).await {
            fails.push(format!("Regression flow {} failed:\n{e}", f.display()));
        }
    }
    fails
}

pub async fn run_issue(cfg: Config, issue_no: u64, open_pr: bool) -> Result<()> {
    let issue = github::fetch_issue(&cfg.github.repo, issue_no).await?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = cfg.state_dir().join("runs").join(format!("{ts}-issue-{issue_no}"));
    std::fs::create_dir_all(&dir)?;
    let (worktree, branch) = prepare_worktree(&cfg, issue_no).await?;
    tracing::info!(run = %dir.display(), worktree = %worktree.display(), "run started");
    let adb = Adb::resolve(&cfg.device.adb, cfg.device.serial.as_deref()).await?;

    let mut run = Run { cfg, dir, worktree, cost: 0.0 };
    let issue_md = issue.as_markdown();
    run.save("issue.md", &issue_md)?;

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
    run.save("plan.md", &plan_md)?;

    let mcp_cfg = run.write_mcp_config(&adb.serial)?;
    let flows_rel = run.cfg.app.flows_dir.display().to_string();
    let mut feedback: Option<String> = None;
    let mut review: Option<Value> = None;

    for iter in 1..=run.cfg.agents.max_fix_iterations {
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
        let dev = run.agent(spec, &format!("developer-{iter}.jsonl")).await?;
        run.save(&format!("developer-{iter}.md"), &dev.text)?;

        // 3. Deterministic gate
        if let Some(f) = gate(&run.cfg, &run.worktree, &run.dir, iter).await? {
            tracing::warn!("gate failed");
            feedback = Some(f);
            continue;
        }

        // 4. Device: install, regression, exploratory QA — under an exclusive lease.
        let qa = {
            let _lease = device::acquire_lease(&adb.serial, &format!("issue-{issue_no}")).await?;
            let inst = adb.install(&run.worktree.join(&run.cfg.app.apk)).await?;
            if !inst.ok {
                feedback = Some(format!("APK install failed:\n{}", inst.tail(3000)));
                continue;
            }
            let reg_fails = regression(&run.cfg, &run.worktree, &adb, &run.dir).await;
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
            run.save(&format!("qa-{iter}.json"), &serde_json::to_string_pretty(&qa)?)?;
            qa
        };

        match qa["verdict"].as_str() {
            Some("pass") => {}
            Some("blocked") => anyhow::bail!("QA blocked (environment problem): {}", qa["notes"]),
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
        run.save(&format!("review-{iter}.json"), &serde_json::to_string_pretty(&r)?)?;
        if r["approve"] == true {
            review = Some(r);
            break;
        }
        let comments: Vec<&str> = r["blocking_comments"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).collect();
        feedback = Some(format!("Code review requested changes:\n- {}", comments.join("\n- ")));
    }

    let Some(review) = review else {
        run.save("summary.md", &format!("FAILED after {} iterations. Last feedback:\n\n{}\n", run.cfg.agents.max_fix_iterations, feedback.unwrap_or_default()))?;
        anyhow::bail!("issue #{issue_no}: not done after {} iterations (cost ${:.2}); see {}", run.cfg.agents.max_fix_iterations, run.cost, run.dir.display());
    };

    // 6. Commit (+ optional PR)
    let title = review["pr_title"].as_str().unwrap_or(&issue.title).to_string();
    shell::check("git", &["add", "-A"], &run.worktree).await?;
    let _ = std::fs::remove_file(run.worktree.join("local.properties"));
    shell::check("git", &["commit", "-m", &format!("{title}\n\nCloses #{issue_no}")], &run.worktree).await?;
    let mut summary = format!("DONE: {title}\nbranch: {branch}\ncost: ${:.2}\n", run.cost);
    if open_pr {
        let body = format!("{}\n\nCloses #{issue_no}\n\nVerified on a real device by the harness QA agent. Run artifacts: `{}`", review["pr_body"].as_str().unwrap_or(""), run.dir.display());
        let url = github::open_pr(&run.worktree, &run.cfg.github.repo, &run.cfg.app.base_branch, &branch, &title, &body).await?;
        let _ = writeln!(summary, "PR: {url}");
    }
    run.save("summary.md", &summary)?;
    println!("{summary}");
    Ok(())
}

/// Ad-hoc exploratory QA on whatever build is installed — no issue, no code changes.
pub async fn explore(cfg: Config, goal: String) -> Result<()> {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = cfg.state_dir().join("runs").join(format!("{ts}-explore"));
    std::fs::create_dir_all(&dir)?;
    let adb = Adb::resolve(&cfg.device.adb, cfg.device.serial.as_deref()).await?;
    let worktree = cfg.app.repo.clone();
    let mut run = Run { cfg, dir, worktree, cost: 0.0 };
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
    let pretty = serde_json::to_string_pretty(&qa)?;
    run.save("qa.json", &pretty)?;
    println!("{pretty}\n\nartifacts: {}\ncost: ${:.2}", run.dir.display(), run.cost);
    Ok(())
}
