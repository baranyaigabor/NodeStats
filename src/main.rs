#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod metrics;
mod network;
mod platform;
mod service;
mod tasks;
mod update;
#[cfg(target_os = "windows")]
mod windows;
mod workload_control;
mod workloads;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use flate2::{Compression, write::GzEncoder};
use fs2::FileExt;
use metrics::Collector;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    io::{self, BufRead, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use sysinfo::System;
use time::OffsetDateTime;

#[derive(Parser)]
#[command(
    name = "nodestats",
    version,
    about = "Collect system metrics and send them to your paired NodeStats account"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    /// Pair once, then install and start the background agent at machine startup.
    Setup {
        #[arg(long, env = "NODESTATS_SERVER")]
        server: Option<String>,
        #[arg(long, env = "NODESTATS_NAME")]
        name: Option<String>,
        #[arg(long, env = "NODESTATS_STATE_DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show the background agent's status.
    Status,
    /// Start the configured background agent.
    Start,
    /// Stop the background agent until the next start or reboot.
    Stop,
    #[cfg(target_os = "windows")]
    #[command(hide = true)]
    UnregisterStartup,
    /// Report in the foreground for containers, troubleshooting, or hardware diagnostics.
    Run(RunOptions),
    /// Collect and print one local sample without pairing or uploading.
    Snapshot,
    /// Stream local samples and respond to process requests over standard I/O.
    #[command(hide = true)]
    DesktopLocal {
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        interval_ms: Option<u64>,
    },
}
#[derive(Args)]
struct RunOptions {
    #[arg(long, hide = true, conflicts_with_all = ["desktop_events", "allow_task_management", "allow_workload_management"])]
    managed: bool,
    #[arg(long, hide = true, requires = "managed")]
    skip_update: bool,
    #[arg(long, env = "NODESTATS_SERVER")]
    server: Option<String>,
    #[arg(long, env = "NODESTATS_NAME")]
    name: Option<String>,
    /// Compose project/service or Kubernetes namespace/workload.
    #[arg(long, env = "NODESTATS_WORKLOAD")]
    workload: Option<String>,
    #[arg(long, env = "NODESTATS_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Emit pairing and reporting events as JSON lines for the desktop app.
    #[arg(long, hide = true)]
    desktop_events: bool,
    #[arg(long, hide = true, requires = "desktop_events")]
    desktop_samples: bool,
    /// Share the process list and allow confirmed force quits for your own processes.
    #[arg(long)]
    allow_task_management: bool,
    /// Allow the paired account to run the preset Docker and Kubernetes actions and request recent logs.
    #[arg(long)]
    allow_workload_management: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    server: String,
    node_id: String,
    token: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessCredentials {
    server: String,
    client_id: String,
    client_secret: String,
}

impl AccessCredentials {
    fn load(dir: &Path) -> Result<Option<Self>> {
        let file = match fs::File::open(dir.join("cloudflare-access.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Opening Cloudflare Access credentials"),
        };
        let mut credentials: Self =
            serde_json::from_reader(file).context("Invalid Cloudflare Access credentials file")?;
        credentials.server = endpoint(&credentials.server)?;
        if !credentials.server.starts_with("https://")
            || [&credentials.client_id, &credentials.client_secret]
                .iter()
                .any(|value| value.is_empty() || value.bytes().any(|byte| !byte.is_ascii_graphic()))
        {
            bail!(
                "Cloudflare Access credentials require an HTTPS origin, client ID and client secret."
            );
        }
        Ok(Some(credentials))
    }
}

fn monitor_request(
    http: &ureq::Agent,
    server: &str,
    route: &str,
    access: Option<&AccessCredentials>,
) -> ureq::Request {
    let request = http.post(&format!("{server}/api/monitor/{route}"));
    match access.filter(|credentials| credentials.server == server) {
        Some(credentials) => request
            // Access can append its own cookie before proxying the request.
            .set("Origin", server)
            .set("CF-Access-Client-Id", &credentials.client_id)
            .set("CF-Access-Client-Secret", &credentials.client_secret),
        None => request,
    }
}

fn check_monitor_response(response: &ureq::Response) -> Result<()> {
    if (300..400).contains(&response.status()) {
        if response.header("Location").is_some_and(|location| {
            url::Url::parse(location)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| host.ends_with(".cloudflareaccess.com"))
        }) {
            bail!(
                "Cloudflare Access requires valid service credentials in cloudflare-access.json."
            );
        }
        bail!(
            "NodeStats server returned an unexpected redirect (HTTP {}).",
            response.status()
        );
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairStart {
    code: String,
    secret: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Approval {
    Pending,
    Approved {
        #[serde(rename = "nodeId")]
        node_id: String,
        token: String,
    },
}

fn endpoint(s: &str) -> Result<String> {
    server_origin(s, matches!(platform_name(), "docker" | "kubernetes"))
}
fn server_origin(s: &str, container: bool) -> Result<String> {
    let u = url::Url::parse(s).context("Invalid NodeStats server URL")?;
    if !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
    {
        bail!("Use the server origin, such as http://localhost:8081");
    }
    if !matches!(u.scheme(), "http" | "https") {
        bail!("Use an HTTP or HTTPS server URL");
    }
    if u.scheme() == "http"
        && !matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        && !(container && u.host_str() == Some("host.docker.internal"))
    {
        bail!("Use HTTPS for a remote server. HTTP is available for local development.");
    }
    Ok(u.origin().ascii_serialization())
}
fn platform_name() -> &'static str {
    if env::var_os("KUBERNETES_SERVICE_HOST").is_some() || env::var_os("POD_NAMESPACE").is_some() {
        "kubernetes"
    } else if Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || env::var_os("container").is_some()
    {
        "docker"
    } else {
        env::consts::OS
    }
}

#[cfg(test)]
const OVERLAY_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(10);
const MAX_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
const DESKTOP_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

fn clamp_sample_interval(interval_ms: u64) -> Duration {
    Duration::from_millis(interval_ms).clamp(MIN_SAMPLE_INTERVAL, MAX_SAMPLE_INTERVAL)
}

fn sample_interval(platform: &str, desktop_mode: bool, desktop_installed: bool) -> Duration {
    if matches!(platform, "macos" | "linux" | "windows") && (desktop_mode || desktop_installed) {
        DESKTOP_SAMPLE_INTERVAL
    } else {
        DEFAULT_SAMPLE_INTERVAL
    }
}

fn is_packaged_desktop_exe(exe: &Path) -> bool {
    let Some(collector_dir) = exe.parent() else {
        return false;
    };
    let Some(resources_dir) = collector_dir.parent() else {
        return false;
    };
    if !collector_dir
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("collector"))
        || !resources_dir
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("resources"))
    {
        return false;
    }
    if let Some(contents_dir) = resources_dir.parent()
        && contents_dir
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("contents"))
        && contents_dir.ancestors().skip(1).any(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
    {
        return true;
    }
    resources_dir.parent().is_some_and(|app_root| {
        ["NodeStats.exe", "nodestats", ".nodestats-portable"]
            .iter()
            .any(|marker| app_root.join(marker).exists())
    })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn has_nodestats_desktop_entry(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        name.ends_with(".desktop") && name.contains("nodestats")
    })
}

