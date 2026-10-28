use serde::Serialize;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use sysinfo::{Components, Disks, Networks, System};
use time::OffsetDateTime;

#[derive(Clone, Debug, Serialize)]
pub struct Metric {
    pub key: String,
    pub label: String,
    pub group: &'static str,
    pub unit: &'static str,
    pub value: Option<f64>,
    pub scope: &'static str,
    #[serde(rename = "composeProject", skip_serializing_if = "Option::is_none")]
    pub compose_project: Option<String>,
    #[serde(rename = "composeService", skip_serializing_if = "Option::is_none")]
    pub compose_service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}
impl Metric {
    pub fn new(
        key: impl Into<String>,
        label: impl Into<String>,
        group: &'static str,
        unit: &'static str,
        value: f64,
        scope: &'static str,
    ) -> Self {
        let mut label = label.into();
        let trimmed = label.trim();
        if trimmed.is_empty() {
            label = "Sensor".into();
        } else if trimmed.len() != label.len() || trimmed.len() > 160 {
            let mut utf16_length = 0;
            label = trimmed
                .chars()
                .take_while(|ch| {
                    utf16_length += ch.len_utf16();
                    utf16_length <= 160
                })
                .collect();
        }
        Self {
            key: key.into(),
            label,
            group,
            unit,
            value: value.is_finite().then_some(value),
            scope,
            compose_project: None,
            compose_service: None,
            reason: None,
        }
    }
    pub fn missing(
        key: &str,
        label: &str,
        group: &'static str,
        unit: &'static str,
        scope: &'static str,
        reason: &'static str,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            group,
            unit,
            value: None,
            scope,
            compose_project: None,
            compose_service: None,
            reason: Some(reason),
        }
    }
}
#[derive(Serialize)]
pub struct Sample {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub metrics: Vec<Metric>,
}

/// A stable ASCII key for hardware paths, including Windows volume mount points.
pub fn id(s: &str) -> String {
    let mut hash = 2166136261u32;
    for b in s.bytes() {
        hash = (hash ^ b as u32).wrapping_mul(16777619);
    }
    format!("{hash:08x}")
}

