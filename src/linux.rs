use crate::metrics::{Metric, id};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

fn read(p: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_to_string(p).ok()?.trim().to_owned())
}
fn value(p: impl AsRef<Path>) -> Option<f64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}
fn children(path: &str) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
}
fn pair_value(text: &str, key: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let (name, v) = line.split_once(' ')?;
        (name == key).then(|| v.parse::<f64>().ok()).flatten()
    })
}

#[derive(Default)]
pub struct EnergyCounters(std::collections::HashMap<PathBuf, (f64, Instant)>);
pub fn sensors(counters: &mut EnergyCounters) -> Vec<Metric> {
    let mut m = Vec::new();
    for hw in children("/sys/class/hwmon") {
        let name = read(hw.join("name")).unwrap_or_else(|| "Sensor".into());
        let sensor = id(&fs::canonicalize(&hw)
            .unwrap_or_else(|_| hw.clone())
            .to_string_lossy());
        for file in fs::read_dir(hw.clone()).into_iter().flatten().flatten() {
            let fname = file.file_name();
            let Some(fname) = fname.to_str() else {
                continue;
            };
            let Some((base, measure)) = fname.rsplit_once('_') else {
                continue;
            };
            let group_unit = if base.starts_with("power") && ["input", "average"].contains(&measure)
            {
                Some(("power", "W", 1e-6))
            } else if base.starts_with("energy") && measure == "input" {
                Some(("power", "Wh", 1. / 3.6e9))
            } else if base.starts_with("fan") && measure == "input" {
                Some(("system", "RPM", 1.))
            } else {
                None
            };
            if let (Some((group, unit, scale)), Some(v)) = (group_unit, value(file.path())) {
                let channel = read(hw.join(format!("{base}_label"))).unwrap_or_else(|| base.into());
                m.push(Metric::new(
                    format!("{group}.{sensor}.{fname}"),
                    format!("{name} {channel} {measure}"),
                    group,
                    unit,
                    v * scale,
                    "host",
                ));
            }
        }
    }
    for card in children("/sys/class/drm") {
        let n = card.file_name().unwrap_or_default().to_string_lossy();
        if !n
            .strip_prefix("card")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let dev = card.join("device");
        let gpu = id(&fs::canonicalize(&dev)
            .unwrap_or(dev.clone())
            .to_string_lossy());
        for (file, suffix, label, unit) in [
            ("gpu_busy_percent", "usage", "GPU utilization", "%"),
            ("mem_busy_percent", "memory_busy", "Memory bus", "%"),
            ("mem_info_vram_used", "vram_used", "VRAM used", "B"),
            ("mem_info_vram_total", "vram_total", "VRAM capacity", "B"),
        ] {
            if let Some(v) = value(dev.join(file)) {
                m.push(Metric::new(
                    format!("gpu.{gpu}.{suffix}"),
                    format!("{n} {label}"),
                    "gpu",
                    unit,
                    v,
                    "host GPU",
                ));
            }
        }
    }
    for zone in children("/sys/class/powercap") {
        let counter = zone.join("energy_uj");
        let counter_text = match fs::read_to_string(&counter) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let name = read(zone.join("name")).unwrap_or_else(|| "CPU".into());
                m.push(Metric::missing(
                    &format!("power.rapl.{}.watts", id(&zone.to_string_lossy())),
                    &format!("{name} average power"),
                    "power",
                    "W",
                    "host",
                    "The Linux kernel restricts access to this package energy counter.",
                ));
                continue;
            }
            Err(_) => continue,
        };
        if let Ok(uj) = counter_text.trim().parse::<f64>() {
            let name = read(zone.join("name")).unwrap_or_else(|| "Power domain".into());
            let key = id(&zone.to_string_lossy());
            let now = Instant::now();
            if let Some((last, t)) = counters.0.insert(zone.clone(), (uj, now)) {
                let delta = if uj >= last {
                    Some(uj - last)
                } else {
                    value(zone.join("max_energy_range_uj")).map(|max| max - last + uj)
                };
                if let Some(delta) = delta.filter(|d| *d >= 0.) {
                    m.push(Metric::new(
                        format!("power.rapl.{key}.watts"),
                        format!("{name} average power"),
                        "power",
                        "W",
                        delta / 1e6 / now.duration_since(t).as_secs_f64(),
                        "host",
                    ));
                }
            }
            m.push(Metric::new(
                format!("power.rapl.{key}"),
                format!("{name} energy counter"),
                "power",
                "Wh",
                uj / 3.6e9,
                "host",
            ));
        }
    }
    for b in children("/sys/class/power_supply") {
        let name = b.file_name().unwrap_or_default().to_string_lossy();
        if let Some(v) = value(b.join("power_now"))
            .or_else(|| Some(value(b.join("current_now"))? * value(b.join("voltage_now"))? / 1e6))
        {
            m.push(Metric::new(
                format!("power.supply.{}", id(&name)),
                format!("{name} power"),
                "power",
                "W",
                v / 1e6,
                "power supply",
            ));
        }
        if let Some(v) = value(b.join("capacity")) {
            m.push(Metric::new(
                format!("system.battery.{}", id(&name)),
                format!("{name} charge"),
                "system",
                "%",
                v,
                "battery",
            ));
        }
    }
    m
}

