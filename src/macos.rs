use crate::{
    metrics::{Metric, id},
    platform::probe,
};
use plist::{Dictionary, Value};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    io::{self, BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

type CfRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfRef;
    fn IOServiceGetMatchingServices(port: u32, matching: CfRef, iterator: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut CfRef,
        allocator: CfRef,
        options: u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: CfRef);
    fn CFStringCreateWithCString(allocator: CfRef, text: *const c_char, encoding: u32) -> CfRef;
    fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
    fn CFPropertyListCreateData(
        allocator: CfRef,
        value: CfRef,
        format: isize,
        options: usize,
        error: *mut CfRef,
    ) -> CfRef;
    fn CFDataGetLength(data: CfRef) -> isize;
    fn CFDataGetBytePtr(data: CfRef) -> *const u8;
}

struct CfOwned(CfRef);
impl CfOwned {
    fn new(reference: CfRef) -> Option<Self> {
        (!reference.is_null()).then(|| Self(reference))
    }
}
impl Drop for CfOwned {
    fn drop(&mut self) {
        // Each wrapper owns one Create-rule reference.
        unsafe { CFRelease(self.0) }
    }
}
struct IoObject(u32);
impl Drop for IoObject {
    fn drop(&mut self) {
        // Iterators and every nonzero result of IOIteratorNext are owned handles.
        unsafe { IOObjectRelease(self.0) };
    }
}

fn native_registry(class: &CStr, keys: &[(&str, &CStr)]) -> Option<Vec<Dictionary>> {
    let null = std::ptr::null();
    // Matching consumes the dictionary, including on failure. All other references
    // stay alive until their values have been copied into owned Rust data.
    unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return None;
        }
        let mut handle = 0;
        if IOServiceGetMatchingServices(0, matching, &mut handle) != 0 || handle == 0 {
            return None;
        }
        let iterator = IoObject(handle);
        let mut devices = Vec::new();
        loop {
            let entry = IOIteratorNext(iterator.0);
            if entry == 0 {
                break;
            }
            let entry = IoObject(entry);
            if !devices.is_empty() {
                // Retain ioreg's ordering for stable indexed GPU keys.
                return None;
            }
            let mut properties = null;
            if IORegistryEntryCreateCFProperties(entry.0, &mut properties, null, 0) != 0 {
                return None;
            }
            let properties = CfOwned::new(properties)?;
            let mut device = Dictionary::new();
            for (name, key) in keys {
                let key = CfOwned::new(CFStringCreateWithCString(null, key.as_ptr(), 0x0800_0100))?;
                let value = CFDictionaryGetValue(properties.0, key.0);
                if value.is_null() {
                    continue;
                }
                let data = CfOwned::new(CFPropertyListCreateData(
                    null,
                    value,
                    200,
                    0,
                    std::ptr::null_mut(),
                ))?;
                let len = CFDataGetLength(data.0);
                if !(1..=1024 * 1024).contains(&len) {
                    return None;
                }
                let ptr = CFDataGetBytePtr(data.0);
                if ptr.is_null() {
                    return None;
                }
                let bytes = std::slice::from_raw_parts(ptr, len as usize);
                device.insert(
                    (*name).into(),
                    Value::from_reader(std::io::Cursor::new(bytes)).ok()?,
                );
            }
            if device.is_empty() {
                return None;
            }
            devices.push(device);
        }
        (!devices.is_empty()).then_some(devices)
    }
}

const GRAPHICS_KEYS: &[(&str, &CStr)] = &[("PerformanceStatistics", c"PerformanceStatistics")];
const BATTERY_KEYS: &[(&str, &CStr)] = &[
    ("PowerTelemetryData", c"PowerTelemetryData"),
    ("CurrentCapacity", c"CurrentCapacity"),
    ("InstantAmperage", c"InstantAmperage"),
    ("Voltage", c"Voltage"),
];

