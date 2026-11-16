use crate::AccessCredentials;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use ring::{
    digest::{Context as Digest, SHA256},
    signature::{ED25519, UnparsedPublicKey},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use url::Url;

const FEED: &str = "nodestats-updates-v1.json";
const MAX_FEED: u64 = 32 * 1024;
const MAX_BINARY: u64 = 48 * 1024 * 1024;
const PENDING: &str = "startup-pending";

#[derive(Deserialize)]
struct SignedFeed {
    payload: String,
    signature: String,
}
#[derive(Deserialize)]
struct Feed {
    schema: u32,
    cli: Release,
}
#[derive(Deserialize)]
struct Release {
    version: String,
    artifacts: HashMap<String, Artifact>,
}
#[derive(Deserialize)]
struct Artifact {
    file: String,
    size: u64,
    sha256: String,
}

type Version = [u32; 3];

fn version(value: &str) -> Option<Version> {
    let mut fields = value.split('.');
    let mut result = [0; 3];
    for slot in &mut result {
        let field = fields.next()?;
        *slot = field.parse().ok()?;
        if slot.to_string() != field {
            return None;
        }
    }
    fields.next().is_none().then_some(result)
}

fn hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn target() -> Option<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some((os, arch))
}

struct Candidate {
    version: Version,
    artifact: Artifact,
}

fn verify_feed(bytes: &[u8], key: &[u8; 32], os: &str, arch: &str) -> Result<Candidate> {
    let envelope: SignedFeed = serde_json::from_slice(bytes)?;
    let signature = hex::<64>(&envelope.signature).context("Invalid update signature")?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(envelope.payload.as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("Update signature did not verify"))?;
    let feed: Feed = serde_json::from_str(&envelope.payload)?;
    ensure!(feed.schema == 1, "Unsupported update feed");
    let version = version(&feed.cli.version).context("Invalid CLI update version")?;
    let mut artifacts = feed.cli.artifacts;
    let id = format!("{os}-{arch}");
    let artifact = artifacts
        .remove(&id)
        .context("No update for this platform")?;
    let ext = if os == "windows" { ".exe" } else { "" };
    let expected = format!("nodestats-agent-{}-{id}{ext}", feed.cli.version);
    ensure!(artifact.file == expected, "Invalid update filename");
    ensure!(
        (1..=MAX_BINARY).contains(&artifact.size),
        "Invalid update size"
    );
    ensure!(hex::<32>(&artifact.sha256).is_some(), "Invalid update hash");
    Ok(Candidate { version, artifact })
}

fn read_limited(source: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    source.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Update feed is too large");
    Ok(bytes)
}

fn download(
    http: &ureq::Agent,
    url: Url,
    access: Option<&AccessCredentials>,
) -> Result<Box<dyn Read + Send + Sync>> {
    let mut request = http.get(url.as_str());
    if let Some(access) =
        access.filter(|access| access.server == url.origin().ascii_serialization())
    {
        request = request
            .set("CF-Access-Client-Id", &access.client_id)
            .set("CF-Access-Client-Secret", &access.client_secret);
    }
    let response = request.call()?;
    ensure!(response.status() == 200, "Update download is unavailable");
    Ok(response.into_reader())
}

fn check_binary(source: impl Read, mut output: impl Write, artifact: &Artifact) -> Result<()> {
    let mut source = source.take(artifact.size + 1);
    let mut hasher = Digest::new(&SHA256);
    let mut count = 0;
    let mut buffer = [0; 16 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let bytes = &buffer[..n];
        count += n as u64;
        hasher.update(bytes);
        output.write_all(bytes)?;
    }
    ensure!(count == artifact.size, "Update length did not match");
    let expected = hex::<32>(&artifact.sha256).context("Invalid update hash")?;
    ensure!(
        hasher.finish().as_ref() == expected,
        "Update hash did not match"
    );
    Ok(())
}