pub struct Cgroup {
    cpu: PathBuf,
    mem: PathBuf,
    account: PathBuf,
    cpuset: PathBuf,
    io: PathBuf,
    io_last: Option<(f64, f64, Instant)>,
    last: Option<(f64, Instant)>,
}
impl Cgroup {
    pub fn new() -> Self {
        let map = read("/proc/self/cgroup").unwrap_or_default();
        let mountinfo = read("/proc/self/mountinfo").unwrap_or_default();
        let locate = |controller: &str| {
            for line in map.lines() {
                let mut f = line.splitn(3, ':');
                let _ = f.next();
                let ctrls = f.next().unwrap_or("");
                let path = f.next().unwrap_or("/");
                if !ctrls.is_empty() && !ctrls.split(',').any(|k| k == controller) {
                    continue;
                }
                for mount in mountinfo.lines() {
                    let Some((before, after)) = mount.split_once(" - ") else {
                        continue;
                    };
                    let fields: Vec<_> = before.split_whitespace().collect();
                    let kind: Vec<_> = after.split_whitespace().collect();
                    if fields.len() < 5 || kind.len() < 3 {
                        continue;
                    }
                    let matching = if ctrls.is_empty() {
                        kind[0] == "cgroup2"
                    } else {
                        kind[0] == "cgroup" && kind[2].split(',').any(|k| k == controller)
                    };
                    if matching {
                        let base = Path::new(fields[4]);
                        let rel = Path::new(path).strip_prefix(fields[3]).ok();
                        if let Some(rel) = rel.filter(|p| {
                            p.components()
                                .all(|c| matches!(c, std::path::Component::Normal(_)))
                        }) {
                            let target = base.join(rel);
                            if target.exists() {
                                return target;
                            }
                        }
                        return base.into();
                    }
                }
            }
            PathBuf::from("/sys/fs/cgroup")
        };
        let mut cg = Self {
            cpu: locate("cpu"),
            mem: locate("memory"),
            account: locate("cpuacct"),
            cpuset: locate("cpuset"),
            io: locate("blkio"),
            last: None,
            io_last: None,
        };
        cg.sample(1);
        cg
    }
    pub fn sample(&mut self, cpus: usize) -> Vec<Metric> {
        let mut m = Vec::new();
        let scope = "workload";
        let used = value(self.mem.join("memory.current"))
            .or_else(|| value(self.mem.join("memory.usage_in_bytes")));
        let limit = value(self.mem.join("memory.max"))
            .or_else(|| value(self.mem.join("memory.limit_in_bytes")))
            .filter(|v| *v > 0. && *v < 1e18);
        if let Some(v) = used {
            m.push(Metric::new(
                "memory.used",
                "Workload memory used",
                "memory",
                "B",
                v,
                scope,
            ));
        }
        if let Some(v) = limit {
            m.push(Metric::new(
                "memory.total",
                "Workload memory limit",
                "memory",
                "B",
                v,
                scope,
            ));
        }
        if let (Some(u), Some(l)) = (used, limit) {
            m.push(Metric::new(
                "memory.usage",
                "Workload memory utilization",
                "memory",
                "%",
                100. * u / l,
                scope,
            ));
        } else {
            m.push(Metric::missing(
                "memory.usage",
                "Workload memory utilization",
                "memory",
                "%",
                scope,
                "No finite readable workload memory limit.",
            ));
        }
        let usage = read(self.cpu.join("cpu.stat"))
            .and_then(|s| pair_value(&s, "usage_usec"))
            .map(|v| v / 1e6)
            .or_else(|| value(self.account.join("cpuacct.usage")).map(|v| v / 1e9));
        let max = read(self.cpu.join("cpu.max"))
            .and_then(|s| {
                let (q, p) = s.split_once(' ')?;
                Some(q.parse::<f64>().ok()? / p.parse::<f64>().ok()?)
            })
            .or_else(|| {
                Some(
                    value(self.cpu.join("cpu.cfs_quota_us"))?
                        / value(self.cpu.join("cpu.cfs_period_us"))?,
                )
            })
            .filter(|v| *v > 0.);
        let available = read(self.cpuset.join("cpuset.cpus.effective"))
            .or_else(|| read(self.cpuset.join("cpuset.cpus")))
            .and_then(|s| cpu_count(&s))
            .unwrap_or(cpus.max(1)) as f64;
        let capacity = max.map(|v| v.min(available)).unwrap_or(available);
        m.push(Metric::new(
            "cpu.capacity",
            "Workload CPU capacity",
            "cpu",
            "cores",
            capacity,
            scope,
        ));
        if let Some(u) = usage {
            let now = Instant::now();
            if let Some((prev, at)) = self.last.filter(|(prev, _)| *prev <= u) {
                m.push(Metric::new(
                    "cpu.usage",
                    "Workload CPU utilization",
                    "cpu",
                    "%",
                    100. * (u - prev) / now.duration_since(at).as_secs_f64() / capacity,
                    scope,
                ));
            }
            self.last = Some((u, now));
        } else {
            m.push(Metric::missing(
                "cpu.usage",
                "Workload CPU utilization",
                "cpu",
                "%",
                scope,
                "CPU accounting is unavailable in this cgroup.",
            ));
        }
        if let Some(per_cpu) = read(self.account.join("cpuacct.usage_percpu")) {
            for (i, v) in per_cpu
                .split_whitespace()
                .enumerate()
                .filter_map(|(i, v)| Some((i, v.parse::<f64>().ok()?)))
            {
                m.push(Metric::new(
                    format!("cpu.time.{i}"),
                    format!("Core {i} CPU time"),
                    "cpu",
                    "s",
                    v / 1e9,
                    scope,
                ));
            }
        }
        if let Some(stat) = read(self.io.join("io.stat")) {
            let mut rx = 0.;
            let mut tx = 0.;
            for field in stat.split_whitespace() {
                if let Some((k, v)) = field.split_once('=') {
                    if let Ok(bytes) = v.parse::<f64>() {
                        match k {
                            "rbytes" => rx += bytes,
                            "wbytes" => tx += bytes,
                            _ => {}
                        }
                    }
                }
            }
            let now = Instant::now();
            if let Some((old_rx, old_tx, t)) = self.io_last.replace((rx, tx, now)) {
                let dt = now.duration_since(t).as_secs_f64();
                if rx >= old_rx && tx >= old_tx {
                    m.push(Metric::new(
                        "storage.io.read",
                        "Workload disk read",
                        "storage",
                        "B/s",
                        (rx - old_rx) / dt,
                        scope,
                    ));
                    m.push(Metric::new(
                        "storage.io.write",
                        "Workload disk write",
                        "storage",
                        "B/s",
                        (tx - old_tx) / dt,
                        scope,
                    ));
                }
            }
        }
        m
    }
}
fn cpu_count(s: &str) -> Option<usize> {
    let mut sum = 0;
    for range in s.split(',') {
        let (a, b) = range.split_once('-').unwrap_or((range, range));
        sum += b.parse::<usize>().ok()?.checked_sub(a.parse().ok()?)? + 1;
    }
    Some(sum)
}
