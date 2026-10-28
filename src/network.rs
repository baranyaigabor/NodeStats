use crate::metrics::Metric;
use std::{
    net::TcpStream,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub struct Ping {
    cache: Arc<Mutex<Option<(Instant, f64)>>>,
    interval: Arc<(Mutex<Duration>, Condvar)>,
}

impl Default for Ping {
    fn default() -> Self {
        Self {
            cache: Arc::default(),
            interval: Arc::new((Mutex::new(Duration::from_secs(5)), Condvar::new())),
        }
    }
}

impl Ping {
    pub fn start(server: &str, initial_interval: Duration) -> Self {
        let ping = Self {
            cache: Arc::default(),
            interval: Arc::new((Mutex::new(initial_interval), Condvar::new())),
        };
        let Ok(url) = url::Url::parse(server) else {
            return ping;
        };
        let cache = Arc::downgrade(&ping.cache);
        let ticker = Arc::clone(&ping.interval);
        thread::spawn(move || {
            while cache.strong_count() > 0 {
                let measured = url.socket_addrs(|| None).ok().and_then(|addresses| {
                    addresses.into_iter().take(4).find_map(|address| {
                        let start = Instant::now();
                        TcpStream::connect_timeout(&address, Duration::from_millis(700))
                            .ok()
                            .map(|_| (Instant::now(), start.elapsed().as_secs_f64() * 1000.))
                    })
                });
                let Some(reader) = cache.upgrade() else {
                    return;
                };
                *reader.lock().unwrap() = measured;
                drop(reader);
                let (lock, condvar) = &*ticker;
                let guard = lock.lock().unwrap();
                let wait = (*guard).clamp(Duration::from_secs(1), Duration::from_secs(60));
                let _ = condvar.wait_timeout(guard, wait);
            }
        });
        ping
    }

    pub fn set_interval(&self, interval: Duration) {
        let (lock, condvar) = &*self.interval;
        *lock.lock().unwrap() = interval;
        condvar.notify_all();
    }

    #[cfg(test)]
    pub fn current_interval(&self) -> Duration {
        *self.interval.0.lock().unwrap()
    }

    pub fn reading(&self) -> Metric {
        let interval = *self.interval.0.lock().unwrap();
        let max_age = (interval * 3).max(Duration::from_secs(15));
        let value = self
            .cache
            .lock()
            .unwrap()
            .filter(|(at, _)| at.elapsed() < max_age);
        match value {
            Some((_, ms)) => Metric::new(
                "network.ping",
                "Ping",
                "network",
                "ms",
                ms,
                "TCP to the NodeStats server",
            ),
            None => Metric::missing(
                "network.ping",
                "Ping",
                "network",
                "ms",
                "TCP to the NodeStats server",
                "No recent TCP response from the NodeStats server.",
            ),
        }
    }
}

pub fn buffer() -> Metric {
    let scope = match std::env::consts::OS {
        "linux" => "Memory allocated to TCP buffers",
        _ => "Memory allocated to network buffers",
    };
    match buffer_bytes() {
        Some(bytes) => Metric::new(
            "network.buffer",
            "Net buffer",
            "network",
            "B",
            bytes as f64,
            scope,
        ),
        None => Metric::missing(
            "network.buffer",
            "Net buffer",
            "network",
            "B",
            scope,
            "Network buffer counters are unavailable on this node.",
        ),
    }
}

#[cfg(target_os = "linux")]
fn buffer_bytes() -> Option<u64> {
    let stats = std::fs::read_to_string("/proc/net/sockstat").ok()?;
    let tcp = stats.lines().find_map(|line| line.strip_prefix("TCP:"))?;
    let mut fields = tcp.split_ascii_whitespace();
    let pages: u64 = fields
        .find(|field| *field == "mem")
        .and_then(|_| fields.next())?
        .parse()
        .ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    pages.checked_mul(page_size as u64)
}

#[cfg(target_os = "macos")]
fn buffer_bytes() -> Option<u64> {
    let output = crate::platform::probe("/usr/sbin/netstat", &["-m"])?;
    macos_buffer_bytes(&String::from_utf8_lossy(&output))
}

#[cfg(target_os = "macos")]
fn macos_buffer_bytes(output: &str) -> Option<u64> {
    let mut fields = output
        .lines()
        .find(|line| line.contains(" allocated to network "))?
        .split_ascii_whitespace();
    let amount: u64 = fields.next()?.parse().ok()?;
    let multiplier = match fields.next()? {
        "B" => 1,
        "KB" => 1024,
        "MB" => 1024 * 1024,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn buffer_bytes() -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn reads_macos_network_buffer_allocation_in_bytes() {
        let output = "333/1369 mbufs in use:\n29878 KB allocated to network (1.9% in use)";
        assert_eq!(super::macos_buffer_bytes(output), Some(30_595_072));
        assert_eq!(super::macos_buffer_bytes("unavailable"), None);
    }
}
