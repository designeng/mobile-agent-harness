//! `harness mcp` — a small stdio MCP server that gives agents app-lifecycle,
//! logs, evidence and Maestro on the leased device. UI interaction itself
//! (tap/swipe/type/screen reading) comes from mobile-mcp.

use crate::device::Adb;
use crate::shell;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct McpArgs {
    pub serial: Option<String>,
    pub adb: String,
    pub maestro: String,
    pub package: String,
    pub apk: PathBuf,
    pub flows_dir: PathBuf,
    pub artifacts: PathBuf,
}

#[derive(Clone)]
pub struct DeviceServer {
    adb: Adb,
    a: McpArgs,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LogcatReq {
    /// Number of most recent lines (default 200).
    pub lines: Option<u32>,
    /// Only the app's process (default true).
    pub app_only: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ShotReq {
    /// Short evidence name, e.g. "login-error". Saved as PNG in the run artifacts.
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct FlowReq {
    /// Flow file path, relative to the flows dir (e.g. "login.yaml") or absolute.
    pub flow: String,
}

fn r(o: anyhow::Result<shell::Out>) -> Result<String, String> {
    match o {
        Ok(o) if o.ok => Ok(o.tail(4000)),
        Ok(o) => Err(o.tail(4000)),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

#[tool_router(router = tool_router)]
impl DeviceServer {
    pub fn new(adb: Adb, a: McpArgs) -> Self {
        Self { adb, a, tool_router: Self::tool_router() }
    }

    #[tool(description = "Install (or reinstall) the freshly built app APK on the device.")]
    async fn app_install(&self) -> Result<String, String> {
        r(self.adb.install(&self.a.apk).await)
    }

    #[tool(description = "Launch the app under test from its launcher activity.")]
    async fn app_launch(&self) -> Result<String, String> {
        r(self.adb.launch(&self.a.package).await)
    }

    #[tool(description = "Force-stop the app under test.")]
    async fn app_stop(&self) -> Result<String, String> {
        r(self.adb.stop(&self.a.package).await)
    }

    #[tool(description = "Clear the app's data (fresh-install state). Stops the app.")]
    async fn app_clear_data(&self) -> Result<String, String> {
        r(self.adb.clear(&self.a.package).await)
    }

    #[tool(description = "Read recent logcat lines, by default only from the app process.")]
    async fn logcat(&self, Parameters(p): Parameters<LogcatReq>) -> Result<String, String> {
        let pkg = p.app_only.unwrap_or(true).then_some(self.a.package.as_str());
        self.adb.logcat(p.lines.unwrap_or(200).min(2000), pkg).await.map_err(|e| format!("{e:#}"))
    }

    #[tool(description = "Clear logcat buffers. Do this before reproducing a scenario.")]
    async fn logcat_clear(&self) -> Result<String, String> {
        r(self.adb.logcat_clear().await)
    }

    #[tool(description = "Read the crash buffer (uncaught exceptions, ANRs). Empty means no crashes.")]
    async fn crash_log(&self) -> Result<String, String> {
        let s = self.adb.crash_log().await.map_err(|e| format!("{e:#}"))?;
        // The crash buffer is device-wide; vendor ROMs fill it with noise. Keep only our app.
        let pkg = &self.a.package;
        let ours: Vec<&str> = s.split("FATAL EXCEPTION").skip(1).filter(|b| b.contains(pkg.as_str())).collect();
        let anr = self.adb.run(&["shell", "dumpsys", "activity", "anr"]).await.map(|o| o.stdout).unwrap_or_default();
        let mut out = String::new();
        for b in &ours {
            out.push_str("FATAL EXCEPTION");
            out.push_str(&b.chars().take(4000).collect::<String>());
            out.push('\n');
        }
        if anr.contains(pkg.as_str()) {
            out.push_str("ANR reported for the app (dumpsys activity anr)\n");
        }
        Ok(if out.is_empty() { format!("(no crashes or ANRs of {pkg})") } else { out })
    }

    #[tool(description = "Save a screenshot as evidence into the run artifacts and return its path.")]
    async fn save_screenshot(&self, Parameters(p): Parameters<ShotReq>) -> Result<String, String> {
        let dir = self.a.artifacts.join("screens");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let ts = chrono::Local::now().format("%H%M%S");
        let dest = dir.join(format!("{ts}-{}.png", slug(&p.name)));
        self.adb.screenshot(&dest).await.map_err(|e| format!("{e:#}"))?;
        Ok(dest.display().to_string())
    }

    #[tool(description = "Run one Maestro flow on the device and return pass/fail with output. \
        Use it to verify a flow you just wrote. Do not interact via mobile-mcp while it runs.")]
    async fn maestro_test(&self, Parameters(p): Parameters<FlowReq>) -> Result<String, String> {
        let flow = if Path::new(&p.flow).is_absolute() { PathBuf::from(&p.flow) } else { self.a.flows_dir.join(&p.flow) };
        run_maestro(&self.adb, &self.a.maestro, &flow, &self.a.artifacts).await
    }
}

/// Shared with the pipeline's regression gate.
pub async fn run_maestro(adb: &Adb, maestro: &str, flow: &Path, artifacts: &Path) -> Result<String, String> {
    let stem = flow.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "flows".into());
    let out_dir = artifacts.join("maestro").join(slug(&stem));
    let _ = std::fs::create_dir_all(&out_dir);
    let flow_s = flow.to_string_lossy();
    let out_s = out_dir.to_string_lossy();
    // Reinstalling the driver every run would need a manual confirmation on MIUI/HyperOS.
    let reinstall = if adb.maestro_driver_installed().await { "--no-reinstall-driver" } else { "--reinstall-driver" };
    adb.handover_to_maestro().await;
    let res = shell::exec(
        maestro,
        &["--device", &adb.serial, "test", reinstall, &flow_s, "--debug-output", &out_s],
        Path::new("."),
        Duration::from_secs(900),
    )
    .await;
    adb.release_maestro().await;
    match res {
        Ok(o) if o.ok => Ok(format!("PASS\n{}", o.tail(3000))),
        Ok(o) => Err(format!("FAIL (debug output: {out_s})\n{}", o.tail(5000))),
        Err(e) => Err(format!("{e:#}")),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DeviceServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("harness-device", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
            "Device {} / app {}. App lifecycle, logs, evidence screenshots and Maestro flows. \
             Use mobile-mcp for tapping, typing and reading the screen.",
            self.adb.serial, self.a.package
        ))
    }
}

pub async fn serve(a: McpArgs) -> anyhow::Result<()> {
    let adb = Adb::resolve(&a.adb, a.serial.as_deref()).await?;
    let service = DeviceServer::new(adb, a).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
