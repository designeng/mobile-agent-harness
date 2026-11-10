mod agent;
mod config;
mod db;
mod device;
mod github;
mod mcp;
mod pipeline;
mod shell;
mod status;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "harness", about = "Agent software factory for Android, tested on a real device")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check device, tools and config.
    Doctor {
        #[arg(long, default_value = "harness.toml")]
        config: PathBuf,
    },
    /// Implement a GitHub issue end-to-end: plan → code → build → device QA → review → commit.
    Run {
        issue: u64,
        #[arg(long, default_value = "harness.toml")]
        config: PathBuf,
        /// Push the branch and open a draft PR at the end.
        #[arg(long)]
        pr: bool,
    },
    /// Exploratory QA of the installed app with a free-form goal (no code changes).
    Explore {
        goal: String,
        #[arg(long, default_value = "harness.toml")]
        config: PathBuf,
    },
    /// Show state of runs: stage, iteration, cost, QA/review verdicts.
    Status {
        /// Only runs whose name contains this (e.g. `issue-2`).
        run: Option<String>,
        /// How many most recent runs to show.
        #[arg(long, default_value_t = 5)]
        last: usize,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "harness.toml")]
        config: PathBuf,
    },
    /// Device MCP server over stdio (spawned by agents, not by hand).
    Mcp {
        #[arg(long)]
        serial: Option<String>,
        #[arg(long, default_value = "adb")]
        adb: String,
        #[arg(long)]
        maestro: String,
        #[arg(long)]
        package: String,
        #[arg(long)]
        apk: PathBuf,
        #[arg(long)]
        flows_dir: PathBuf,
        #[arg(long)]
        artifacts: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout belongs to MCP in `mcp` mode, so logs always go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    match Cli::parse().cmd {
        Cmd::Doctor { config } => doctor(&config).await,
        Cmd::Run { issue, config, pr } => {
            let cfg = config::Config::load(&config)?;
            pipeline::run_issue(cfg, issue, pr).await
        }
        Cmd::Explore { goal, config } => {
            let cfg = config::Config::load(&config)?;
            pipeline::explore(cfg, goal).await
        }
        Cmd::Status { run, last, json, config } => status::show(&config::Config::load(&config)?, run.as_deref(), last, json),
        Cmd::Mcp { serial, adb, maestro, package, apk, flows_dir, artifacts } => {
            mcp::serve(mcp::McpArgs { serial, adb, maestro, package, apk, flows_dir, artifacts }).await
        }
    }
}

async fn doctor(config: &Path) -> Result<()> {
    let mut ok = true;
    let mut check = |name: &str, r: Result<String>| match r {
        Ok(v) => println!("✓ {name}: {v}"),
        Err(e) => {
            ok = false;
            println!("✗ {name}: {e:#}");
        }
    };
    let cfg = config::Config::load(config);
    let (adb, maestro, serial) = match &cfg {
        Ok(c) => (c.device.adb.clone(), c.device.maestro.clone(), c.device.serial.clone()),
        Err(_) => ("adb".into(), config::expand_home("~/.maestro/maestro/bin/maestro"), None),
    };
    check("config", cfg.as_ref().map(|c| format!("{} → app repo {}", config.display(), c.app.repo.display())).map_err(|e| anyhow::anyhow!("{e:#}")));
    if let Ok(c) = &cfg {
        check("app repo", shell::check("git", &["rev-parse", "--abbrev-ref", "HEAD"], &c.app.repo).await.map(|b| format!("git, on {b}")));
    }

    let dev = device::Adb::resolve(&adb, serial.as_deref()).await;
    match &dev {
        Ok(d) => {
            let model = d.prop("ro.product.model").await.unwrap_or_default();
            let ver = d.prop("ro.build.version.release").await.unwrap_or_default();
            check("device", Ok(format!("{} ({model}, Android {ver})", d.serial)));
            let lp = d.run(&["shell", "pm", "list", "packages", "dev.mobile.maestro"]).await.map(|o| o.stdout).unwrap_or_default();
            check(
                "maestro driver on device",
                if lp.contains("dev.mobile.maestro") { Ok("installed".into()) } else { Ok("not installed yet (installed on first maestro run)".into()) },
            );
            if let Ok(c) = &cfg {
                let p = d.run(&["shell", "pm", "path", &c.app.package]).await.map(|o| o.stdout).unwrap_or_default();
                check("app on device", Ok(if p.trim().is_empty() { format!("{} not installed", c.app.package) } else { "installed".into() }));
            }
        }
        Err(e) => check("device", Err(anyhow::anyhow!("{e:#}"))),
    }
    check("maestro", shell::check(&maestro, &["--version"], Path::new(".")).await.map(|v| v.lines().last().unwrap_or("").to_string()));
    check("claude", shell::check("claude", &["--version"], Path::new(".")).await);
    check("gh", shell::check("gh", &["auth", "status", "--active"], Path::new(".")).await.map(|_| "authenticated".into()));
    check("npx (mobile-mcp)", shell::check("npx", &["--version"], Path::new(".")).await);
    if let Ok(c) = &cfg {
        check("github repo", shell::check("gh", &["repo", "view", &c.github.repo, "--json", "nameWithOwner", "-q", ".nameWithOwner"], Path::new(".")).await);
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