fn desktop_app_installed() -> bool {
    if !matches!(platform_name(), "macos" | "linux" | "windows") {
        return false;
    }
    if env::current_exe()
        .ok()
        .is_some_and(|exe| is_packaged_desktop_exe(&exe))
    {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        Path::new("/Applications/NodeStats.app").is_dir()
            || dirs::home_dir().is_some_and(|home| home.join("Applications/NodeStats.app").is_dir())
    }
    #[cfg(target_os = "windows")]
    {
        env::var_os("LOCALAPPDATA").is_some_and(|base| {
            Path::new(&base)
                .join("Programs/NodeStats/NodeStats.exe")
                .is_file()
        }) || ["ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(env::var_os)
            .any(|base| Path::new(&base).join("NodeStats/NodeStats.exe").is_file())
    }
    #[cfg(target_os = "linux")]
    {
        env::var_os("APPIMAGE").is_some()
            || Path::new("/opt/NodeStats").is_dir()
            || has_nodestats_desktop_entry(Path::new("/usr/share/applications"))
            || dirs::data_local_dir()
                .is_some_and(|dir| has_nodestats_desktop_entry(&dir.join("applications")))
            || dirs::config_dir()
                .is_some_and(|dir| has_nodestats_desktop_entry(&dir.join("autostart")))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}
fn workload_name(explicit: Option<String>) -> Option<String> {
    explicit
        .or_else(|| {
            let ns = env::var("POD_NAMESPACE").ok()?;
            let app = env::var("WORKLOAD_NAME")
                .or_else(|_| env::var("POD_NAME"))
                .ok()?;
            Some(format!("{ns}/{app}"))
        })
        .or_else(|| {
            Some(format!(
                "{}/{}",
                env::var("COMPOSE_PROJECT_NAME").ok()?,
                env::var("COMPOSE_SERVICE_NAME").ok()?
            ))
        })
}
fn node_name(provided: Option<String>) -> Result<String> {
    let default = System::host_name().unwrap_or_else(|| "Node".into());
    let name = if let Some(name) = provided {
        name
    } else if io::stdin().is_terminal() {
        eprint!("Node name [{default}]: ");
        io::stderr().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            default
        } else {
            input.to_owned()
        }
    } else {
        default
    };
    let name = name.trim();
    if name.is_empty() || name.encode_utf16().count() > 160 {
        bail!("Choose a node name between 1 and 160 characters.");
    }
    Ok(name.to_owned())
}

fn pairing_response<T: serde::de::DeserializeOwned>(response: ureq::Response) -> Result<T> {
    check_monitor_response(&response)?;
    // Broker messages are small. Bound the decoded body even with chunked transfer.
    const LIMIT: usize = 4096;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        bail!("Pairing response exceeded the protocol limit.");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn valid_pair_code(code: &str) -> bool {
    const ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    code.len() == 8 && code.chars().all(|ch| ALPHABET.contains(ch))
}

fn enroll(
    http: &ureq::Agent,
    server: &str,
    name: String,
    workload: Option<String>,
    access: Option<&AccessCredentials>,
    pending_path: Option<&Path>,
) -> Result<Identity> {
    let metadata = json!({"name":name,"platform":platform_name(),"workload":workload,"hostname":System::host_name(),
        "arch":env::consts::ARCH,"osVersion":System::long_os_version()});
    let pending = pending_path
        .and_then(|path| fs::File::open(path).ok())
        .and_then(|file| serde_json::from_reader::<_, PairStart>(file).ok())
        .filter(|pending| pending.expires_at > OffsetDateTime::now_utc());
    let start: PairStart = match pending {
        Some(pending) => pending,
        None => loop {
            match monitor_request(http, server, "pair/start", access).send_json(&metadata) {
                Ok(response) => break pairing_response(response)?,
                Err(ureq::Error::Transport(_) | ureq::Error::Status(429 | 500..=599, _))
                    if pending_path.is_some() =>
                {
                    thread::sleep(Duration::from_secs(5));
                }
                Err(error) => return Err(error.into()),
            }
        },
    };
    if !valid_pair_code(&start.code) {
        bail!("The server returned an invalid pairing code.");
    }
    if let Some(path) = pending_path {
        write_private(path, &start)?
            .persist(path)
            .context("Saving pending pairing")?;
        println!("{}", json!({"status":"pairing","code":start.code}));
    } else {
        anstream::println!(
            "Open {server}, sign in, choose Connect node, and enter \x1b[1;36m{}\x1b[0m",
            start.code
        );
    }
    io::stdout().flush()?;
    while OffsetDateTime::now_utc() < start.expires_at {
        thread::sleep(Duration::from_secs(2));
        let request = monitor_request(http, server, "pair/poll", access)
            .send_json(json!({"secret":start.secret}));
        match request {
            Ok(r) => match pairing_response(r)? {
                Approval::Pending => continue,
                Approval::Approved { node_id, token } => {
                    return Ok(Identity {
                        node_id,
                        token,
                        server: server.into(),
                    });
                }
            },
            Err(ureq::Error::Transport(_) | ureq::Error::Status(429 | 500..=599, _)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("Pairing code expired. Run the client again for a new code.")
}
fn write_private(path: &Path, value: &impl Serialize) -> Result<tempfile::NamedTempFile> {
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing state directory")?)?;
    serde_json::to_writer(&mut temp, value)?;
    temp.as_file().sync_all()?;
    Ok(temp)
}
fn resolve_state_dir(state: Option<PathBuf>) -> Result<PathBuf> {
    let path = state
        .or_else(|| dirs::config_local_dir().map(|p| p.join("NodeStats")))
        .context("Set NODESTATS_STATE_DIR to a writable private directory")?;
    Ok(std::path::absolute(path)?)
}

fn lock_state(dir: &Path) -> Result<fs::File> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("Opening {}", dir.display()))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("agent.lock"))?;
    lock.try_lock_exclusive()
        .context("NodeStats is already using this installation. Stop the running agent before starting another instance.")?;
    Ok(lock)
}

fn agent_http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(8))
        .build()
}

