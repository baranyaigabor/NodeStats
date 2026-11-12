use super::{Action, checked};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn system_tool(relative: &str) -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("SystemRoot").context("Missing the Windows system directory")?)
            .join("System32")
            .join(relative),
    )
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn path_literal(path: &Path) -> Result<String> {
    Ok(literal(path.to_str().context(
        "Use a Unicode path for the NodeStats installation",
    )?))
}

fn encoded(script: &str) -> String {
    let utf16: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    STANDARD.encode(utf16)
}

fn elevate(script: &str, operation: &str) -> Result<()> {
    let powershell = system_tool(r"WindowsPowerShell\v1.0\powershell.exe")?;
    let launcher = format!(
        "$ErrorActionPreference = 'Stop'; try {{ \
          $child = Start-Process -FilePath {} -Verb RunAs -WindowStyle Hidden -Wait -PassThru \
          -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{}'; \
          exit $child.ExitCode \
          }} catch {{ Write-Error $_; exit 1 }}",
        path_literal(&powershell)?,
        encoded(script)
    );
    checked(
        Command::new(&powershell).args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded(&launcher),
        ]),
        operation,
    )
}

pub fn install(executable: &Path, state: &Path) -> Result<()> {
    let script = format!(
        "$sourceBinary = {}\n$sourceIdentity = {}\n$sourceAccess = {}\n{}",
        path_literal(executable)?,
        path_literal(&state.join("identity.json"))?,
        path_literal(&state.join("cloudflare-access.json"))?,
        include_str!("windows-setup.ps1"),
    );
    elevate(&script, "Registering the NodeStats startup task")
}

pub fn control(action: Action) -> Result<()> {
    let (verb, operation) = match action {
        Action::Status => ("/Query", "Checking the NodeStats startup task"),
        Action::Start => ("/Run", "Starting NodeStats"),
        Action::Stop => ("/End", "Stopping NodeStats"),
    };
    let scheduler = system_tool("schtasks.exe")?;
    if matches!(action, Action::Status) {
        return checked(
            Command::new(scheduler).args([verb, "/TN", "NodeStats"]),
            operation,
        );
    }
    elevate(
        &format!(
            "& {} {verb} /TN NodeStats; exit $LASTEXITCODE",
            path_literal(&scheduler)?
        ),
        operation,
    )
}

pub fn unregister() -> Result<()> {
    elevate(
        "$ErrorActionPreference = 'Stop';          $task = Get-ScheduledTask -TaskName NodeStats -ErrorAction SilentlyContinue;          if ($task) { if ($task.State -eq 'Running') { Stop-ScheduledTask -InputObject $task };          Unregister-ScheduledTask -InputObject $task -Confirm:$false }",
        "Removing automatic startup",
    )
}