pub struct Collector {
    sys: System,
    disks: Disks,
    nets: Networks,
    temps: Components,
    last: Instant,
    last_slow_refresh: Instant,
    extended: crate::platform::Extras,
    discovery: Option<crate::workloads::Discovery>,
    interval: Duration,
    workload: bool,
    #[cfg(target_os = "linux")]
    cg: crate::linux::Cgroup,
    energy: HashMap<String, (f64, Instant, f64)>,
}
impl Collector {
    pub fn new(workload: bool, interactive: bool, desktop: bool, interval: Duration) -> Self {
        let mut sys = System::new();
        if workload {
            // Capacity fallback needs the CPU count, not per-core usage or clocks.
            sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        } else {
            sys.refresh_cpu_all();
            sys.refresh_memory();
        }
        let discovery = (!workload).then(|| {
            crate::workloads::Discovery::new(
                sys.cpus().len() as f64,
                sys.total_memory() as f64,
                interval,
            )
        });
        let now = Instant::now();
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            nets: Networks::new_with_refreshed_list(),
            temps: Components::new_with_refreshed_list(),
            last: now,
            last_slow_refresh: now.checked_sub(Duration::from_secs(1)).unwrap_or(now),
            extended: crate::platform::Extras::new(interactive, desktop, interval),
            discovery,
            interval,
            workload,
            #[cfg(target_os = "linux")]
            cg: crate::linux::Cgroup::new(),
            energy: HashMap::new(),
        }
    }
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
        self.extended.set_interval(interval);
        if let Some(discovery) = &self.discovery {
            discovery.set_interval(interval);
        }
    }
    pub fn sample(&mut self) -> Sample {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f64().max(0.001);
        self.last = now;
        if !self.workload {
            self.sys.refresh_cpu_all();
            self.sys.refresh_memory();
        }
        self.nets.refresh(true);
        if now.duration_since(self.last_slow_refresh) >= Duration::from_millis(250) {
            self.disks.refresh(true);
            self.temps.refresh(true);
            self.last_slow_refresh = now;
        }
        let mut m = Vec::new();
        let scope = if self.workload { "workload" } else { "host" };
        if !self.workload {
            m.push(Metric::new(
                "cpu.usage",
                "CPU utilization",
                "cpu",
                "%",
                self.sys.global_cpu_usage() as f64,
                scope,
            ));
            for (i, cpu) in self.sys.cpus().iter().enumerate() {
                m.push(Metric::new(
                    format!("cpu.core.{i}"),
                    format!("Core {i}"),
                    "cpu",
                    "%",
                    cpu.cpu_usage() as f64,
                    scope,
                ));
                if cpu.frequency() > 0 {
                    m.push(Metric::new(
                        format!("cpu.frequency.{i}"),
                        format!("Core {i} clock"),
                        "cpu",
                        "MHz",
                        cpu.frequency() as f64,
                        scope,
                    ));
                }
            }
            let used = self.sys.used_memory() as f64;
            let total = self.sys.total_memory() as f64;
            if total > 0.0 {
                m.push(Metric::new(
                    "memory.usage",
                    "Memory utilization",
                    "memory",
                    "%",
                    100.0 * used / total,
                    scope,
                ));
                m.push(Metric::new(
                    "memory.used",
                    "Memory used",
                    "memory",
                    "B",
                    used,
                    scope,
                ));
                m.push(Metric::new(
                    "memory.total",
                    "Installed memory",
                    "memory",
                    "B",
                    total,
                    scope,
                ));
            }
            m.push(Metric::new(
                "memory.swap",
                "Swap used",
                "memory",
                "B",
                self.sys.used_swap() as f64,
                scope,
            ));
            m.push(Metric::new(
                "system.uptime",
                "System uptime",
                "system",
                "s",
                System::uptime() as f64,
                scope,
            ));
        }
        #[cfg(target_os = "linux")]
        if self.workload {
            m.extend(self.cg.sample(self.sys.cpus().len()));
        }
        let mut disks = self
            .disks
            .list()
            .iter()
            .filter(|d| d.total_space() > 0)
            .collect::<Vec<_>>();
        disks.sort_by_key(|d| d.mount_point());
        for disk in disks {
            let mount = disk.mount_point().to_string_lossy();
            // Container bind mounts can expose host devices unrelated to this workload.
            if self.workload && mount != "/" {
                continue;
            }
            let k = id(&mount);
            let label = if mount == "/" {
                "Root volume"
            } else {
                mount.as_ref()
            };
            let total = disk.total_space();
            let used = total.saturating_sub(disk.available_space());
            let ds = if self.workload {
                "backing filesystem"
            } else {
                "volume"
            };
            for (field, unit, value) in [
                ("usage", "%", 100.0 * used as f64 / total as f64),
                ("used", "B", used as f64),
                ("capacity", "B", total as f64),
            ] {
                m.push(Metric::new(
                    format!("storage.{k}.{field}"),
                    format!("{label} {field}"),
                    "storage",
                    unit,
                    value,
                    ds,
                ));
            }
            if !self.workload {
                for (field, bytes) in [
                    ("read", disk.usage().read_bytes),
                    ("write", disk.usage().written_bytes),
                ] {
                    m.push(Metric::new(
                        format!("storage.{k}.{field}"),
                        format!("{label} {field}"),
                        "storage",
                        "B/s",
                        bytes as f64 / dt,
                        ds,
                    ));
                }
            }
        }
        let mut download = 0u64;
        let mut upload = 0u64;
        let mut has_network = false;
        for (name, net) in &self.nets {
            let addresses = net.ip_networks();
            if !matches!(name.as_str(), "lo" | "lo0")
                && !name.to_ascii_lowercase().contains("loopback")
                && (addresses.is_empty()
                    || addresses.iter().any(|address| !address.addr.is_loopback()))
            {
                has_network = true;
                download = download.saturating_add(net.received());
                upload = upload.saturating_add(net.transmitted());
            }
            if net.total_received() == 0 && net.total_transmitted() == 0 {
                continue;
            }
            let interface = id(name);
            for (field, bytes) in [("rx", net.received()), ("tx", net.transmitted())] {
                m.push(Metric::new(
                    format!("network.{interface}.{field}"),
                    format!("{name} {field}"),
                    "network",
                    "B/s",
                    bytes as f64 / dt,
                    scope,
                ));
            }
        }
        for (key, label, bytes) in [
            ("network.download", "Download", download),
            ("network.upload", "Upload", upload),
        ] {
            m.push(if has_network {
                Metric::new(
                    key,
                    label,
                    "network",
                    "B/s",
                    bytes as f64 / dt,
                    "Non-loopback interfaces",
                )
            } else {
                Metric::missing(
                    key,
                    label,
                    "network",
                    "B/s",
                    "Non-loopback interfaces",
                    "No non-loopback network interfaces reported.",
                )
            });
        }
        for c in self.temps.list() {
            if let Some(value) = c
                .temperature()
                .filter(|v| v.is_finite() && *v > -100. && *v < 250.)
            {
                m.push(Metric::new(
                    format!("temperature.{}", id(c.label())),
                    c.label(),
                    "temperature",
                    "°C",
                    value as f64,
                    "host",
                ));
            }
        }
        m.extend(self.extended.read());
        // Estimate session energy per sensor, without bridging outages or double-counting domains.
        let mut estimates = Vec::new();
        let max_energy_gap =
            (self.interval * 2 + Duration::from_secs(4)).max(Duration::from_secs(8));
        for metric in m.iter().filter(|v| v.unit == "W") {
            if let Some(watts) = metric.value {
                let (prev, last, wh) = self
                    .energy
                    .entry(metric.key.clone())
                    .or_insert((watts, now, 0.));
                let elapsed = now.duration_since(*last);
                if elapsed < max_energy_gap {
                    *wh += (watts + *prev) * 0.5 * elapsed.as_secs_f64() / 3600.;
                }
                *prev = watts;
                *last = now;
                estimates.push(Metric::new(
                    format!("{}.energy", metric.key),
                    format!("{} · session energy estimate", metric.label),
                    "power",
                    "Wh",
                    *wh,
                    metric.scope,
                ));
            }
        }
        m.extend(estimates);
        for (group, key, label, unit, reason) in [
            (
                "gpu",
                "gpu.usage",
                "GPU utilization",
                "%",
                "No supported GPU usage counter is accessible.",
            ),
            (
                "temperature",
                "temperature.sensor",
                "Temperature",
                "°C",
                "No readable temperature sensor was reported.",
            ),
            (
                "power",
                "power.draw",
                "Power draw",
                "W",
                "No readable hardware power counter was reported.",
            ),
        ] {
            if !m.iter().any(|v| v.group == group && v.unit == unit) {
                m.push(Metric::missing(key, label, group, unit, "host", reason));
            }
        }
        let mut keys = std::collections::HashSet::new();
        m.retain(|v| keys.insert(v.key.clone()));
        if let Some(discovery) = &self.discovery {
            m.extend(
                discovery
                    .read()
                    .into_iter()
                    .take(4096usize.saturating_sub(m.len())),
            );
        }
        Sample {
            at: OffsetDateTime::now_utc(),
            metrics: m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_propagates_interval_to_energy_window_and_subsystems() {
        let mut collector = Collector::new(true, false, false, Duration::from_secs(30));
        assert_eq!(collector.interval, Duration::from_secs(30));
        collector.set_interval(Duration::from_secs(45));
        assert_eq!(collector.interval, Duration::from_secs(45));

        let ping = crate::network::Ping::start("http://127.0.0.1:1", Duration::from_secs(30));
        assert_eq!(ping.current_interval(), Duration::from_secs(30));
        ping.set_interval(Duration::from_secs(1));
        assert_eq!(ping.current_interval(), Duration::from_secs(1));
    }
}
