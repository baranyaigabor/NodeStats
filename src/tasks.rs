use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::collections::HashMap;
use std::{env, ffi::OsString, path::Path, process};
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind,
};

const MAX_JS_SAFE_INT: u64 = (1 << 53) - 1;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProcess {
    pid: u32,
    started: String,
    name: String,
    cpu: f32,
    memory: u64,
    can_quit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_percent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    virtual_memory: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_disk_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_disk_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

#[derive(Deserialize)]
pub struct TaskCommand {
    pub id: String,
    pid: u32,
    started: String,
}

#[derive(Serialize)]
pub struct TaskResult {
    pub id: String,
    pub status: &'static str,
}

pub struct Tasks {
    system: System,
    own_pid: Pid,
    can_signal: bool,
    total_memory: u64,
    #[cfg(unix)]
    users: HashMap<u32, String>,
}

fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .without_tasks()
        .with_user(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
}

fn sanitize_text(input: &str, max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let cleaned: String = input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let count = trimmed.chars().count();
    if count <= max_chars {
        return Some(trimmed.to_string());
    }
    if max_chars >= 4 {
        let mut out: String = trimmed.chars().take(max_chars - 1).collect();
        out.push('…');
        Some(out)
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

fn extract_app_name(exe: Option<&Path>) -> Option<String> {
    let bundle = exe?
        .ancestors()
        .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
        .last()?;
    let stem = bundle.file_stem()?.to_string_lossy();
    sanitize_text(&stem, 120)
}

fn format_status(status: ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Run => "Running",
        ProcessStatus::Sleep => "Sleeping",
        ProcessStatus::Idle => "Idle",
        ProcessStatus::Stop | ProcessStatus::Tracing => "Stopped",
        ProcessStatus::Zombie | ProcessStatus::Dead => "Zombie",
        ProcessStatus::Wakekill | ProcessStatus::Waking => "Waking",
        ProcessStatus::Parked => "Parked",
        ProcessStatus::LockBlocked | ProcessStatus::UninterruptibleDiskSleep => "Waiting",
        ProcessStatus::Unknown(_) => "Unknown",
    }
}

fn is_sensitive_key(raw: &str) -> bool {
    let key = raw.trim_start_matches('-').to_ascii_lowercase();
    matches!(
        key.as_str(),
        "password"
            | "passwd"
            | "passphrase"
            | "token"
            | "secret"
            | "api-key"
            | "apikey"
            | "api_key"
            | "access-key"
            | "access_key"
            | "secret-key"
            | "secret_key"
            | "private-key"
            | "private_key"
            | "auth"
            | "authorization"
            | "bearer"
            | "cookie"
            | "credential"
            | "credentials"
            | "dsn"
            | "connection-string"
            | "connection_string"
    ) || key.ends_with("_password")
        || key.ends_with("_passwd")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_key")
        || key.ends_with("-password")
        || key.ends_with("-token")
        || key.ends_with("-secret")
}

fn redact_url_userinfo(arg: &str) -> String {
    let Some(scheme_pos) = arg.find("://") else {
        return arg.to_string();
    };
    let rest = &arg[scheme_pos + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if let Some(at_pos) = authority.rfind('@') {
        let mut out = String::with_capacity(arg.len());
        out.push_str(&arg[..scheme_pos + 3]);
        out.push_str("[REDACTED]");
        out.push_str(&rest[at_pos..]);
        out
    } else {
        arg.to_string()
    }
}

fn redact_command(args: &[OsString]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(args.len().min(64));
    let mut redact_next = false;
    for raw in args.iter().take(64) {
        let arg = raw.to_string_lossy();
        if redact_next {
            parts.push(String::from("[REDACTED]"));
            redact_next = false;
            continue;
        }
        if let Some((key, _val)) = arg.split_once('=')
            && is_sensitive_key(key)
        {
            parts.push(format!("{key}=[REDACTED]"));
            continue;
        }
        if arg.starts_with("--") && is_sensitive_key(&arg) {
            parts.push(arg.into_owned());
            redact_next = true;
            continue;
        }
        parts.push(redact_url_userinfo(&arg));
    }
    sanitize_text(&parts.join(" "), 260)
}

#[cfg(unix)]
fn lookup_username(uid: u32) -> Option<String> {
    use std::ffi::{CStr, c_char};
    use std::mem::MaybeUninit;
    use std::ptr;

    let mut pwd = MaybeUninit::<libc::passwd>::uninit();
    let initial = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buf_len = usize::try_from(initial)
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(1024)
        .min(16_384);
    let mut result: *mut libc::passwd = ptr::null_mut();
    let mut buf = vec![0u8; buf_len];
    loop {
        let rc = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf_len < 65_536 {
            buf_len *= 2;
            buf.resize(buf_len, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        let pwd = unsafe { pwd.assume_init() };
        if pwd.pw_name.is_null() {
            return None;
        }
        let name = unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy();
        return sanitize_text(&name, 64);
    }
}

impl Tasks {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_memory();
        let total_memory = system.total_memory();
        Self {
            system,
            own_pid: Pid::from_u32(process::id()),
            can_signal: can_signal(),
            total_memory,
            #[cfg(unix)]
            users: HashMap::new(),
        }
    }

    pub fn snapshot(&mut self) -> Vec<TaskProcess> {
        if self.total_memory == 0 {
            self.system.refresh_memory();
            self.total_memory = self.system.total_memory();
        }
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            refresh_kind()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                .with_cmd(UpdateKind::OnlyIfNotSet),
        );
        let can_signal = self.can_signal;
        let own_pid = self.own_pid;
        let total_memory = self.total_memory;

        let mut sorted_entries: Vec<(TaskProcess, Option<u32>, Option<String>)> = self
            .system
            .processes()
            .values()
            .filter(|process| (1..=i32::MAX as u32).contains(&process.pid().as_u32()))
            .map(|process| {
                let pid = process.pid().as_u32();
                let identity = can_quit_in(&self.system, own_pid, can_signal, process)
                    .then(|| platform::identity(pid))
                    .flatten();
                let name = sanitize_text(&process.name().to_string_lossy(), 80)
                    .unwrap_or_else(|| format!("Process {pid}"));
                let cpu = process.cpu_usage();
                let memory = process.memory().min(MAX_JS_SAFE_INT);
                let virtual_memory = process.virtual_memory().min(MAX_JS_SAFE_INT);
                let memory_percent = (total_memory > 0).then(|| {
                    let pct = ((memory as f64 / total_memory as f64) * 1000.0).round() / 10.0;
                    (pct as f32).clamp(0.0, 100.0)
                });
                let disk = process.disk_usage();
                let exe_path = process.exe();
                let app_name = extract_app_name(exe_path);
                let exe = exe_path.and_then(|path| sanitize_text(&path.to_string_lossy(), 240));
                let command = redact_command(process.cmd());
                #[cfg(unix)]
                let uid_num = process.user_id().map(|uid| **uid);
                #[cfg(not(unix))]
                let uid_num: Option<u32> = None;
                #[cfg(unix)]
                let uid_raw: Option<String> = None;
                #[cfg(not(unix))]
                let uid_raw = process
                    .user_id()
                    .and_then(|uid| sanitize_text(&format!("{uid:?}"), 64));
                let entry = TaskProcess {
                    pid,
                    can_quit: identity.is_some(),
                    started: identity.unwrap_or_else(|| process.start_time()).to_string(),
                    name,
                    cpu: if cpu.is_finite() { cpu.max(0.) } else { 0. },
                    memory,
                    app_name,
                    status: Some(format_status(process.status())),
                    user: None,
                    parent_pid: process
                        .parent()
                        .map(|p| p.as_u32())
                        .filter(|&p| p <= i32::MAX as u32),
                    memory_percent,
                    virtual_memory: Some(virtual_memory),
                    disk_read: Some(disk.read_bytes.min(MAX_JS_SAFE_INT)),
                    disk_written: Some(disk.written_bytes.min(MAX_JS_SAFE_INT)),
                    total_disk_read: Some(disk.total_read_bytes.min(MAX_JS_SAFE_INT)),
                    total_disk_written: Some(disk.total_written_bytes.min(MAX_JS_SAFE_INT)),
                    run_time: Some(process.run_time().min(MAX_JS_SAFE_INT)),
                    exe,
                    command,
                };
                (entry, uid_num, uid_raw)
            })
            .collect();

        sorted_entries.sort_unstable_by(|(a, _, _), (b, _, _)| {
            b.cpu
                .total_cmp(&a.cpu)
                .then(b.memory.cmp(&a.memory))
                .then(a.pid.cmp(&b.pid))
        });
        sorted_entries.truncate(200);

        sorted_entries
            .into_iter()
            .map(|(mut entry, uid_num, uid_raw)| {
                #[cfg(unix)]
                if let Some(numeric) = uid_num {
                    let resolved = if let Some(cached) = self.users.get(&numeric) {
                        Some(cached.clone())
                    } else if let Some(name) =
                        lookup_username(numeric).or_else(|| Some(numeric.to_string()))
                    {
                        if self.users.len() < 1024 {
                            self.users.insert(numeric, name.clone());
                        }
                        Some(name)
                    } else {
                        None
                    };
                    entry.user = resolved;
                }
                #[cfg(not(unix))]
                {
                    let _ = uid_num;
                    entry.user = uid_raw;
                }
                let _ = &uid_raw;
                entry
            })
            .collect()
    }

    pub fn force_quit(&self, task: TaskCommand, reported: &[TaskProcess]) -> TaskResult {
        let status = self.execute(&task, reported);
        TaskResult {
            id: task.id,
            status,
        }
    }
    fn execute(&self, task: &TaskCommand, reported: &[TaskProcess]) -> &'static str {
        if !reported
            .iter()
            .any(|entry| entry.pid == task.pid && entry.started == task.started && entry.can_quit)
        {
            return "unavailable";
        }
        let Ok(expected) = task.started.parse::<u64>() else {
            return "unavailable";
        };
        if platform::identity(task.pid) != Some(expected) {
            return "unavailable";
        }
        let pid = Pid::from_u32(task.pid);
        let mut fresh = System::new();
        fresh.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind());
        if fresh.process(pid).is_some_and(|process| {
            can_quit_in(&self.system, self.own_pid, self.can_signal, process)
        }) {
            platform::force_quit(task.pid, expected)
        } else {
            "unavailable"
        }
    }
}

fn can_quit_in(system: &System, own_pid: Pid, can_signal: bool, target: &Process) -> bool {
    if !can_signal {
        return false;
    }
    let Some(ours) = system.process(own_pid) else {
        return false;
    };
    let Some(uid) = ours.user_id() else {
        return false;
    };
    #[cfg(unix)]
    if target.effective_user_id() != Some(uid) {
        return false;
    }
    if target.pid().as_u32() <= 1 || target.user_id() != Some(uid) {
        return false;
    }
    if std::iter::successors(Some(own_pid), |pid| {
        system.process(*pid).and_then(Process::parent)
    })
    .take(128)
    .any(|pid| target.pid() == pid)
    {
        return false;
    }
    let Some(exe) = target.exe().filter(|path| path.is_absolute()) else {
        return false;
    };
    !protected_executable(exe)
}

fn can_signal() -> bool {
    #[cfg(unix)]
    return unsafe { libc::getuid() != 0 && libc::geteuid() == libc::getuid() };
    #[cfg(windows)]
    return platform::unelevated();
    #[cfg(not(any(unix, windows)))]
    return false;
}

fn protected_executable(exe: &Path) -> bool {
    if env::current_exe().is_ok_and(|own| {
        own == exe
            || own
                .ancestors()
                .find(|path| path.extension().is_some_and(|ext| ext == "app"))
                .is_some_and(|bundle| exe.starts_with(bundle))
    }) {
        return true;
    }
    #[cfg(target_os = "macos")]
    return exe.starts_with("/System") || exe.starts_with("/usr/libexec");
    #[cfg(target_os = "windows")]
    return {
        let path = exe.to_string_lossy().to_ascii_lowercase();
        let system_root = env::var("SystemRoot").unwrap_or_else(|_| String::from(r"C:\Windows"));
        let system_root = system_root.to_ascii_lowercase();
        path.trim_start_matches(r"\\?\")
            .strip_prefix(&system_root)
            .is_some_and(|remainder| remainder.starts_with('\\'))
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return false;
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        fs,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };

    pub fn identity(pid: u32) -> Option<u64> {
        // The process name can contain spaces and parentheses. Fields after the
        // final parenthesis begin with process state, which is field 3.
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_ascii_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }

    pub fn force_quit(pid: u32, expected: u64) -> &'static str {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            return "failed";
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if identity(pid) != Some(expected) {
            return "unavailable";
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if result == 0 { "stopped" } else { "failed" }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn identity(pid: u32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let written = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        info.pbi_start_tvsec
            .checked_mul(1_000_000)?
            .checked_add(info.pbi_start_tvusec)
    }

    pub fn force_quit(pid: u32, expected: u64) -> &'static str {
        if identity(pid) != Some(expected) {
            return "unavailable";
        }
        if unsafe { libc::kill(pid as i32, libc::SIGKILL) } == 0 {
            "stopped"
        } else {
            "failed"
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{
            GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
        },
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    pub fn unelevated() -> bool {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let token = Handle(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut written = 0;
        let result = unsafe {
            GetTokenInformation(
                token.0,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut written,
            )
        };
        result != 0 && elevation.TokenIsElevated == 0
    }

    fn open(pid: u32, access: u32) -> Option<Handle> {
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then(|| Handle(handle))
    }
    fn started(handle: &Handle) -> Option<u64> {
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        (unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) }
            != 0)
            .then_some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    pub fn identity(pid: u32) -> Option<u64> {
        started(&open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
    }
    pub fn force_quit(pid: u32, expected: u64) -> &'static str {
        let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE) else {
            return "failed";
        };
        if started(&handle) != Some(expected) {
            return "unavailable";
        }
        if unsafe { TerminateProcess(handle.0, 1) } != 0 {
            "stopped"
        } else {
            "failed"
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    pub fn identity(_: u32) -> Option<u64> {
        None
    }
    pub fn force_quit(_: u32, _: u64) -> &'static str {
        "unavailable"
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn rejects_unreported_and_changed_processes_and_can_stop_a_reported_child() {
        let executable = if cfg!(target_os = "macos") {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let Ok(mut child) = std::process::Command::new(executable).arg("30").spawn() else {
            return;
        };
        let mut manager = Tasks::new();
        let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            manager.snapshot();
            let started = platform::identity(child.id()).unwrap().to_string();
            let make_command = |started| TaskCommand {
                id: String::from("test"),
                pid: child.id(),
                started,
            };
            let entry = TaskProcess {
                pid: child.id(),
                started: started.clone(),
                name: String::from("sleep"),
                cpu: 0.0,
                memory: 0,
                can_quit: true,
                app_name: None,
                status: Some("Sleeping"),
                user: None,
                parent_pid: None,
                memory_percent: None,
                virtual_memory: None,
                disk_read: None,
                disk_written: None,
                total_disk_read: None,
                total_disk_written: None,
                run_time: None,
                exe: None,
                command: None,
            };
            assert_eq!(
                manager
                    .force_quit(make_command(started.clone()), &[])
                    .status,
                "unavailable"
            );
            assert_eq!(
                manager
                    .force_quit(
                        make_command(String::from("0")),
                        std::slice::from_ref(&entry)
                    )
                    .status,
                "unavailable"
            );
            if manager.can_signal {
                assert_eq!(
                    manager.force_quit(make_command(started), &[entry]).status,
                    "stopped"
                );
            }
        }));
        let _ = child.kill();
        let _ = child.wait();
        if let Err(failure) = check {
            std::panic::resume_unwind(failure);
        }
    }

    #[test]
    fn extracts_app_name_redacts_command_secrets_and_populates_snapshot_fields() {
        assert_eq!(
            extract_app_name(Some(Path::new(
                "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper"
            )))
            .as_deref(),
            Some("Visual Studio Code")
        );
        assert_eq!(extract_app_name(Some(Path::new("/usr/bin/sleep"))), None);

        let args = [
            OsString::from("/usr/bin/app"),
            OsString::from("--token=super-secret"),
            OsString::from("--password"),
            OsString::from("hunter2"),
            OsString::from("https://admin:pass@db.local/app"),
            OsString::from("--port"),
            OsString::from("8080"),
        ];
        assert_eq!(
            redact_command(&args).as_deref(),
            Some(
                "/usr/bin/app --token=[REDACTED] --password [REDACTED] https://[REDACTED]@db.local/app --port 8080"
            )
        );

        let mut manager = Tasks::new();
        let snapshot = manager.snapshot();
        assert!(!snapshot.is_empty());
        assert!(
            snapshot
                .iter()
                .any(|item| item.status.is_some() && item.run_time.is_some())
        );
    }
}