struct Enrollment<'a> {
    server: Option<String>,
    name: Option<String>,
    workload: Option<String>,
    pending: Option<&'a Path>,
    allow_pairing: bool,
    default_server: &'a str,
}

fn load_identity(
    dir: &Path,
    http: &ureq::Agent,
    access: Option<&AccessCredentials>,
    options: Enrollment<'_>,
) -> Result<Identity> {
    let supplied = options.server.as_deref().map(endpoint).transpose()?;
    let identity_path = dir.join("identity.json");
    match fs::File::open(&identity_path) {
        Ok(f) => {
            let mut saved: Identity =
                serde_json::from_reader(f).context("Reading paired identity")?;
            saved.server = endpoint(&saved.server)?;
            if supplied.as_ref().is_some_and(|s| s != &saved.server) {
                bail!(
                    "This installation is paired to a different server. Use a separate --state-dir."
                );
            }
            Ok(saved)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if !options.allow_pairing {
                bail!("The background agent has no saved pairing. Run nodestats setup first.");
            }
            let server = supplied.unwrap_or_else(|| options.default_server.into());
            let name = node_name(options.name)?;
            let identity = enroll(
                http,
                &server,
                name,
                workload_name(options.workload),
                access,
                options.pending,
            )?;
            write_private(&identity_path, &identity)?
                .persist_noclobber(&identity_path)
                .context("Saving paired identity")?;
            Ok(identity)
        }
        Err(e) => Err(e.into()),
    }
}