fn install_candidate(
    http: &ureq::Agent,
    downloads: &Url,
    directory: &Path,
    envelope: &[u8],
    candidate: &Candidate,
    access: Option<&AccessCredentials>,
) -> Result<PathBuf> {
    let path = directory.join(&candidate.artifact.file);
    let mut binary = tempfile::NamedTempFile::new_in(directory)?;
    check_binary(
        download(http, downloads.join(&candidate.artifact.file)?, access)?,
        binary.as_file_mut(),
        &candidate.artifact,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        binary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    binary.as_file().sync_all()?;
    binary.persist(&path)?;

    let mut receipt = tempfile::NamedTempFile::new_in(directory)?;
    receipt.write_all(envelope)?;
    receipt.as_file().sync_all()?;
    receipt.persist(directory.join(FEED))?;
    Ok(path)
}

fn candidate_path(
    directory: &Path,
    key: &[u8; 32],
    origin: &str,
    access: Option<&AccessCredentials>,
) -> Result<Option<PathBuf>> {
    let Some((os, arch)) = target() else {
        return Ok(None);
    };
    let mut downloads = Url::parse(origin)?;
    ensure!(
        downloads.scheme() == "https" && downloads.origin().ascii_serialization() == origin,
        "Update origin must be an HTTPS origin"
    );
    downloads.set_path("/downloads/");
    let current = version(env!("CARGO_PKG_VERSION")).context("Invalid installed CLI version")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let parent = directory
            .parent()
            .context("Missing update state directory")?;
        let uid = unsafe { libc::geteuid() };
        let metadata = fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o022 == 0,
            "Update state directory is not private"
        );
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "Update cache is not private"
        );
    }
    #[cfg(not(unix))]
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("update.lock"))?;
    lock.try_lock_exclusive()?;
    let cached = (|| {
        let receipt = read_limited(File::open(directory.join(FEED))?, MAX_FEED)?;
        let entry = verify_feed(&receipt, key, os, arch)?;
        ensure!(entry.version > current, "Cached release is not newer");
        let path = directory.join(&entry.artifact.file);
        check_binary(File::open(&path)?, std::io::sink(), &entry.artifact)?;
        Ok::<_, anyhow::Error>((entry.version, path))
    })()
    .ok();
    let http = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(4))
        .build();
    let updated = (|| {
        let receipt = read_limited(download(&http, downloads.join(FEED)?, access)?, MAX_FEED)?;
        let entry = verify_feed(&receipt, key, os, arch)?;
        let installed = cached.as_ref().map_or(current, |(version, _)| *version);
        if entry.version <= installed {
            return Ok(None);
        }
        let path = install_candidate(&http, &downloads, directory, &receipt, &entry, access)?;
        if let Some((_, old)) = &cached {
            let _ = fs::remove_file(old);
        }
        eprintln!("NodeStats downloaded a verified startup update.");
        Ok::<_, anyhow::Error>(Some(path))
    })();
    match updated {
        Ok(Some(path)) => Ok(Some(path)),
        Ok(None) => Ok(cached.map(|(_, path)| path)),
        Err(error) => {
            eprintln!(
                "NodeStats update unavailable ({error}). Continuing with the installed version."
            );
            Ok(cached.map(|(_, path)| path))
        }
    }
}

fn update_directory(state: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // Local Service can write its own profile and only read Program Files.
        let _ = state;
        Some(dirs::data_local_dir()?.join("NodeStats").join("updates-v1"))
    }
    #[cfg(not(windows))]
    Some(state.join("updates-v1"))
}

fn pending_matches(directory: &Path, executable: &Path) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            File::open(directory.join(PENDING))
                .ok()
                .and_then(|file| read_limited(file, 512).ok())
                .is_some_and(|pending| pending == name.as_bytes())
        })
}

fn mark_pending(directory: &Path, executable: &Path) -> Result<()> {
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid update path")?;
    let mut record = tempfile::NamedTempFile::new_in(directory)?;
    record.write_all(name.as_bytes())?;
    record.as_file().sync_all()?;
    record.persist(directory.join(PENDING))?;
    Ok(())
}

pub fn confirm_startup(state: &Path) {
    if let (Some(directory), Ok(executable)) = (update_directory(state), std::env::current_exe())
        && pending_matches(&directory, &executable)
    {
        let _ = fs::remove_file(directory.join(PENDING));
    }
}

