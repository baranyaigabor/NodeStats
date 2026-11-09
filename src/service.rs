use anyhow::{Context, Result, bail};
use std::{path::Path, process::Command};

#[cfg(unix)]
mod unix;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::unregister as unregister_windows_startup;

#[derive(Clone, Copy)]
pub enum Action {
    Status,
    Start,
    Stop,
}

pub enum Manager {
    #[cfg(target_os = "macos")]
    Launchd,
    #[cfg(target_os = "linux")]
    Systemd,
    #[cfg(target_os = "linux")]
    OpenRc,
    #[cfg(target_os = "windows")]
    TaskScheduler,
}

impl Manager {
    pub fn detect() -> Result<Self> {
        if matches!(crate::platform_name(), "docker" | "kubernetes") {
            bail!(
                "Use the container restart policy or Kubernetes Deployment to manage automatic startup."
            );
        }
        #[cfg(target_os = "macos")]
        return Ok(Self::Launchd);
        #[cfg(target_os = "windows")]
        return Ok(Self::TaskScheduler);
        #[cfg(target_os = "linux")]
        {
            if Path::new("/run/systemd/system").is_dir() {
                return Ok(Self::Systemd);
            }
            if Path::new("/run/openrc").is_dir() {
                return Ok(Self::OpenRc);
            }
        }
        #[allow(unreachable_code)]
        {
            bail!(
                "Automatic startup requires launchd, Windows Task Scheduler, systemd, or OpenRC. Use nodestats run with your system's supervisor."
            );
        }
    }

    pub fn check_setup(&self) -> Result<()> {
        #[cfg(unix)]
        unix::check_setup()?;
        Ok(())
    }

    pub fn install(&self, state: &Path) -> Result<()> {
        let executable = std::env::current_exe().context("Locating the NodeStats executable")?;
        #[cfg(unix)]
        return unix::install(self, &executable, state);
        #[cfg(target_os = "windows")]
        return windows::install(&executable, state);
    }

    pub fn control(&self, action: Action) -> Result<()> {
        #[cfg(unix)]
        return unix::control(self, action);
        #[cfg(target_os = "windows")]
        return windows::control(action);
    }
}

fn checked(command: &mut Command, operation: &str) -> Result<()> {
    let status = command.status().with_context(|| operation.to_owned())?;
    if !status.success() {
        bail!("{operation} failed ({status}).");
    }
    Ok(())
}