fn registry(class: &str) -> Vec<Dictionary> {
    probe("/usr/sbin/ioreg", &["-rc", class, "-d", "1", "-a"])
        .and_then(|b| Value::from_reader_xml(b.as_slice()).ok())
        .and_then(Value::into_array)
        .unwrap_or_default()
        .into_iter()
        .filter_map(Value::into_dictionary)
        .collect()
}
fn number(v: &Value) -> Option<f64> {
    v.as_real()
        .or_else(|| v.as_signed_integer().map(|i| i as f64))
        .or_else(|| v.as_unsigned_integer().map(|i| i as f64))
}
pub fn sensors() -> Vec<Metric> {
    let mut m = Vec::new();
    for (i, dev) in native_registry(c"IOAccelerator", GRAPHICS_KEYS)
        .unwrap_or_else(|| registry("IOAccelerator"))
        .iter()
        .enumerate()
    {
        let Some(stats) = dev
            .get("PerformanceStatistics")
            .and_then(Value::as_dictionary)
        else {
            continue;
        };
        for (prop, key, label, unit) in [
            ("Device Utilization %", "usage", "GPU utilization", "%"),
            ("Renderer Utilization %", "renderer", "Renderer engine", "%"),
            ("Tiler Utilization %", "tiler", "Tiler engine", "%"),
            ("In use system memory", "memory", "GPU memory in use", "B"),
            (
                "Alloc system memory",
                "allocated",
                "GPU memory allocated",
                "B",
            ),
        ] {
            if let Some(v) = stats.get(prop).and_then(number) {
                m.push(Metric::new(
                    format!("gpu.{i}.{key}"),
                    label,
                    "gpu",
                    unit,
                    v,
                    "host GPU",
                ));
            }
        }
    }
    for b in native_registry(c"AppleSmartBattery", BATTERY_KEYS)
        .unwrap_or_else(|| registry("AppleSmartBattery"))
    {
        if let Some(stats) = b.get("PowerTelemetryData").and_then(Value::as_dictionary) {
            for (prop, key, label) in [
                ("SystemPowerIn", "input", "System input power"),
                ("SystemLoad", "load", "System load power"),
            ] {
                if let Some(v) = stats
                    .get(prop)
                    .and_then(number)
                    .filter(|v| *v >= 0. && *v < 2_000_000.)
                {
                    m.push(Metric::new(
                        format!("power.system.{key}"),
                        label,
                        "power",
                        "W",
                        v / 1000.,
                        "host",
                    ));
                }
            }
        }
        if let Some(v) = b
            .get("CurrentCapacity")
            .and_then(number)
            .filter(|v| *v <= 100.)
        {
            m.push(Metric::new(
                "system.battery",
                "Battery charge",
                "system",
                "%",
                v,
                "battery",
            ));
        }
        if let Some(v) = b.get("Voltage").and_then(number) {
            m.push(Metric::new(
                "power.battery.voltage",
                "Battery voltage",
                "power",
                "V",
                v / 1000.,
                "battery",
            ));
        }
        if let (Some(ma), Some(mv)) = (
            b.get("InstantAmperage").and_then(|v| {
                v.as_unsigned_integer()
                    .map(|i| i as i64)
                    .or_else(|| v.as_signed_integer())
            }),
            b.get("Voltage").and_then(number),
        ) {
            m.push(Metric::new(
                "power.battery.flow",
                "Battery power (+ charging)",
                "power",
                "W",
                ma as f64 * mv / 1e6,
                "battery",
            ));
        }
    }
    m
}

/// Metal HUD logs expose the first frame number of each batch even when macOS
/// redacts the per-frame timings. Compare consecutive batches from the same process.
fn metal_fps(log: &[u8]) -> Option<f64> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry<'a> {
        #[serde(rename = "processID")]
        process_id: u64,
        timestamp: &'a str,
        event_message: &'a str,
    }

    let timestamp_format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond][offset_hour sign:mandatory][offset_minute]",
    )
    .expect("valid unified log timestamp format");
    let mut previous = HashMap::new();
    let mut latest = None;
    for line in log.split(|byte| *byte == b'\n') {
        let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
            continue;
        };
        let Some(frame) = entry
            .event_message
            .strip_prefix("metal-HUD: ")
            .and_then(|message| message.split_once(','))
            .and_then(|(frame, _)| frame.parse::<u64>().ok())
        else {
            continue;
        };
        let Ok(at) = time::OffsetDateTime::parse(entry.timestamp, &timestamp_format) else {
            continue;
        };
        if let Some((old_at, old_frame)) = previous.insert(entry.process_id, (at, frame)) {
            let elapsed = (at - old_at).as_seconds_f64();
            if let Some(frames) = frame.checked_sub(old_frame)
                && frames > 0
                && (0.25..=2.5).contains(&elapsed)
            {
                latest = Some(frames as f64 / elapsed);
            }
        }
    }
    latest
}

pub fn fps_reading() -> Metric {
    let fps = probe(
        "/usr/bin/log",
        &[
            "show",
            "--last",
            "4s",
            "--info",
            "--style",
            "ndjson",
            "--predicate",
            r#"subsystem == "com.apple.metal.hud" AND category == "Logging" AND eventMessage BEGINSWITH "metal-HUD:""#,
        ],
    )
    .and_then(|log| metal_fps(&log));
    let scope = "Most recent app reporting Metal HUD frames";
    if let Some(fps) = fps {
        Metric::new("gpu.metal.fps", "Metal app FPS", "gpu", "FPS", fps, scope)
    } else {
        Metric::missing(
            "gpu.metal.fps",
            "Metal app FPS",
            "gpu",
            "FPS",
            scope,
            "Waiting for an app launched with Metal HUD logging enabled.",
        )
    }
}

