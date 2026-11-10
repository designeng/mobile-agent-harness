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

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ListedIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl ListedIssue {
    pub fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l.name == name)
    }
}

/// All open issues with labels (one `gh` call).
pub async fn list_open_issues(repo: &str) -> Result<Vec<ListedIssue>> {
    let json = shell::check(
        "gh",
        &["issue", "list", "-R", repo, "--state", "open", "--limit", "500", "--json", "number,title,body,labels"],
        Path::new("."),
    )
    .await?;
    serde_json::from_str(&json).context("parsing gh issue list json")
}

/// Creates the label or updates it if it exists.
pub async fn ensure_label(repo: &str, name: &str, color: &str, description: &str) -> Result<()> {
    shell::check("gh", &["label", "create", name, "-R", repo, "--force", "--color", color, "--description", description], Path::new("."))
        .await
        .map(drop)
}

pub async fn edit_labels(repo: &str, number: u64, add: &[&str], remove: &[&str]) -> Result<()> {
    let n = number.to_string();
    let mut args = vec!["issue", "edit", n.as_str(), "-R", repo];
    for l in add {
        args.extend(["--add-label", l]);
    }
    for l in remove {
        args.extend(["--remove-label", l]);
    }
    shell::check("gh", &args, Path::new(".")).await.map(drop)
}

pub async fn comment(repo: &str, number: u64, body: &str) -> Result<()> {
    let n = number.to_string();
    shell::check("gh", &["issue", "comment", &n, "-R", repo, "--body", body], Path::new(".")).await.map(drop)
}
