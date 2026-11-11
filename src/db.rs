//! Run history in SQLite (`.harness/harness.db`): runs, stages, agent calls, QA and review results.
//! Large artifacts (transcripts, build logs, screenshots) stay as files in the run dir; rows hold their paths.

use crate::agent::AgentResult;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// UTC timestamp with millis; sorts correctly as text.
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

const SCHEMA: &str = r#"
CREATE TABLE runs (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,      -- also the artifacts dir name under runs/
    kind        TEXT NOT NULL,             -- issue | explore
    issue       INTEGER,
    title       TEXT,                      -- issue title or explore goal
    issue_md    TEXT,
    dir         TEXT NOT NULL,
    worktree    TEXT,
    branch      TEXT,
    pid         INTEGER NOT NULL,
    status      TEXT NOT NULL DEFAULT 'running',  -- running | done | failed (crashed = running + dead pid)
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at TEXT,
    plan_json   TEXT,
    plan_md     TEXT,
    summary     TEXT,
    error       TEXT,
    pr_url      TEXT
);
CREATE TABLE stages (
    id          INTEGER PRIMARY KEY,
    run_id      INTEGER NOT NULL REFERENCES runs(id),
    iter        INTEGER NOT NULL,          -- 0 = outside the fix loop
    name        TEXT NOT NULL,             -- planner | developer | build | unit | apk | install | regression | qa | reviewer | commit | pr
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at TEXT,
    ok          INTEGER,
    log_path    TEXT,
    detail      TEXT
);
CREATE INDEX stages_run ON stages(run_id);
CREATE TABLE agent_calls (
    id                 INTEGER PRIMARY KEY,
    run_id             INTEGER NOT NULL REFERENCES runs(id),
    stage_id           INTEGER NOT NULL REFERENCES stages(id),
    role               TEXT NOT NULL,
    model              TEXT,
    transcript         TEXT NOT NULL,
    started_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at        TEXT,
    duration_ms        INTEGER,
    num_turns          INTEGER,
    cost_usd           REAL,
    input_tokens       INTEGER,
    output_tokens      INTEGER,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    result_text        TEXT,
    structured_json    TEXT,
    error              TEXT
);
CREATE INDEX agent_calls_run ON agent_calls(run_id);
CREATE TABLE qa_results (
    id         INTEGER PRIMARY KEY,
    run_id     INTEGER NOT NULL REFERENCES runs(id),
    iter       INTEGER NOT NULL,
    verdict    TEXT NOT NULL,
    notes      TEXT,
    raw_json   TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE qa_criteria (
    qa_id     INTEGER NOT NULL REFERENCES qa_results(id),
    criterion TEXT NOT NULL,
    met       INTEGER NOT NULL,
    evidence  TEXT
);
CREATE TABLE bugs (
    qa_id         INTEGER NOT NULL REFERENCES qa_results(id),
    title         TEXT NOT NULL,
    steps_json    TEXT,
    expected      TEXT,
    actual        TEXT,
    evidence_json TEXT,
    logs          TEXT
);
CREATE TABLE reviews (
    id         INTEGER PRIMARY KEY,
    run_id     INTEGER NOT NULL REFERENCES runs(id),
    iter       INTEGER NOT NULL,
    approve    INTEGER NOT NULL,
    pr_title   TEXT,
    pr_body    TEXT,
    raw_json   TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE review_comments (
    review_id INTEGER NOT NULL REFERENCES reviews(id),
    body      TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Connection,
}

pub struct NewRun<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub issue: Option<u64>,
    pub title: Option<&'a str>,
    pub dir: &'a Path,
    pub worktree: &'a Path,
}

/// A stopped issue run that `harness run` / `queue` can continue.
pub struct ResumableRun {
    pub id: i64,
    pub name: String,
    pub dir: std::path::PathBuf,
    pub plan_md: Option<String>,
    pub checkpoint: Option<Value>,
    /// Spent so far.
    pub cost: f64,
}

/// One row of `harness status`.
pub struct RunRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub issue: Option<u64>,
    pub title: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub pid: u32,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub pr_url: Option<String>,
    pub complexity: Option<String>,
    pub cost: f64,
    pub iter: u32,
    /// (name, iter, finished)
    pub stage: Option<(String, u32, bool)>,
    pub last_activity: Option<String>,
    /// Transcript of an agent call still in progress.
    pub live_transcript: Option<String>,
    pub qa: Option<(String, usize)>,
    pub review: Option<(bool, usize)>,
}