type PowerCache = Arc<Mutex<HashMap<String, (Instant, Metric)>>>;
pub struct Power {
    cache: PowerCache,
    declined: bool,
}
impl Power {
    pub fn start(interactive: bool, interval: Duration) -> Self {
        Self::start_with_input(interactive, interval, io::stdin().lock())
    }
    fn start_with_input(interactive: bool, interval: Duration, mut input: impl BufRead) -> Self {
        let cache = PowerCache::default();
        if interactive {
            eprint!(
                "Allow macOS to request administrator permission for CPU and GPU power readings? [Y/n] "
            );
            let _ = io::stderr().flush();
            let mut answer = String::new();
            let agreed = matches!(input.read_line(&mut answer), Ok(n) if n > 0)
                && matches!(
                    answer.trim().to_ascii_lowercase().as_str(),
                    "" | "y" | "yes"
                );
            if !agreed {
                return Self {
                    cache,
                    declined: true,
                };
            }
        }
        let mut cmd = Command::new("/usr/bin/sudo");
        if !interactive {
            cmd.arg("-n");
        }
        let out = Arc::downgrade(&cache);
        let interval_ms = interval.as_millis().clamp(1000, 5000).to_string();
        thread::spawn(move || {
            let mut cmd = cmd;
            cmd.args([
                "/usr/bin/powermetrics",
                "-b",
                "1",
                "-i",
                &interval_ms,
                "-s",
                "cpu_power,gpu_power",
            ])
            .stdout(Stdio::piped());
            if !interactive {
                cmd.stderr(Stdio::null());
            }
            let Ok(mut child) = cmd.spawn() else {
                return;
            };
            if let Some(stdout) = child.stdout.take() {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Some(cache) = out.upgrade() else { break };
                    if let Some(metric) = power_line(&line) {
                        cache
                            .lock()
                            .unwrap()
                            .insert(metric.key.clone(), (Instant::now(), metric));
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        });
        Self {
            cache,
            declined: false,
        }
    }
    pub fn read(&self) -> Vec<Metric> {
        let mut m = self
            .cache
            .lock()
            .unwrap()
            .values()
            .filter(|(t, _)| t.elapsed() < Duration::from_secs(12))
            .map(|(_, m)| m.clone())
            .collect::<Vec<_>>();
        if m.is_empty() {
            let reason = if self.declined {
                "Administrator permission for chip power readings was declined."
            } else {
                "The macOS powermetrics sampler needs administrator approval and compatible hardware."
            };
            m.push(Metric::missing(
                "power.soc",
                "Chip power",
                "power",
                "W",
                "host",
                reason,
            ));
        }
        m
    }
}
fn power_line(line: &str) -> Option<Metric> {
    let (label, reading) = line.trim().split_once(':')?;
    if ![
        "CPU Power",
        "GPU Power",
        "ANE Power",
        "Package Power",
        "Combined Power (CPU + GPU + ANE)",
    ]
    .contains(&label)
    {
        return None;
    }
    let mut parts = reading.split_whitespace();
    let amount: f64 = parts.next()?.parse().ok()?;
    let watts = match parts.next()? {
        "mW" => amount / 1000.,
        "W" => amount,
        _ => return None,
    };
    Some(Metric::new(
        format!("power.soc.{}", id(label)),
        label,
        "power",
        "W",
        watts,
        "host chip",
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn calculates_metal_fps_from_redacted_batches_per_process() {
        let log = br#"
            {"processID":4,"timestamp":"2026-11-23 19:00:00.000000+0100","eventMessage":"metal-HUD: 30,3,175<private>"}
            {"processID":5,"timestamp":"2026-11-23 19:00:00.500000+0100","eventMessage":"metal-HUD: 90,3,175<private>"}
            {"processID":4,"timestamp":"2026-11-23 19:00:01.000000+0100","eventMessage":"metal-HUD: 60,3,175<private>"}
        "#;
        assert_eq!(super::metal_fps(log), Some(30.0));
        assert_eq!(super::metal_fps(b""), None);
        assert_eq!(
            super::metal_fps(log.split(|byte| *byte == b'\n').nth(1).unwrap()),
            None
        );
    }

    #[test]
    fn declined_or_missing_input_skips_privileged_sampler() {
        for input in ["n\n", "no\n", "invalid\n", ""] {
            let power = super::Power::start_with_input(
                true,
                std::time::Duration::from_secs(1),
                input.as_bytes(),
            );
            assert!(power.declined);
            assert!(power.read()[0].reason.unwrap().contains("declined"));
        }
    }
    #[test]
    fn converts_milliwatts() {
        assert_eq!(
            super::power_line("CPU Power: 987 mW").unwrap().value,
            Some(0.987)
        );
    }
}
