use super::{Action, Manager, checked};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const EXECUTABLE: &str = "/usr/local/bin/nodestats";
const PACKAGE_MANAGER_PATHS: &[&str] = &[
    "/opt/homebrew/bin/nodestats",
    "/usr/bin/nodestats",
    "/usr/local/bin/nodestats",
];

fn find_tool(candidates: &[&'static str]) -> Result<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
        .with_context(|| format!("Expected {}", candidates.join(" or ")))
}

fn sudo() -> Result<Command> {
    Ok(Command::new(find_tool(&["/usr/bin/sudo", "/bin/sudo"])?))
}

pub fn check_setup() -> Result<()> {
    // SAFETY: getuid/geteuid take no pointers and do not alter process state.
    if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 } {
        bail!(
            "Run nodestats setup as your regular user. It will request sudo for boot registration."
        );
    }
    let _ = sudo()?;
    Ok(())
}

fn managed_executable_path(executable: &Path, candidates: &[&Path]) -> Option<String> {
    if let Some(matched) = candidates
        .iter()
        .find(|candidate| *candidate == &executable)
    {
        return Some(matched.to_string_lossy().into_owned());
    }
    let canonical_exec = fs::canonicalize(executable).ok()?;
    for candidate in candidates {
        if fs::canonicalize(candidate).ok().as_ref() == Some(&canonical_exec) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn install_executable(executable: &Path) -> Result<String> {
    let candidates: Vec<&Path> = PACKAGE_MANAGER_PATHS.iter().map(Path::new).collect();
    if let Some(managed) = managed_executable_path(executable, &candidates) {
        return Ok(managed);
    }
    let install = find_tool(&["/usr/bin/install", "/bin/install"])?;
    if !Path::new("/usr/local/bin").is_dir() {
        checked(
            sudo()?.args([install, "-d", "-m", "0755", "/usr/local/bin"]),
            "Preparing the installation directory",
        )?;
    }
    checked(
        sudo()?
            .args([install, "-m", "0755"])
            .arg(executable)
            .arg(EXECUTABLE),
        "Installing the NodeStats executable",
    )?;
    Ok(EXECUTABLE.to_owned())
}

fn install_config(config: &[u8], destination: &str, mode: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(config)?;
    file.flush()?;
    let install = find_tool(&["/usr/bin/install", "/bin/install"])?;
    checked(
        sudo()?
            .args([install, "-m", mode])
            .arg(file.path())
            .arg(destination),
        "Installing the NodeStats service definition",
    )
}

fn username() -> Result<String> {
    let id = find_tool(&["/usr/bin/id", "/bin/id"])?;
    let output = Command::new(id).arg("-un").output()?;
    if !output.status.success() {
        bail!("Could not find the current operating-system user.");
    }
    let name = String::from_utf8(output.stdout)?.trim_end().to_owned();
    if name.is_empty() {
        bail!("The operating-system user has no name.");
    }
    Ok(name)
}

pub fn install(manager: &Manager, executable: &Path, state: &Path) -> Result<()> {
    let state = state
        .to_str()
        .context("Use a UTF-8 path for the agent state directory")?;
    let binary = install_executable(executable)?;
    match manager {
        #[cfg(target_os = "macos")]
        Manager::Launchd => install_launchd(&binary, state),
        #[cfg(target_os = "linux")]
        Manager::Systemd => install_systemd(&binary, state),
        #[cfg(target_os = "linux")]
        Manager::OpenRc => install_openrc(&binary, state),
    }
}

fn manage(tool: &'static str, args: &[&str], elevated: bool, operation: &str) -> Result<()> {
    let mut command = if elevated {
        let mut cmd = sudo()?;
        cmd.arg(tool);
        cmd
    } else {
        Command::new(tool)
    };
    checked(command.args(args), operation)
}

fn installed(tool: &str, args: &[&str]) -> Result<bool> {
    Ok(Command::new(tool)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

#[cfg(target_os = "macos")]
const PLIST: &str = "/Library/LaunchDaemons/com.nodestats.agent.plist";
#[cfg(target_os = "macos")]
const JOB: &str = "system/com.nodestats.agent";
#[cfg(target_os = "macos")]
const LAUNCHCTL: &str = "/bin/launchctl";

#[cfg(target_os = "macos")]
fn install_launchd(binary: &str, state: &str) -> Result<()> {
    use plist::{Dictionary, Value};
    let config: Dictionary = [
        ("Label".to_owned(), "com.nodestats.agent".into()),
        ("UserName".to_owned(), username()?.into()),
        (
            "ProgramArguments".to_owned(),
            Value::Array(
                [binary, "run", "--managed", "--state-dir", state]
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            ),
        ),
        ("RunAtLoad".to_owned(), Value::Boolean(true)),
        ("KeepAlive".to_owned(), Value::Boolean(true)),
        ("ThrottleInterval".to_owned(), Value::Integer(30.into())),
    ]
    .into_iter()
    .collect();
    let mut xml = Vec::new();
    Value::Dictionary(config).to_writer_xml(&mut xml)?;
    install_config(&xml, PLIST, "0644")?;
    if installed(LAUNCHCTL, &["print", JOB])? {
        manage(
            LAUNCHCTL,
            &["bootout", JOB],
            true,
            "Stopping the previous NodeStats job",
        )?;
    }
    manage(
        LAUNCHCTL,
        &["enable", JOB],
        true,
        "Enabling NodeStats at boot",
    )?;
    manage(
        LAUNCHCTL,
        &["bootstrap", "system", PLIST],
        true,
        "Starting NodeStats",
    )
}

#[cfg(target_os = "linux")]
fn systemctl() -> Result<&'static str> {
    find_tool(&["/usr/bin/systemctl", "/bin/systemctl"])
}

#[cfg(target_os = "linux")]
fn systemd_argument(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_ascii_control() => quoted.push_str(&format!("\\x{:02x}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "linux")]
fn install_systemd(binary: &str, state: &str) -> Result<()> {
    // SAFETY: getuid has no arguments or side effects.
    let uid = unsafe { libc::getuid() };
    let state = systemd_argument(state);
    let service = format!(
        "[Unit]\nDescription=NodeStats agent\nAfter=network.target\n\n[Service]\nUser={uid}\nExecStart=:{binary} run --managed --state-dir {state}\nRestart=always\nRestartSec=30s\nNoNewPrivileges=yes\n\n[Install]\nWantedBy=multi-user.target\n"
    );
    install_config(
        service.as_bytes(),
        "/etc/systemd/system/nodestats.service",
        "0644",
    )?;
    let systemctl = systemctl()?;
    manage(systemctl, &["daemon-reload"], true, "Reloading systemd")?;
    manage(
        systemctl,
        &["enable", "nodestats.service"],
        true,
        "Enabling NodeStats at boot",
    )?;
    manage(
        systemctl,
        &["restart", "nodestats.service"],
        true,
        "Starting NodeStats",
    )
}

#[cfg(target_os = "linux")]
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn openrc_service() -> Result<&'static str> {
    find_tool(&[
        "/sbin/rc-service",
        "/usr/sbin/rc-service",
        "/usr/bin/rc-service",
    ])
}

#[cfg(target_os = "linux")]
fn install_openrc(binary: &str, state: &str) -> Result<()> {
    let openrc_run = find_tool(&["/sbin/openrc-run", "/usr/sbin/openrc-run"])?;
    let user = sh_quote(&username()?);
    let command_path = sh_quote(binary);
    let state = sh_quote(state);
    let service = format!(
        "#!{openrc_run}\nname=NodeStats\ncommand={command_path}\ncommand_user={user}\ncommand_args='run --managed'\nexport NODESTATS_STATE_DIR={state}\nsupervisor=supervise-daemon\nrespawn_delay=30\nrespawn_max=0\n"
    );
    install_config(service.as_bytes(), "/etc/init.d/nodestats", "0755")?;
    let update = find_tool(&[
        "/sbin/rc-update",
        "/usr/sbin/rc-update",
        "/usr/bin/rc-update",
    ])?;
    manage(
        update,
        &["add", "nodestats", "default"],
        true,
        "Enabling NodeStats at boot",
    )?;
    let rc = openrc_service()?;
    let verb = if installed(rc, &["nodestats", "status"])? {
        "restart"
    } else {
        "start"
    };
    manage(rc, &["nodestats", verb], true, "Starting NodeStats")
}

pub fn control(manager: &Manager, action: Action) -> Result<()> {
    let (_verb, operation) = match action {
        Action::Status => ("status", "Checking the NodeStats service"),
        Action::Start => ("start", "Starting NodeStats"),
        Action::Stop => ("stop", "Stopping NodeStats"),
    };
    match manager {
        #[cfg(target_os = "macos")]
        Manager::Launchd => match action {
            Action::Status => manage(LAUNCHCTL, &["print", JOB], false, operation),
            Action::Start if installed(LAUNCHCTL, &["print", JOB])? => {
                manage(LAUNCHCTL, &["kickstart", JOB], true, operation)
            }
            Action::Start => manage(LAUNCHCTL, &["bootstrap", "system", PLIST], true, operation),
            Action::Stop => manage(LAUNCHCTL, &["bootout", JOB], true, operation),
        },
        #[cfg(target_os = "linux")]
        Manager::Systemd => manage(
            systemctl()?,
            &["--no-pager", _verb, "nodestats.service"],
            !matches!(action, Action::Status),
            operation,
        ),
        #[cfg(target_os = "linux")]
        Manager::OpenRc => manage(
            openrc_service()?,
            &["nodestats", _verb],
            !matches!(action, Action::Status),
            operation,
        ),
    }
}

#[cfg(test)]
mod common_tests {
    use super::*;

    #[test]
    fn managed_executable_path_prefers_package_manager_locations() {
        let dir = tempfile::tempdir().unwrap();
        let cellar_bin = dir.path().join("cellar-nodestats");
        fs::write(&cellar_bin, b"").unwrap();
        let homebrew_link = dir.path().join("homebrew-nodestats");
        std::os::unix::fs::symlink(&cellar_bin, &homebrew_link).unwrap();

        let candidates = [
            Path::new("/opt/homebrew/bin/nodestats"),
            Path::new("/usr/bin/nodestats"),
            Path::new("/usr/local/bin/nodestats"),
            homebrew_link.as_path(),
        ];

        assert_eq!(
            managed_executable_path(Path::new("/opt/homebrew/bin/nodestats"), &candidates),
            Some("/opt/homebrew/bin/nodestats".to_owned())
        );
        assert_eq!(
            managed_executable_path(Path::new("/usr/bin/nodestats"), &candidates),
            Some("/usr/bin/nodestats".to_owned())
        );
        assert_eq!(
            managed_executable_path(&cellar_bin, &candidates),
            Some(homebrew_link.to_string_lossy().into_owned())
        );
        assert_eq!(
            managed_executable_path(Path::new("/tmp/downloads/nodestats"), &candidates),
            None
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn service_manager_treats_state_paths_as_data() {
        assert_eq!(
            systemd_argument(r#"/home/A $ B%20/"quoted""#),
            r#""/home/A $ B%%20/\"quoted\"""#
        );
        assert_eq!(
            sh_quote("/home/O'Brian/$(false)"),
            "'/home/O'\\''Brian/$(false)'"
        );
    }
}