fn json_str(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().map(String::from)
}

fn json_arr(v: &Value, k: &str) -> Option<String> {
    v.get(k).filter(|x| !x.is_null()).map(|x| x.to_string())
}

impl Db {
    pub fn open(state_dir: &Path) -> Result<Db> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("harness.db");
        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // WAL: `harness status` reads while runs write; parallel runs wait on each other's writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        if version < 2 {
            // v2: resumable runs — a checkpoint per run, Claude session ids per agent call.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE runs ADD COLUMN checkpoint TEXT;
                 ALTER TABLE agent_calls ADD COLUMN session_id TEXT;",
            )?;
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        if version < 3 {
            // v3: planner's complexity rating (S/M/L), to compare cost per finished issue by tier.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("ALTER TABLE runs ADD COLUMN complexity TEXT;")?;
            tx.pragma_update(None, "user_version", 3)?;
            tx.commit()?;
        }
        Ok(Db { conn })
    }

    pub fn set_checkpoint(&self, run: i64, checkpoint: &Value) -> Result<()> {
        self.conn.execute("UPDATE runs SET checkpoint = ?2 WHERE id = ?1", params![run, checkpoint.to_string()])?;
        Ok(())
    }

    /// The issue's newest run if it can be continued: it has a checkpoint and was either
    /// interrupted by an environment problem or its process died (crash, Ctrl+C, reboot).
    pub fn resumable_run(&self, issue: u64) -> Result<Option<ResumableRun>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, status, pid, dir, worktree, branch, plan_md, checkpoint,
                        (SELECT COALESCE(SUM(cost_usd), 0) FROM agent_calls WHERE run_id = runs.id)
                 FROM runs WHERE kind = 'issue' AND issue = ?1 ORDER BY id DESC LIMIT 1",
                [issue as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(2)?,
                        r.get::<_, u32>(3)?,
                        ResumableRun {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            dir: r.get::<_, String>(4)?.into(),
                            plan_md: r.get(7)?,
                            checkpoint: r.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str(&s).ok()),
                            cost: r.get(9)?,
                        },
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(status, pid, run)| {
            let stopped = status == "interrupted" || (status == "running" && !crate::shell::pid_alive(pid));
            (stopped && run.checkpoint.is_some() && run.plan_md.is_some()).then_some(run)
        }))
    }

    /// Issues whose newest run is resumable (see `resumable_run`), oldest first.
    pub fn resumable_issues(&self) -> Result<Vec<u64>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT issue FROM runs WHERE kind = 'issue' AND issue IS NOT NULL ORDER BY issue")?;
        let issues = stmt.query_map([], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = vec![];
        for i in issues {
            if self.resumable_run(i as u64)?.is_some() {
                out.push(i as u64);
            }
        }
        Ok(out)
    }

    /// Takes over a stopped run in this process: closes what the dead process left open.
    pub fn reopen_run(&self, run: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(&format!("UPDATE stages SET finished_at = {NOW}, ok = 0, detail = COALESCE(detail, 'interrupted') WHERE run_id = ?1 AND finished_at IS NULL"), [run])?;
        tx.execute(&format!("UPDATE agent_calls SET finished_at = {NOW}, error = COALESCE(error, 'interrupted') WHERE run_id = ?1 AND finished_at IS NULL"), [run])?;
        tx.execute(
            "UPDATE runs SET status = 'running', error = NULL, finished_at = NULL, pid = ?2 WHERE id = ?1",
            params![run, std::process::id()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_run(&self, r: &NewRun) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (name, kind, issue, title, dir, worktree, pid) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                r.name,
                r.kind,
                r.issue.map(|i| i as i64),
                r.title,
                r.dir.to_string_lossy(),
                r.worktree.to_string_lossy(),
                std::process::id()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_issue(&self, run: i64, title: &str, issue_md: &str) -> Result<()> {
        self.conn.execute("UPDATE runs SET title = ?2, issue_md = ?3 WHERE id = ?1", params![run, title, issue_md])?;
        Ok(())
    }

    pub fn set_worktree(&self, run: i64, worktree: &Path, branch: &str) -> Result<()> {
        self.conn.execute("UPDATE runs SET worktree = ?2, branch = ?3 WHERE id = ?1", params![run, worktree.to_string_lossy(), branch])?;
        Ok(())
    }

    pub fn set_plan(&self, run: i64, plan: &Value, plan_md: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET plan_json = ?2, plan_md = ?3, complexity = ?4 WHERE id = ?1",
            params![run, plan.to_string(), plan_md, plan["complexity"].as_str()],
        )?;
        Ok(())
    }

    pub fn set_summary(&self, run: i64, summary: &str) -> Result<()> {
        self.conn.execute("UPDATE runs SET summary = ?2 WHERE id = ?1", params![run, summary])?;
        Ok(())
    }

    pub fn pr_url(&self, run: i64) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT pr_url FROM runs WHERE id = ?1", [run], |r| r.get(0))?)
    }

    pub fn set_pr(&self, run: i64, url: &str) -> Result<()> {
        self.conn.execute("UPDATE runs SET pr_url = ?2 WHERE id = ?1", params![run, url])?;
        Ok(())
    }

    /// Marks the run finished and closes stages/agent calls left open by an error.
    /// `interrupted`: stopped by an environment problem — the run stays resumable.
    pub fn finish_run(&self, run: i64, error: Option<&str>, interrupted: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(&format!("UPDATE stages SET finished_at = {NOW}, ok = 0 WHERE run_id = ?1 AND finished_at IS NULL"), [run])?;
        tx.execute(&format!("UPDATE agent_calls SET finished_at = {NOW}, error = COALESCE(error, 'interrupted') WHERE run_id = ?1 AND finished_at IS NULL"), [run])?;
        tx.execute(
            &format!("UPDATE runs SET status = ?2, error = ?3, finished_at = {NOW} WHERE id = ?1"),
            params![run, if interrupted { "interrupted" } else if error.is_some() { "failed" } else { "done" }, error],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn stage_start(&self, run: i64, iter: u32, name: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO stages (run_id, iter, name) VALUES (?1, ?2, ?3)", params![run, iter, name])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn stage_end(&self, stage: i64, ok: bool, log: Option<&Path>, detail: Option<&str>) -> Result<()> {
        self.conn.execute(
            &format!("UPDATE stages SET finished_at = {NOW}, ok = ?2, log_path = ?3, detail = ?4 WHERE id = ?1"),
            params![stage, ok, log.map(|p| p.to_string_lossy().into_owned()), detail],
        )?;
        Ok(())
    }

    pub fn agent_start(&self, run: i64, stage: i64, role: &str, model: Option<&str>, transcript: &Path, session_id: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO agent_calls (run_id, stage_id, role, model, transcript, session_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run, stage, role, model, transcript.to_string_lossy(), session_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn agent_end(&self, call: i64, res: &Result<AgentResult>) -> Result<()> {
        let empty = AgentResult::default();
        let (r, err) = match res {
            Ok(r) => (r, r.error.clone()),
            Err(e) => (&empty, Some(format!("{e:#}"))),
        };
        self.conn.execute(
            &format!(
                "UPDATE agent_calls SET finished_at = {NOW}, model = COALESCE(?2, model), duration_ms = ?3, num_turns = ?4,
                 cost_usd = ?5, input_tokens = ?6, output_tokens = ?7, cache_read_tokens = ?8, cache_write_tokens = ?9,
                 result_text = ?10, structured_json = ?11, error = ?12, session_id = COALESCE(?13, session_id) WHERE id = ?1"
            ),
            params![
                call,
                r.model,
                r.duration_ms,
                r.num_turns,
                res.as_ref().ok().map(|r| r.cost_usd),
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.cache_write_tokens,
                res.as_ref().ok().map(|r| r.text.as_str()),
                r.structured.as_ref().map(|v| v.to_string()),
                err,
                r.session_id
            ],
        )?;
        Ok(())
    }

    pub fn save_qa(&self, run: i64, iter: u32, qa: &Value) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO qa_results (run_id, iter, verdict, notes, raw_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run, iter, qa["verdict"].as_str().unwrap_or("?"), json_str(qa, "notes"), qa.to_string()],
        )?;
        let qa_id = tx.last_insert_rowid();
        for c in qa["criteria"].as_array().into_iter().flatten() {
            tx.execute(
                "INSERT INTO qa_criteria (qa_id, criterion, met, evidence) VALUES (?1, ?2, ?3, ?4)",
                params![qa_id, c["criterion"].as_str().unwrap_or(""), c["met"] == true, json_str(c, "evidence")],
            )?;
        }
        for b in qa["bugs"].as_array().into_iter().flatten() {
            tx.execute(
                "INSERT INTO bugs (qa_id, title, steps_json, expected, actual, evidence_json, logs) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    qa_id,
                    b["title"].as_str().unwrap_or(""),
                    json_arr(b, "steps"),
                    json_str(b, "expected"),
                    json_str(b, "actual"),
                    json_arr(b, "evidence"),
                    json_str(b, "logs")
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_review(&self, run: i64, iter: u32, r: &Value) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO reviews (run_id, iter, approve, pr_title, pr_body, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run, iter, r["approve"] == true, json_str(r, "pr_title"), json_str(r, "pr_body"), r.to_string()],
        )?;
        let review_id = tx.last_insert_rowid();
        for c in r["blocking_comments"].as_array().into_iter().flatten().filter_map(|c| c.as_str()) {
            tx.execute("INSERT INTO review_comments (review_id, body) VALUES (?1, ?2)", params![review_id, c])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Id of the newest agent call, 0 if none; a marker for `cost_since`.
    /// (Agent calls, not runs: a resumed run is old but its new calls must count.)
    pub fn cost_marker(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM agent_calls", [], |r| r.get(0))?)
    }

    /// Total cost of agent calls made after `cost_marker()` returned `after`.
    pub fn cost_since(&self, after: i64) -> Result<f64> {
        Ok(self.conn.query_row("SELECT COALESCE(SUM(cost_usd), 0) FROM agent_calls WHERE id > ?1", [after], |r| r.get(0))?)
    }

    /// Most recent `last` runs whose name contains `filter`, oldest first.
    pub fn recent_runs(&self, filter: Option<&str>, last: usize) -> Result<Vec<RunRow>> {
        let pattern = format!("%{}%", filter.unwrap_or(""));
        let mut stmt = self.conn.prepare(
            "SELECT id, name, status, pid, summary, error, pr_url, complexity,
                    (SELECT COALESCE(SUM(cost_usd), 0) FROM agent_calls WHERE run_id = runs.id),
                    (SELECT COALESCE(MAX(iter), 0) FROM stages WHERE run_id = runs.id),
                    kind, issue, title, started_at, finished_at
             FROM runs WHERE name LIKE ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut rows = stmt
            .query_map(params![pattern, last as i64], |r| {
                Ok(RunRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    status: r.get(2)?,
                    pid: r.get(3)?,
                    summary: r.get(4)?,
                    error: r.get(5)?,
                    pr_url: r.get(6)?,
                    complexity: r.get(7)?,
                    cost: r.get(8)?,
                    iter: r.get(9)?,
                    kind: r.get(10)?,
                    issue: r.get::<_, Option<i64>>(11)?.map(|i| i as u64),
                    title: r.get(12)?,
                    started_at: r.get(13)?,
                    finished_at: r.get(14)?,
                    stage: None,
                    last_activity: None,
                    live_transcript: None,
                    qa: None,
                    review: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        for row in &mut rows {
            self.fill_details(row)?;
        }
        Ok(rows)
    }

    /// Everything recorded about one run, for the dashboard; `None` if there is no such run.
    pub fn run_detail(&self, id: i64) -> Result<Option<Value>> {
        let Some(mut run) = self
            .rows_json(
                "SELECT id, name, kind, issue, title, issue_md, branch, status, started_at, finished_at,
                        plan_md, complexity, summary, error, pr_url, checkpoint FROM runs WHERE id = ?1",
                id,
            )?
            .pop()
        else {
            return Ok(None);
        };
        run["stages"] = self
            .rows_json("SELECT id, iter, name, started_at, finished_at, ok, log_path, detail FROM stages WHERE run_id = ?1 ORDER BY id", id)?
            .into();
        run["agent_calls"] = self
            .rows_json(
                "SELECT id, stage_id, role, model, started_at, finished_at, duration_ms, num_turns, cost_usd,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, error, transcript
                 FROM agent_calls WHERE run_id = ?1 ORDER BY id",
                id,
            )?
            .into();
        let mut qa = self.rows_json("SELECT id, iter, verdict, notes, created_at FROM qa_results WHERE run_id = ?1 ORDER BY id", id)?;
        for q in &mut qa {
            let qid = q["id"].as_i64().unwrap_or(0);
            q["criteria"] = self.rows_json("SELECT criterion, met, evidence FROM qa_criteria WHERE qa_id = ?1", qid)?.into();
            q["bugs"] = self.rows_json("SELECT title, steps_json, expected, actual, evidence_json, logs FROM bugs WHERE qa_id = ?1", qid)?.into();
        }
        run["qa"] = qa.into();
        let mut reviews = self.rows_json("SELECT id, iter, approve, pr_title, created_at FROM reviews WHERE run_id = ?1 ORDER BY id", id)?;
        for r in &mut reviews {
            let rid = r["id"].as_i64().unwrap_or(0);
            r["comments"] = self.rows_json("SELECT body FROM review_comments WHERE review_id = ?1", rid)?.into();
        }
        run["reviews"] = reviews.into();
        Ok(Some(run))
    }

    /// Rows of a one-parameter query as JSON objects keyed by column name.
    fn rows_json(&self, sql: &str, param: i64) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(sql)?;
        let cols: Vec<String> = stmt.column_names().map(String::from).collect();
        let rows = stmt.query_map([param], |r| {
            let mut obj = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                use rusqlite::types::ValueRef;
                let v = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => n.into(),
                    ValueRef::Real(f) => f.into(),
                    ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned().into(),
                };
                obj.insert(c.clone(), v);
            }
            Ok(Value::Object(obj))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn fill_details(&self, row: &mut RunRow) -> Result<()> {
        let id = row.id;
        row.stage = self
            .conn
            .query_row(
                "SELECT name, iter, finished_at IS NOT NULL FROM stages WHERE run_id = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.last_activity = self.conn.query_row(
            "SELECT MAX(t) FROM (
                 SELECT started_at t FROM runs WHERE id = ?1 UNION ALL SELECT finished_at FROM runs WHERE id = ?1
                 UNION ALL SELECT started_at FROM stages WHERE run_id = ?1 UNION ALL SELECT finished_at FROM stages WHERE run_id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        row.live_transcript = self
            .conn
            .query_row(
                "SELECT transcript FROM agent_calls WHERE run_id = ?1 AND finished_at IS NULL ORDER BY id DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        row.qa = self
            .conn
            .query_row(
                "SELECT verdict, (SELECT COUNT(*) FROM bugs WHERE qa_id = q.id) FROM qa_results q WHERE run_id = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)),
            )
            .optional()?;
        row.review = self
            .conn
            .query_row(
                "SELECT approve, (SELECT COUNT(*) FROM review_comments WHERE review_id = v.id) FROM reviews v WHERE run_id = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)),
            )
            .optional()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_lifecycle() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("harness-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir)?;
        let run = db.create_run(&NewRun { name: "20261110-120000-issue-7", kind: "issue", issue: Some(7), title: None, dir: &dir, worktree: &dir })?;
        db.set_issue(run, "Fix login", "# Fix login")?;

        let st = db.stage_start(run, 1, "developer")?;
        let call = db.agent_start(run, st, "developer", None, &dir.join("developer-1.jsonl"), Some("s1"))?;
        let res = AgentResult { cost_usd: 0.5, input_tokens: Some(10), model: Some("m".into()), ..Default::default() };
        db.agent_end(call, &Ok(res))?;
        db.stage_end(st, true, None, None)?;

        db.save_qa(run, 1, &json!({"verdict": "fail", "criteria": [{"criterion": "c", "met": false, "evidence": "e"}],
            "bugs": [{"title": "b", "steps": ["s"], "expected": "x", "actual": "y"}], "flows_written": []}))?;
        db.save_review(run, 1, &json!({"approve": false, "blocking_comments": ["a", "b"], "pr_title": "t", "pr_body": ""}))?;
        // Left open: finish_run must close it.
        db.stage_start(run, 2, "build")?;
        db.finish_run(run, Some("boom"), false)?;

        // Reopening must not re-apply the schema.
        let db = Db::open(&dir)?;
        let rows = db.recent_runs(Some("issue-7"), 5)?;
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.status, "failed");
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert_eq!(r.cost, 0.5);
        assert_eq!(r.iter, 2);
        assert_eq!(r.stage, Some(("build".into(), 2, true)));
        assert_eq!(r.qa, Some(("fail".into(), 1)));
        assert_eq!(r.review, Some((false, 2)));
        assert!(r.live_transcript.is_none());
        assert_eq!((r.kind.as_str(), r.issue), ("issue", Some(7)));
        let d = db.run_detail(run)?.expect("detail");
        assert_eq!(d["title"], "Fix login");
        assert_eq!(d["stages"].as_array().map(Vec::len), Some(2));
        assert_eq!(d["agent_calls"][0]["cost_usd"], 0.5);
        assert_eq!(d["qa"][0]["bugs"][0]["title"], "b");
        assert_eq!(d["reviews"][0]["comments"].as_array().map(Vec::len), Some(2));
        assert!(db.run_detail(run + 100)?.is_none());
        assert!(db.recent_runs(Some("issue-8"), 5)?.is_empty());
        assert_eq!(db.cost_marker()?, call);
        assert_eq!(db.cost_since(call - 1)?, 0.5);
        assert_eq!(db.cost_since(call)?, 0.0);
        // Failed runs are not resumable; interrupted ones with a plan and checkpoint are.
        assert!(db.resumable_run(7)?.is_none());
        db.set_plan(run, &json!({}), "plan")?;
        db.set_checkpoint(run, &json!({"iter": 2, "step": "gate"}))?;
        db.finish_run(run, Some("device gone"), true)?;
        let r = db.resumable_run(7)?.expect("resumable");
        assert_eq!((r.id, r.cost), (run, 0.5));
        assert_eq!(db.resumable_issues()?, vec![7]);
        db.reopen_run(run)?;
        assert!(db.resumable_run(7)?.is_none(), "running in this live process");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
