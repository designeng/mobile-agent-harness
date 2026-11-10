use crate::shell;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub author: Author,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

impl Issue {
    pub fn as_markdown(&self) -> String {
        let mut s = format!("# #{} {}\n\n{}\n", self.number, self.title, self.body);
        for c in &self.comments {
            s.push_str(&format!("\n---\n**{}:** {}\n", c.author.login, c.body));
        }
        s
    }
}

pub async fn fetch_issue(repo: &str, number: u64) -> Result<Issue> {
    let n = number.to_string();
    let json = shell::check(
        "gh",
        &["issue", "view", &n, "-R", repo, "--json", "number,title,body,comments"],
        Path::new("."),
    )
    .await?;
    serde_json::from_str(&json).context("parsing gh issue json")
}

/// Push the branch and open a draft PR. Returns the PR URL.
pub async fn open_pr(worktree: &Path, repo: &str, base: &str, branch: &str, title: &str, body: &str) -> Result<String> {
    shell::check("git", &["push", "-u", "origin", branch], worktree).await?;
    shell::check(
        "gh",
        &["pr", "create", "-R", repo, "--draft", "--base", base, "--head", branch, "--title", title, "--body", body],
        worktree,
    )
    .await
}