pub fn start(state: &Path) -> Option<Result<()>> {
    // Never use a user-writable update store from an elevated Unix process.
    #[cfg(unix)]
    if unsafe { libc::geteuid() == 0 || libc::geteuid() != libc::getuid() } {
        return None;
    }
    if matches!(crate::platform_name(), "docker" | "kubernetes") {
        return None;
    }
    let key = hex::<32>(option_env!("NODESTATS_UPDATE_PUBLIC_KEY")?)?;
    let origin = option_env!("NODESTATS_UPDATE_ORIGIN").unwrap_or("https://nodestats.hu");
    let directory = update_directory(state)?;
    let access = AccessCredentials::load(state).ok().flatten();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(candidate_path(&directory, &key, origin, access.as_ref()));
    });
    let path = match receive.recv_timeout(Duration::from_secs(9)) {
        Ok(Ok(path)) => path?,
        Ok(Err(error)) => {
            eprintln!("NodeStats update check skipped ({error}).");
            return None;
        }
        Err(_) => {
            eprintln!("NodeStats update check timed out. Continuing with the installed version.");
            return None;
        }
    };
    let directory = path.parent()?;
    if pending_matches(directory, &path) {
        eprintln!(
            "The previous update did not finish startup. Continuing with the installed version."
        );
        return None;
    }
    if let Err(error) = mark_pending(directory, &path) {
        eprintln!(
            "NodeStats could not stage startup ({error}). Continuing with the installed version."
        );
        return None;
    }
    let mut command = Command::new(path);
    command
        .args(std::env::args_os().skip(1))
        .arg("--skip-update");
    match launch(&mut command) {
        Ok(()) => Some(Ok(())),
        Err(error) => {
            eprintln!(
                "NodeStats could not launch the update ({error}). Continuing with the installed version."
            );
            None
        }
    }
}

#[cfg(unix)]
fn launch(command: &mut Command) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Err(command.exec().into())
}

#[cfg(windows)]
fn launch(command: &mut Command) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
    };
    struct Job(HANDLE);
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    // Task Scheduler stopping the bootstrap also closes this job and stops its worker.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let job = Job(handle);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut child = command.spawn()?;
    if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("Updated worker exited with {status}");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn launch(_: &mut Command) -> Result<()> {
    anyhow::bail!("Startup updates are unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        digest::digest,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use serde_json::json;

    fn hex_string(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn verifies_signed_platform_and_rejects_tampering() {
        let signer = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
        let key: &[u8; 32] = signer.public_key().as_ref().try_into().unwrap();
        let payload = json!({
            "schema": 1,
            "cli": {"version": "2.10.0", "artifacts": {"linux-arm64": {
                "file": "nodestats-agent-2.10.0-linux-arm64",
                "size": 123,
                "sha256": "2".repeat(64)
            }}}
        })
        .to_string();
        let mut receipt = json!({
            "payload": payload,
            "signature": hex_string(signer.sign(payload.as_bytes()).as_ref())
        });
        let encode = |receipt: &serde_json::Value| serde_json::to_vec(receipt).unwrap();
        assert_eq!(
            verify_feed(&encode(&receipt), key, "linux", "arm64")
                .unwrap()
                .version,
            [2, 10, 0]
        );
        assert!(verify_feed(&encode(&receipt), key, "linux", "x64").is_err());
        receipt["payload"] = json!(payload.replace("2.10.0", "9.10.0"));
        assert!(verify_feed(&encode(&receipt), key, "linux", "arm64").is_err());
        assert!(version("2.1.0") < version("2.10.0"));
        assert!(version("2.01.0").is_none());
    }

    #[test]
    fn stages_only_verified_downloads() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        let state = tempfile::tempdir().unwrap();
        let body = b"known release artifact";
        let candidate = Candidate {
            version: [2, 0, 0],
            artifact: Artifact {
                file: "fixture".into(),
                size: body.len() as u64,
                sha256: hex_string(digest(&SHA256, body).as_ref()),
            },
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            for bytes in [body.as_slice(), b"damaged release"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = BufReader::new(&mut stream);
                loop {
                    let mut line = String::new();
                    if request.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .unwrap();
                stream.write_all(bytes).unwrap();
            }
        });
        let root = Url::parse(&format!("http://{address}/")).unwrap();
        let http = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .redirects(0)
            .build();
        let path = install_candidate(
            &http,
            &root,
            state.path(),
            b"verified receipt",
            &candidate,
            None,
        )
        .unwrap();
        assert!(
            install_candidate(&http, &root, state.path(), b"bad receipt", &candidate, None)
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), body);
        assert_eq!(
            fs::read(state.path().join(FEED)).unwrap(),
            b"verified receipt"
        );
        responder.join().unwrap();
    }

    #[test]
    fn rejects_truncated_tampered_or_oversized_binary() {
        let body = b"verified runner";
        let artifact = Artifact {
            file: "test".into(),
            size: body.len() as u64,
            sha256: hex_string(digest(&SHA256, body).as_ref()),
        };
        let mut output = Vec::new();
        check_binary(&body[..], &mut output, &artifact).unwrap();
        assert_eq!(output, body);
        for invalid in [
            b"verified runne".as_slice(),
            b"modified runner",
            b"verified runner!",
        ] {
            assert!(check_binary(invalid, std::io::sink(), &artifact).is_err());
        }
    }
}