fn setup(server: Option<String>, name: Option<String>, state: Option<PathBuf>) -> Result<()> {
    let manager = service::Manager::detect()?;
    manager.check_setup()?;
    let dir = resolve_state_dir(state)?;
    anstream::println!("\x1b[1;36mNodeStats\x1b[0m  Background agent setup");
    {
        let _lock = lock_state(&dir)?;
        let access = AccessCredentials::load(&dir)?;
        let identity = load_identity(
            &dir,
            &agent_http(),
            access.as_ref(),
            Enrollment {
                server,
                name,
                workload: None,
                pending: None,
                allow_pairing: true,
                default_server: "https://nodestats.hu",
            },
        )?;
        anstream::println!("\x1b[32mPaired\x1b[0m to {}", identity.server);
    }
    anstream::println!(
        "Setting up automatic startup. Your operating system may ask for administrator approval."
    );
    manager.install(&dir)?;
    anstream::println!("\x1b[32mReady\x1b[0m  Background agent started and enabled at boot.");
    #[cfg(target_os = "windows")]
    println!("You can close this window. Use the NodeStats startup task to check the agent.");
    #[cfg(not(target_os = "windows"))]
    println!("You can close this terminal. Use nodestats status to check the agent.");
    Ok(())
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReportingSample {
    Native(metrics::Sample),
    Desktop(serde_json::Value),
}

struct Upload<'a> {
    http: &'a ureq::Agent,
    identity: &'a Identity,
    access: Option<&'a AccessCredentials>,
    authorization: String,
    body: Vec<u8>,
    compressed: Vec<u8>,
    gzip_enabled: bool,
}

