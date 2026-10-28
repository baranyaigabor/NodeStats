use crate::metrics::Metric;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

/// Run vendor utilities off the sampler thread and discard stalled or oversized output.
pub fn probe(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    probe_runtime(program, args, Duration::from_secs(4), false)
}

pub(crate) fn probe_runtime(
    program: &str,
    args: &[&str],
    timeout: Duration,
    logs: bool,
) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if logs { Stdio::piped() } else { Stdio::null() })
        .spawn()
        .ok()?;
    fn read(pipe: impl Read + Send + 'static) -> thread::JoinHandle<Option<Vec<u8>>> {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            (bytes.len() <= 4 * 1024 * 1024).then_some(bytes)
        })
    }
    let stdout = read(child.stdout.take()?);
    let stderr = child.stderr.take().map(read);
    let status = match child.wait_timeout(timeout) {
        Ok(Some(s)) => s,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    let mut result = stdout.join().ok()??;
    if let Some(stderr) = stderr {
        result.extend(stderr.join().ok()??);
    }
    Some(result)
}

type Cache = Arc<Mutex<(Instant, Vec<Metric>)>>;
type IntervalSignal = Arc<(Mutex<Duration>, Condvar)>;
pub struct Extras {
    cache: Cache,
    interval: IntervalSignal,
    #[cfg(target_os = "macos")]
    power: crate::macos::Power,
}
impl Extras {
    pub fn new(_interactive: bool, _desktop: bool, interval: Duration) -> Self {
        let cache: Cache = Arc::new(Mutex::new((Instant::now(), Vec::new())));
        let interval_signal: IntervalSignal = Arc::new((Mutex::new(interval), Condvar::new()));
        let reader = Arc::downgrade(&cache);
        let ticker = Arc::clone(&interval_signal);
        thread::spawn(move || {
            #[cfg(target_os = "linux")]
            let mut counters = crate::linux::EnergyCounters::default();
            #[cfg(target_os = "macos")]
            let mut cached_fps: Option<(Instant, Metric)> = None;
            loop {
                if reader.strong_count() == 0 {
                    break;
                }
                let begin = Instant::now();
                #[cfg(target_os = "linux")]
                let mut values = crate::linux::sensors(&mut counters);
                #[cfg(target_os = "macos")]
                let mut values = {
                    let target_interval = (*ticker.0.lock().unwrap())
                        .clamp(Duration::from_millis(200), Duration::from_secs(60));
                    let fps_cadence =
                        target_interval.clamp(Duration::from_millis(500), Duration::from_secs(60));
                    let mut values = crate::macos::sensors();
                    if _desktop {
                        let fps = match &cached_fps {
                            Some((at, metric)) if at.elapsed() < fps_cadence => metric.clone(),
                            _ => {
                                let metric = crate::macos::fps_reading();
                                cached_fps = Some((Instant::now(), metric.clone()));
                                metric
                            }
                        };
                        values.push(fps);
                    }
                    values
                };
                #[cfg(target_os = "windows")]
                let mut values = crate::windows::sensors();
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                let mut values = Vec::new();
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                values.extend(nvidia());
                values.push(crate::network::buffer());
                if let Some(cache) = reader.upgrade() {
                    *cache.lock().unwrap() = (Instant::now(), values);
                } else {
                    break;
                }
                let (lock, condvar) = &*ticker;
                let mut guard = lock.lock().unwrap();
                loop {
                    if reader.strong_count() == 0 {
                        break;
                    }
                    let target =
                        (*guard).clamp(Duration::from_millis(200), Duration::from_secs(60));
                    if begin.elapsed() >= target {
                        break;
                    }
                    let wait = target.saturating_sub(begin.elapsed());
                    if wait.is_zero() {
                        break;
                    }
                    let (next, _) = condvar.wait_timeout(guard, wait).unwrap();
                    guard = next;
                }
            }
        });
        Self {
            cache,
            interval: interval_signal,
            #[cfg(target_os = "macos")]
            power: crate::macos::Power::start(_interactive, interval),
        }
    }
    pub fn set_interval(&self, interval: Duration) {
        let (lock, condvar) = &*self.interval;
        *lock.lock().unwrap() = interval;
        condvar.notify_all();
    }
    pub fn read(&self) -> Vec<Metric> {
        let interval = *self.interval.0.lock().unwrap();
        let max_age = (interval * 2).max(Duration::from_secs(12));
        let cache = self.cache.lock().unwrap();
        let values = if cache.0.elapsed() < max_age {
            cache.1.clone()
        } else {
            Vec::new()
        };
        #[cfg(target_os = "macos")]
        {
            let mut values = values;
            values.extend(self.power.read());
            values
        }
        #[cfg(not(target_os = "macos"))]
        {
            values
        }
    }
}
// Vendor probes run using the client's account. Resolve them in system driver
// directories rather than searching the user's PATH or current working directory.
#[cfg(target_os = "linux")]
fn nvidia_tool() -> Option<std::path::PathBuf> {
    [
        "/usr/bin/nvidia-smi",
        "/usr/local/nvidia/bin/nvidia-smi",
        "/usr/lib/wsl/lib/nvidia-smi",
        "/run/current-system/sw/bin/nvidia-smi",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|p| p.is_file())
}

#[cfg(target_os = "windows")]
fn nvidia_tool() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from);
    let program_files = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(std::path::PathBuf::from);
    [
        root.map(|p| p.join(r"System32\nvidia-smi.exe")),
        program_files.map(|p| p.join(r"NVIDIA Corporation\NVSMI\nvidia-smi.exe")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_absolute() && p.is_file())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn nvidia() -> Vec<Metric> {
    let mut m = Vec::new();
    let query = "--query-gpu=uuid,name,utilization.gpu,utilization.memory,memory.used,memory.total,temperature.gpu,power.draw,clocks.current.graphics,clocks.current.memory";
    let Some(tool) = nvidia_tool() else {
        return m;
    };
    if let Some(bytes) = tool
        .to_str()
        .and_then(|path| probe(path, &[query, "--format=csv,noheader,nounits"]))
    {
        for row in String::from_utf8_lossy(&bytes).lines() {
            let f: Vec<_> = row.split(',').map(str::trim).collect();
            if f.len() != 10 {
                continue;
            }
            let id = crate::metrics::id(f[0]);
            let name = f[1];
            for (i, group, key, label, unit, scale) in [
                (2, "gpu", "usage", "utilization", "%", 1.),
                (3, "gpu", "memory_busy", "memory bus", "%", 1.),
                (4, "gpu", "vram_used", "VRAM used", "B", 1048576.),
                (5, "gpu", "vram_total", "VRAM capacity", "B", 1048576.),
                (6, "temperature", "gpu", "temperature", "°C", 1.),
                (7, "power", "gpu", "board power", "W", 1.),
                (8, "gpu", "clock", "graphics clock", "MHz", 1.),
                (9, "gpu", "vram_clock", "memory clock", "MHz", 1.),
            ] {
                if let Ok(v) = f[i].parse::<f64>() {
                    m.push(Metric::new(
                        format!("{group}.{id}.{key}"),
                        format!("{name} {label}"),
                        group,
                        unit,
                        v * scale,
                        "host GPU",
                    ));
                }
            }
        }
    }
    m
}
