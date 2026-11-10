use crate::shell::{self, Out};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Adb {
    pub adb: String,
    pub serial: String,
}

const T: Duration = Duration::from_secs(120);

impl Adb {
    /// Use `serial` if given, otherwise the single connected device.
    pub async fn resolve(adb: &str, serial: Option<&str>) -> Result<Self> {
        let out = shell::check(adb, &["devices"], Path::new(".")).await?;
        let online: Vec<&str> = out
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once('\t'))
            .filter(|(_, state)| *state == "device")
            .map(|(s, _)| s)
            .collect();
        let serial = match serial {
            Some(s) if online.contains(&s) => s.to_string(),
            Some(s) => bail!("device {s} is not connected/authorized (online: {online:?})"),
            None => match online.as_slice() {
                [one] => one.to_string(),
                [] => bail!("no Android device connected"),
                many => bail!("several devices connected, set device.serial: {many:?}"),
            },
        };
        Ok(Self { adb: adb.to_string(), serial })
    }

    pub async fn run(&self, args: &[&str]) -> Result<Out> {
        let mut full = vec!["-s", self.serial.as_str()];
        full.extend_from_slice(args);
        shell::exec(&self.adb, &full, Path::new("."), T).await
    }

    pub async fn prop(&self, name: &str) -> Result<String> {
        Ok(self.run(&["shell", "getprop", name]).await?.stdout.trim().to_string())
    }

    pub async fn install(&self, apk: &Path) -> Result<Out> {
        self.run(&["install", "-r", "-t", "-g", &apk.to_string_lossy()]).await
    }

    /// MIUI/HyperOS asks the user to confirm the *first* USB install of a package
    /// (updates via `install -r` are silent afterwards). Keep retrying while the
    /// user confirms on the phone, up to `wait`.
    pub async fn install_confirmed(&self, apk: &Path, wait: Duration) -> Result<Out> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let o = self.install(apk).await?;
            let restricted = o.tail(4000).contains("INSTALL_FAILED_USER_RESTRICTED");
            if o.ok || !restricted || std::time::Instant::now() >= deadline {
                return Ok(o);
            }
            tracing::warn!("install needs confirmation: tap «Install» in the dialog on the phone");
        }
    }

    pub async fn launch(&self, pkg: &str) -> Result<Out> {
        self.run(&["shell", "monkey", "-p", pkg, "-c", "android.intent.category.LAUNCHER", "1"]).await
    }

    pub async fn stop(&self, pkg: &str) -> Result<Out> {
        self.run(&["shell", "am", "force-stop", pkg]).await
    }

    pub async fn clear(&self, pkg: &str) -> Result<Out> {
        self.run(&["shell", "pm", "clear", pkg]).await
    }

    /// Recent logcat lines; restricted to the app's process when `pkg` is given and running.
    pub async fn logcat(&self, lines: u32, pkg: Option<&str>) -> Result<String> {
        let n = lines.to_string();
        let mut args = vec!["logcat", "-d", "-v", "time", "-t", n.as_str()];
        let pid;
        if let Some(pkg) = pkg {
            pid = self.run(&["shell", "pidof", pkg]).await?.stdout.trim().to_string();
            if pid.is_empty() {
                return Ok(format!("(process {pkg} is not running; use app_only=false for full log)"));
            }
            args.extend(["--pid", pid.as_str()]);
        }
        Ok(self.run(&args).await?.stdout)
    }

    pub async fn logcat_clear(&self) -> Result<Out> {
        self.run(&["logcat", "-c"]).await
    }

    pub async fn crash_log(&self) -> Result<String> {
        Ok(self.run(&["logcat", "-d", "-b", "crash", "-v", "time"]).await?.stdout)
    }

    /// Only one UiAutomation client may exist on the device. mobile-mcp keeps its
    /// on-device server (mobilecli DeviceServer) alive for 30 min, and the Maestro
    /// driver lingers after the CLI exits — so hand UiAutomation over explicitly.
    pub async fn handover_to_maestro(&self) {
        let _ = self.run(&["shell", "pkill", "-f", "com.mobilenext.mobilecli.DeviceServer"]).await;
    }

    pub async fn release_maestro(&self) {
        let _ = self.run(&["shell", "am", "force-stop", "dev.mobile.maestro.test"]).await;
        let _ = self.run(&["shell", "am", "force-stop", "dev.mobile.maestro"]).await;
    }

    pub async fn maestro_driver_installed(&self) -> bool {
        let out = self.run(&["shell", "pm", "list", "packages", "dev.mobile.maestro"]).await.map(|o| o.stdout).unwrap_or_default();
        let has = |p: &str| out.lines().any(|l| l.trim() == format!("package:{p}"));
        has("dev.mobile.maestro") && has("dev.mobile.maestro.test")
    }

    pub async fn screenshot(&self, dest: &Path) -> Result<()> {
        let remote = "/sdcard/harness-shot.png";
        let o = self.run(&["shell", "screencap", "-p", remote]).await?;
        anyhow::ensure!(o.ok, "screencap failed: {}", o.tail(500));
        let o = self.run(&["pull", remote, &dest.to_string_lossy()]).await?;
        anyhow::ensure!(o.ok, "pull failed: {}", o.tail(500));
        let _ = self.run(&["shell", "rm", remote]).await;
        Ok(())
    }
}

/// Exclusive, cross-process lease on a physical device.
/// Held for the whole time an agent or test run touches the phone.
pub struct Lease {
    _file: File,
}

pub async fn acquire_lease(serial: &str, holder: &str) -> Result<Lease> {
    let dir = PathBuf::from(crate::config::expand_home("~/.harness/locks"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{serial}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let file = match file.try_lock() {
        Ok(()) => file,
        Err(std::fs::TryLockError::WouldBlock) => {
            let owner = std::fs::read_to_string(&path).unwrap_or_default();
            tracing::info!("device {serial} busy ({}), waiting…", owner.trim());
            tokio::task::spawn_blocking(move || file.lock().map(|_| file)).await??
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    };
    std::fs::write(&path, format!("{holder} pid={}\n", std::process::id()))?;
    Ok(Lease { _file: file })
}