impl Upload<'_> {
    fn send(&mut self) -> Result<ureq::Response, Box<ureq::Error>> {
        let request = || {
            monitor_request(self.http, &self.identity.server, "samples", self.access)
                .set("Authorization", &self.authorization)
                .set("Content-Type", "application/json")
        };
        if self.gzip_enabled && self.body.len() >= 1024 {
            self.compressed.clear();
            let mut encoder = GzEncoder::new(&mut self.compressed, Compression::fast());
            let encoded = encoder
                .write_all(&self.body)
                .and_then(|()| encoder.finish());
            if encoded.is_ok() && self.compressed.len() < self.body.len() {
                match request()
                    .set("Content-Encoding", "gzip")
                    .send_bytes(&self.compressed)
                {
                    Err(ureq::Error::Status(415, _)) => self.gzip_enabled = false,
                    result => return result.map_err(Box::new),
                }
            }
        }
        request().send_bytes(&self.body).map_err(Box::new)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestResponse {
    task: Option<tasks::TaskCommand>,
    workload_command: Option<workload_control::Command>,
    refresh_interval_ms: Option<u64>,
}

fn run(options: RunOptions) -> Result<()> {
    let RunOptions {
        server,
        name,
        workload,
        state_dir,
        managed,
        skip_update,
        desktop_events,
        allow_task_management,
        allow_workload_management,
        desktop_samples,
    } = options;
    let workload_for_tasks = workload.clone();
    let default_interval = sample_interval(
        platform_name(),
        desktop_events || desktop_samples,
        desktop_app_installed(),
    );
    let mut interval = default_interval;
    // Keep only the newest local sample while a server request is in flight.
    let desktop_input = desktop_samples.then(|| {
        let latest = Arc::new((Mutex::new(None), Condvar::new()));
        let input = Arc::clone(&latest);
        thread::spawn(move || {
            for line in io::stdin().lock().lines().map_while(Result::ok) {
                if let Ok(sample) = serde_json::from_str::<serde_json::Value>(&line)
                    && sample
                        .get("metrics")
                        .is_some_and(|metrics| metrics.is_array())
                {
                    let (lock, ready) = &*input;
                    *lock.lock().unwrap() = Some(sample);
                    ready.notify_one();
                }
            }
            std::process::exit(0);
        });
        latest
    });
    let dir = resolve_state_dir(state_dir)?;
    if managed
        && !skip_update
        && dir.join("identity.json").is_file()
        && let Some(result) = update::start(&dir)
    {
        return result;
    }
    let _lock = lock_state(&dir)?;
    let http = agent_http();
    let access = AccessCredentials::load(&dir)?;
    let pending = dir.join("pending-pairing.json");
    let identity = load_identity(
        &dir,
        &http,
        access.as_ref(),
        Enrollment {
            server,
            name,
            workload,
            pending: desktop_events.then_some(pending.as_path()),
            allow_pairing: !managed,
            default_server: "http://localhost:8080",
        },
    )?;
    if desktop_events {
        // Keep an approved pairing recoverable until its identity has been saved.
        let _ = fs::remove_file(&pending);
        println!(
            "{}",
            json!({"status":"reporting","nodeId":identity.node_id})
        );
        io::stdout().flush()?;
    } else if managed {
        println!(
            "NodeStats background agent reporting to {}.",
            identity.server
        );
    } else if interval == DESKTOP_SAMPLE_INTERVAL {
        println!(
            "Reporting to {} every second. Press Ctrl+C to stop.",
            identity.server
        );
    } else {
        println!(
            "Reporting to {} every two seconds. Press Ctrl+C to stop.",
            identity.server
        );
    }
    let mut collector = (!desktop_samples).then(|| {
        Collector::new(
            matches!(platform_name(), "docker" | "kubernetes"),
            !managed && io::stdin().is_terminal(),
            desktop_events,
            interval,
        )
    });
    let host = matches!(platform_name(), "linux" | "macos" | "windows")
        && workload_name(workload_for_tasks).is_none();
    let mut tasks = (allow_task_management && host).then(tasks::Tasks::new);
    let allow_workload_management = allow_workload_management && host;
    let mut workload_result: Option<workload_control::Result> = None;
    let mut task_result: Option<tasks::TaskResult> = None;
    let ping = (!desktop_samples).then(|| network::Ping::start(&identity.server, interval));
    if !desktop_samples {
        thread::sleep(interval);
    }
    let mut failing = false;
    let mut upload = Upload {
        http: &http,
        identity: &identity,
        access: access.as_ref(),
        authorization: format!("Bearer {}", identity.token),
        body: Vec::new(),
        compressed: Vec::new(),
        gzip_enabled: true,
    };
    let mut confirm_update = skip_update;
    loop {
        let start = Instant::now();
        let sample = if let Some(collector) = &mut collector {
            let mut sample = collector.sample();
            if let Some(ping) = &ping {
                sample.metrics.push(ping.reading());
            }
            ReportingSample::Native(sample)
        } else {
            let Some((lock, ready)) = desktop_input.as_deref() else {
                unreachable!("desktop_samples always initializes desktop_input");
            };
            let mut guard = lock.lock().unwrap();
            if guard.is_none() {
                (guard, _) = ready.wait_timeout(guard, interval).unwrap();
            }
            let Some(sample) = guard.take() else {
                continue;
            };
            ReportingSample::Desktop(sample)
        };
        let task_processes = tasks.as_mut().map(tasks::Tasks::snapshot);
        let command_sample = if tasks.is_some() || allow_workload_management {
            let mut value = serde_json::to_value(&sample)?;
            if let Some(processes) = &task_processes {
                let mut report = json!({"processes": processes});
                if let Some(result) = &task_result {
                    report["result"] = serde_json::to_value(result)?;
                }
                value["tasks"] = report;
            }
            if allow_workload_management {
                value["workloadControl"] = match &workload_result {
                    Some(result) => json!({"result":result}),
                    None => json!({}),
                };
            }
            Some(value)
        } else {
            None
        };
        upload.body.clear();
        match &command_sample {
            Some(report) => serde_json::to_writer(&mut upload.body, report)?,
            None => serde_json::to_writer(&mut upload.body, &sample)?,
        }
        let request_at = Instant::now();
        if confirm_update {
            update::confirm_startup(&dir);
            confirm_update = false;
        }
        match upload.send() {
            Ok(response) => {
                check_monitor_response(&response)?;
                if let Ok(body) = serde_json::from_reader::<_, IngestResponse>(
                    response.into_reader().take(65_536),
                ) {
                    let next_interval = body
                        .refresh_interval_ms
                        .map(clamp_sample_interval)
                        .unwrap_or(default_interval);
                    if next_interval != interval {
                        interval = next_interval;
                        if let Some(collector) = &mut collector {
                            collector.set_interval(interval);
                        }
                        if let Some(ping) = &ping {
                            ping.set_interval(interval);
                        }
                        if desktop_events {
                            println!(
                                "{}",
                                json!({
                                    "status": "config",
                                    "refreshIntervalMs": body.refresh_interval_ms.map(|ms| clamp_sample_interval(ms).as_millis() as u64)
                                })
                            );
                            let _ = io::stdout().flush();
                        }
                    }
                    if tasks.is_some() || allow_workload_management {
                        task_result = None;
                        workload_result = None;
                        let fresh = request_at.elapsed() <= Duration::from_secs(5);
                        if let (Some(manager), Some(processes), Some(task)) =
                            (&tasks, &task_processes, body.task)
                        {
                            task_result = Some(if fresh {
                                manager.force_quit(task, processes)
                            } else {
                                tasks::TaskResult {
                                    id: task.id,
                                    status: "unavailable",
                                }
                            });
                        }
                        if allow_workload_management && let Some(command) = body.workload_command {
                            workload_result = Some(
                                match command_sample
                                    .as_ref()
                                    .and_then(|value| value["metrics"].as_array())
                                    .filter(|_| fresh)
                                {
                                    Some(metrics) => command.execute(metrics),
                                    None => command.unavailable(),
                                },
                            );
                        }
                    }
                }
                if failing {
                    eprintln!("Reporting resumed.");
                }
                failing = false;
            }
            Err(error) => match *error {
                ureq::Error::Status(401 | 403, _) => bail!(
                    "The node credential was rejected. Revoke the old node and pair again using a new state directory."
                ),
                ureq::Error::Status(s, _) if s < 500 && s != 429 => {
                    return Err(anyhow!("The server rejected this sample (HTTP {s})."));
                }
                e => {
                    if !failing {
                        eprintln!("Reporting interrupted ({e}). Retrying.");
                    }
                    failing = true;
                }
            },
        }
        if !desktop_samples {
            thread::sleep(interval.saturating_sub(start.elapsed()));
        } else {
            thread::sleep(Duration::from_millis(10).saturating_sub(start.elapsed()));
        }
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum LocalCommand {
    Processes {
        id: String,
    },
    ForceQuit(tasks::TaskCommand),
    #[serde(rename_all = "camelCase")]
    SetInterval {
        interval_ms: u64,
    },
}

fn desktop_local(server: Option<String>, interval_ms: Option<u64>) -> Result<()> {
    let ping = match server {
        Some(server) => network::Ping::start(
            &endpoint(&server)?,
            interval_ms
                .map(clamp_sample_interval)
                .unwrap_or_else(|| sample_interval(platform_name(), true, desktop_app_installed())),
        ),
        None => network::Ping::default(),
    };
    let hostname = System::host_name();
    println!(
        "{}",
        json!({"status":"local", "node": {
            "name": hostname.as_deref().unwrap_or("This machine"),
            "hostname": hostname,
            "platform": env::consts::OS,
            "arch": env::consts::ARCH,
            "osVersion": System::long_os_version()
        }})
    );
    io::stdout().flush()?;
    let mut interval = interval_ms
        .map(clamp_sample_interval)
        .unwrap_or_else(|| sample_interval(platform_name(), true, desktop_app_installed()));
    let mut collector = Collector::new(false, false, true, interval);
    let (commands, requests) = mpsc::sync_channel(8);
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if let Ok(command) = serde_json::from_str(&line)
                && commands.send(command).is_err()
            {
                break;
            }
        }
    });

    let mut tasks = tasks::Tasks::new();
    let mut processes = Vec::new();
    let mut next_sample = Instant::now() + interval;
    loop {
        match requests.recv_timeout(next_sample.saturating_duration_since(Instant::now())) {
            Ok(LocalCommand::Processes { id }) => {
                processes = tasks.snapshot();
                println!("{}", json!({"id": id, "processes": &processes}));
            }
            Ok(LocalCommand::ForceQuit(command)) => {
                println!(
                    "{}",
                    serde_json::to_string(&tasks.force_quit(command, &processes))?
                );
            }
            Ok(LocalCommand::SetInterval { interval_ms }) => {
                let next = clamp_sample_interval(interval_ms);
                if next != interval {
                    interval = next;
                    collector.set_interval(interval);
                    ping.set_interval(interval);
                    next_sample = next_sample.min(Instant::now() + interval);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_sample {
            let started = Instant::now();
            let mut sample = collector.sample();
            sample.metrics.push(ping.reading());
            println!("{}", json!({"sample": sample}));
            next_sample = started + interval;
        }
        io::stdout().flush()?;
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Setup {
            server,
            name,
            state_dir,
        } => setup(server, name, state_dir),
        Cmd::Status => service::Manager::detect()?.control(service::Action::Status),
        Cmd::Start => service::Manager::detect()?.control(service::Action::Start),
        Cmd::Stop => service::Manager::detect()?.control(service::Action::Stop),
        #[cfg(target_os = "windows")]
        Cmd::UnregisterStartup => service::unregister_windows_startup(),
        Cmd::Run(options) => run(options),
        Cmd::DesktopLocal {
            server,
            interval_ms,
        } => desktop_local(server, interval_ms),
        Cmd::Snapshot => {
            let interval = sample_interval(platform_name(), false, desktop_app_installed());
            let mut c = Collector::new(
                matches!(platform_name(), "docker" | "kubernetes"),
                false,
                false,
                interval,
            );
            thread::sleep(interval);
            println!("{}", serde_json::to_string_pretty(&c.sample())?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn printable_pairing_code_matches_protocol() {
        assert!(super::valid_pair_code("AB9X7J2L"));
        assert!(!super::valid_pair_code("ABCDEFG\r"));
        assert!(!super::valid_pair_code("ABCD"));
    }

    #[test]
    fn access_headers_are_only_sent_to_the_configured_origin() {
        let http = ureq::AgentBuilder::new().redirects(0).build();
        let access = super::AccessCredentials {
            server: "https://nodestats.example".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
        };
        for route in ["pair/start", "pair/poll", "samples"] {
            let request = super::monitor_request(&http, &access.server, route, Some(&access));
            assert_eq!(request.header("Origin"), Some(access.server.as_str()));
            assert_eq!(
                request.header("CF-Access-Client-Id"),
                Some("test-client-id")
            );
            assert_eq!(
                request.header("CF-Access-Client-Secret"),
                Some("test-client-secret")
            );
        }
        for origin in ["https://other.example", "http://localhost:8081"] {
            let request = super::monitor_request(&http, origin, "pair/start", Some(&access));
            assert_eq!(request.header("CF-Access-Client-Id"), None);
            assert_eq!(request.header("CF-Access-Client-Secret"), None);
        }
    }

    #[test]
    fn explains_access_redirect_before_parsing_json() {
        let response: ureq::Response = "HTTP/1.1 302 Found\r\nLocation: https://team.cloudflareaccess.com/cdn-cgi/access/login\r\n\r\n"
            .parse()
            .unwrap();
        let error = super::pairing_response::<super::PairStart>(response)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Cloudflare Access requires valid service credentials")
        );
    }

    #[test]
    fn restricts_cleartext_origins() {
        for origin in ["http://localhost:8081", "http://127.0.0.1", "http://[::1]"] {
            assert!(super::server_origin(origin, false).is_ok());
        }
        let gateway = "http://host.docker.internal:8081";
        assert!(super::server_origin(gateway, true).is_ok());
        assert!(super::server_origin(gateway, false).is_err());
        assert!(super::server_origin("http://example.com", true).is_err());
        assert!(super::server_origin("https://example.com", false).is_ok());
    }

    #[test]
    fn uses_one_second_cadence_only_on_desktop_os_with_desktop_app() {
        use std::time::Duration;
        let one = Duration::from_secs(1);
        let two = Duration::from_secs(2);
        for platform in ["macos", "linux", "windows"] {
            assert_eq!(super::sample_interval(platform, true, false), one);
            assert_eq!(super::sample_interval(platform, false, true), one);
            assert_eq!(super::sample_interval(platform, false, false), two);
        }
        for platform in ["docker", "kubernetes", "freebsd"] {
            assert_eq!(super::sample_interval(platform, true, true), two);
        }
    }

    #[test]
    fn clamps_sample_interval_and_parses_commands_and_ingest_config() {
        use std::time::Duration;
        assert_eq!(super::OVERLAY_SAMPLE_INTERVAL, Duration::from_millis(200));
        assert_eq!(super::clamp_sample_interval(1), Duration::from_millis(10));
        assert_eq!(super::clamp_sample_interval(10), Duration::from_millis(10));
        assert_eq!(
            super::clamp_sample_interval(200),
            Duration::from_millis(200)
        );
        assert_eq!(super::clamp_sample_interval(1000), Duration::from_secs(1));
        assert_eq!(
            super::clamp_sample_interval(60_000),
            Duration::from_secs(60)
        );
        assert_eq!(
            super::clamp_sample_interval(99_999),
            Duration::from_secs(60)
        );

        let parsed: super::LocalCommand =
            serde_json::from_str(r#"{"type":"setInterval","intervalMs":10}"#).unwrap();
        assert!(matches!(
            parsed,
            super::LocalCommand::SetInterval { interval_ms: 10 }
        ));

        let with_refresh: super::IngestResponse =
            serde_json::from_str(r#"{"ok":true,"refreshIntervalMs":250}"#).unwrap();
        assert_eq!(with_refresh.refresh_interval_ms, Some(250));
        let default_refresh: super::IngestResponse =
            serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert_eq!(default_refresh.refresh_interval_ms, None);
    }

    #[test]
    fn detects_packaged_desktop_collectors_and_linux_desktop_entries() {
        use std::{fs, path::Path};
        assert!(super::is_packaged_desktop_exe(Path::new(
            "/Applications/NodeStats.app/Contents/Resources/collector/nodestats"
        )));
        assert!(!super::is_packaged_desktop_exe(Path::new(
            "/usr/local/bin/nodestats"
        )));

        let portable = tempfile::tempdir().unwrap();
        let collector = portable.path().join("resources/collector");
        fs::create_dir_all(&collector).unwrap();
        fs::write(portable.path().join(".nodestats-portable"), "").unwrap();
        assert!(super::is_packaged_desktop_exe(&collector.join("nodestats")));

        let apps = tempfile::tempdir().unwrap();
        assert!(!super::has_nodestats_desktop_entry(apps.path()));
        fs::write(apps.path().join("other.desktop"), "[Desktop Entry]\n").unwrap();
        assert!(!super::has_nodestats_desktop_entry(apps.path()));
        fs::write(
            apps.path().join("com.example.nodestats7fe7ee.desktop"),
            "[Desktop Entry]\n",
        )
        .unwrap();
        assert!(super::has_nodestats_desktop_entry(apps.path()));
        assert!(!super::has_nodestats_desktop_entry(
            &apps.path().join("missing")
        ));
    }
}
