use crate::{metrics::Metric, platform::probe};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Usage {
    cpu: Option<f64>,
    cpu_limit: Option<f64>,
    memory: Option<f64>,
    memory_limit: Option<f64>,
}

struct Workload {
    kind: &'static str,
    id: String,
    name: String,
    running: bool,
    compose_project: Option<String>,
    compose_service: Option<String>,
    usage: Usage,
}

impl Workload {
    fn readings(self, host_cpu: f64, host_memory: f64) -> Vec<Metric> {
        let (kind, scope, aggregate) = match self.kind {
            "docker-runtime" => ("docker", "Docker runtime", true),
            "docker-project" => ("docker", "Docker Compose project", true),
            "docker-service" => ("docker", "Docker service", true),
            "kube-cluster" => ("kubernetes", "Kubernetes cluster", true),
            "kube-namespace" => ("kubernetes", "Kubernetes namespace", true),
            "kube-service" => ("kubernetes", "Kubernetes service", true),
            "deployment" => ("kubernetes", "Kubernetes deployment", true),
            "kube-container" => ("kubernetes", "Kubernetes container", false),
            kind => (kind, "Workload inventory", false),
        };
        let prefix = format!("workload.{kind}.{}", self.id);
        let u = self.usage;
        let percent =
            |used: Option<f64>, limit: Option<f64>| Some(100. * used? / limit.filter(|v| *v > 0.)?);
        let mut inventory = Metric::new(
            format!("{prefix}.present"),
            self.name.chars().take(160).collect::<String>(),
            "system",
            "",
            if self.running { 1. } else { 0. },
            scope,
        );
        inventory.compose_project = self.compose_project;
        inventory.compose_service = self.compose_service;
        let mut metrics = vec![inventory];
        if aggregate {
            for (resource, label, unit, value) in [
                ("cpu", "CPU in use", "cores", u.cpu),
                ("memory", "Memory in use", "B", u.memory),
            ] {
                if let Some(value) = value {
                    metrics.push(Metric::new(
                        format!("{prefix}.{resource}.used"),
                        label,
                        resource,
                        unit,
                        value,
                        scope,
                    ));
                }
            }
            return metrics;
        }
        for (suffix, label, group, unit, value) in [
            ("cpu.used", "CPU in use", "cpu", "cores", u.cpu),
            ("cpu.limit", "CPU allocation", "cpu", "cores", u.cpu_limit),
            (
                "cpu.allocated",
                "CPU utilization",
                "cpu",
                "%",
                percent(u.cpu, u.cpu_limit),
            ),
            (
                "cpu.host",
                "CPU share of host",
                "cpu",
                "%",
                percent(u.cpu, Some(host_cpu)),
            ),
            ("memory.used", "Memory in use", "memory", "B", u.memory),
            (
                "memory.limit",
                "Memory allocation",
                "memory",
                "B",
                u.memory_limit,
            ),
            (
                "memory.allocated",
                "Memory utilization",
                "memory",
                "%",
                percent(u.memory, u.memory_limit),
            ),
            (
                "memory.host",
                "Memory share of host",
                "memory",
                "%",
                percent(u.memory, Some(host_memory)),
            ),
        ] {
            let key = format!("{prefix}.{suffix}");
            metrics.push(match value.filter(|v| v.is_finite() && *v >= 0.) {
                Some(v) => Metric::new(key, label, group, unit, v, "Workload"),
                None => Metric::missing(
                    &key,
                    label,
                    group,
                    unit,
                    "Workload",
                    "Usage or capacity is unavailable from this runtime.",
                ),
            });
        }
        metrics
    }
}

fn summed_usage<'a>(usages: impl IntoIterator<Item = &'a Usage>) -> Usage {
    let mut cpu = Some(0.);
    let mut memory = Some(0.);
    for usage in usages {
        cpu = cpu.zip(usage.cpu).map(|(sum, value)| sum + value);
        memory = memory.zip(usage.memory).map(|(sum, value)| sum + value);
    }
    Usage {
        cpu,
        memory,
        ..Usage::default()
    }
}

fn aggregate<'a>(
    kind: &'static str,
    id: String,
    name: String,
    running: bool,
    members: impl IntoIterator<Item = &'a Workload>,
) -> Workload {
    let usage = summed_usage(
        members
            .into_iter()
            .filter(|item| item.running)
            .map(|item| &item.usage),
    );
    Workload {
        kind,
        id,
        name,
        running,
        usage,
        compose_project: None,
        compose_service: None,
    }
}

fn docker_aggregates(mut rows: Vec<Workload>) -> Vec<Workload> {
    let mut projects: HashMap<&str, Vec<&Workload>> = HashMap::new();
    let mut services: HashMap<(&str, &str), Vec<&Workload>> = HashMap::new();
    for row in &rows {
        if let Some(project) = row.compose_project.as_deref() {
            projects.entry(project).or_default().push(row);
        }
        if let (Some(project), Some(service)) = (&row.compose_project, &row.compose_service) {
            services.entry((project, service)).or_default().push(row);
        }
    }
    let mut summaries = Vec::with_capacity(projects.len() + services.len() + 1);
    summaries.push(aggregate(
        "docker-runtime",
        vm_id("docker", "runtime"),
        "Docker".into(),
        true,
        &rows,
    ));
    for (project, members) in projects {
        let mut summary = aggregate(
            "docker-project",
            vm_id("compose-project", project),
            project.into(),
            members.iter().any(|member| member.running),
            members,
        );
        summary.compose_project = Some(project.into());
        summaries.push(summary);
    }
    for ((project, service), members) in services {
        let label = format!("{project} / {service}");
        let mut summary = aggregate(
            "docker-service",
            vm_id("compose", &label),
            label,
            members.iter().any(|member| member.running),
            members,
        );
        summary.compose_project = Some(project.into());
        summary.compose_service = Some(service.into());
        summaries.push(summary);
    }
    rows.extend(summaries);
    rows
}

type Cache = Arc<Mutex<Option<(Instant, Vec<Metric>)>>>;
pub struct Discovery {
    cache: Cache,
    interval: Arc<(Mutex<Duration>, Condvar)>,
}
impl Discovery {
    pub fn new(cpu: f64, memory: f64, initial_interval: Duration) -> Self {
        let cache = Cache::default();
        let interval = Arc::new((Mutex::new(initial_interval), Condvar::new()));
        let reader = Arc::downgrade(&cache);
        let ticker = Arc::clone(&interval);
        thread::spawn(move || {
            let docker_cli = tool("docker");
            let kube_cli = tool("kubectl");
            let multipass = tool("multipass");
            let libvirt = tool("virsh");
            let mut vm_cpu = HashMap::new();
            while reader.strong_count() > 0 {
                let result = docker_cli
                    .as_deref()
                    .into_iter()
                    .flat_map(docker)
                    .chain(kube_cli.as_deref().into_iter().flat_map(kubernetes))
                    .chain(multipass.as_deref().into_iter().flat_map(virtual_machines))
                    .chain(
                        libvirt
                            .as_deref()
                            .into_iter()
                            .flat_map(|cli| libvirt_machines(cli, &mut vm_cpu)),
                    )
                    .flat_map(|workload| workload.readings(cpu, memory))
                    .collect();
                if let Some(cache) = reader.upgrade() {
                    *cache.lock().unwrap() = Some((Instant::now(), result));
                } else {
                    break;
                }
                let (lock, condvar) = &*ticker;
                let guard = lock.lock().unwrap();
                let wait = (*guard).clamp(Duration::from_secs(2), Duration::from_secs(60));
                let _ = condvar.wait_timeout(guard, wait);
            }
        });
        Self { cache, interval }
    }

    pub fn set_interval(&self, interval: Duration) {
        let (lock, condvar) = &*self.interval;
        *lock.lock().unwrap() = interval;
        condvar.notify_all();
    }

    pub fn read(&self) -> Vec<Metric> {
        let max_age = (*self.interval.0.lock().unwrap() * 3).max(Duration::from_secs(60));
        self.cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(at, _)| at.elapsed() < max_age)
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default()
    }
}

pub(crate) fn tool(name: &str) -> Option<String> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let paths = [
        Path::new("/opt/homebrew/bin").join(&executable),
        Path::new("/usr/local/bin").join(&executable),
        Path::new("/usr/bin").join(&executable),
        Path::new("/snap/bin").join(&executable),
        Path::new("/Applications/Docker.app/Contents/Resources/bin").join(&executable),
        Path::new("/Library/Application Support/com.canonical.multipass/bin").join(&executable),
    ];
    paths
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| p.into_os_string().into_string().ok())
        .or_else(|| {
            if !cfg!(windows) {
                return None;
            }
            let root = std::env::var_os("ProgramFiles")?;
            let base = Path::new(&root);
            [
                base.join("Docker/Docker/resources/bin").join(&executable),
                base.join("Multipass/bin").join(&executable),
            ]
            .into_iter()
            .find(|p| p.is_absolute() && p.is_file())
            .and_then(|p| p.into_os_string().into_string().ok())
        })
}
pub(crate) fn output(program: &str, args: &[&str]) -> Option<String> {
    String::from_utf8(probe(program, args)?).ok()
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn local_docker_endpoint(value: &str) -> bool {
    value.starts_with("unix:///") || value.starts_with("npipe:////./pipe/")
}
pub(crate) fn docker_is_local(cli: &str) -> bool {
    let endpoint = if std::env::var_os("DOCKER_CONTEXT").is_none() {
        std::env::var("DOCKER_HOST").ok()
    } else {
        None
    };
    let endpoint = endpoint.or_else(|| {
        output(
            cli,
            &[
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ],
        )
    });
    endpoint
        .as_deref()
        .is_some_and(|value| local_docker_endpoint(value.trim()))
}
fn compose_label(encoded: &str) -> Option<String> {
    serde_json::from_str::<String>(encoded)
        .ok()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 160
                && value
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch))
        })
}

fn docker(cli: &str) -> Vec<Workload> {
    if !docker_is_local(cli) {
        return vec![];
    }
    let Some(list) = output(
        cli,
        &[
            "ps",
            "--all",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.State}}\t{{json (.Label \"com.docker.compose.project\")}}\t{{json (.Label \"com.docker.compose.service\")}}",
        ],
    ) else {
        return vec![];
    };
    let mut rows = list
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?;
            let name = fields.next()?;
            let running = fields.next()? == "running";
            let compose_project = fields.next().and_then(compose_label);
            let compose_service = fields.next().and_then(compose_label);
            valid_id(id).then(|| Workload {
                kind: "docker",
                id: id.into(),
                name: name.into(),
                running,
                compose_project,
                compose_service,
                usage: Usage::default(),
            })
        })
        .take(100)
        .collect::<Vec<_>>();
    let ids = rows
        .iter()
        .filter(|w| w.running)
        .map(|w| w.id.as_str())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return docker_aggregates(rows);
    }
    let mut stats = vec![
        "stats",
        "--no-stream",
        "--format",
        "{{.ID}}\t{{.CPUPerc}}\t{{.MemUsage}}",
    ];
    stats.extend(&ids);
    let mut inspect = vec![
        "inspect",
        "--type",
        "container",
        "--format",
        "{{.Id}}\t{{.HostConfig.NanoCpus}}\t{{.HostConfig.CpuQuota}}\t{{.HostConfig.CpuPeriod}}\t{{.HostConfig.CpusetCpus}}",
    ];
    inspect.extend(&ids);
    let stats = output(cli, &stats).unwrap_or_default();
    let limits = output(cli, &inspect).unwrap_or_default();
    let runtime_cpus = output(cli, &["info", "--format", "{{.NCPU}}"])
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.);
    let mut stats = stats
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect::<HashMap<_, _>>();
    for row in &mut rows {
        if let Some((cpu, memory)) = stats
            .remove(row.id.as_str())
            .and_then(|line| line.split_once('\t'))
        {
            row.usage.cpu = cpu
                .trim_end_matches('%')
                .parse::<f64>()
                .ok()
                .map(|p| p / 100.);
            if let Some((used, limit)) = memory.split_once('/') {
                row.usage.memory = bytes(used);
                row.usage.memory_limit = bytes(limit);
            }
        }
        if let Some(fields) = limits.lines().find_map(|line| {
            let (id, fields) = line.split_once('\t')?;
            id.starts_with(&row.id).then_some(fields)
        }) {
            let mut fields = fields.split('\t');
            let nano = fields
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default();
            let quota = fields
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or_default();
            let period = fields
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| *v > 0.)
                .unwrap_or(100_000.);
            let set = fields.next().and_then(cpu_set_count).map(f64::from);
            row.usage.cpu_limit = [Some(nano / 1e9), Some(quota / period), set, runtime_cpus]
                .into_iter()
                .flatten()
                .filter(|value| *value > 0.)
                .reduce(f64::min);
        }
    }
    docker_aggregates(rows)
}
fn cpu_set_count(set: &str) -> Option<u32> {
    set.split(',').try_fold(0u32, |sum, part| {
        let count = if let Some((a, b)) = part.split_once('-') {
            b.parse::<u32>()
                .ok()?
                .checked_sub(a.parse().ok()?)?
                .checked_add(1)?
        } else {
            part.parse::<u32>().ok()?;
            1
        };
        sum.checked_add(count)
    })
}
fn bytes(raw: &str) -> Option<f64> {
    let text = raw.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let val = text[..split].parse::<f64>().ok()?;
    let factor = match text[split..].trim() {
        "" | "B" => 1.,
        "Ki" | "KiB" => 1024.,
        "Mi" | "MiB" => 1048576.,
        "Gi" | "GiB" => 1073741824.,
        "Ti" | "TiB" => 1099511627776.,
        "k" | "kB" | "KB" => 1e3,
        "M" | "MB" => 1e6,
        "G" | "GB" => 1e9,
        "T" | "TB" => 1e12,
        _ => return None,
    };
    Some(val * factor)
}
fn cores(raw: &str) -> Option<f64> {
    for (suffix, divisor) in [('m', 1e3), ('u', 1e6), ('n', 1e9)] {
        if let Some(val) = raw.strip_suffix(suffix) {
            return Some(val.parse::<f64>().ok()? / divisor);
        }
    }
    raw.parse().ok()
}

pub(crate) fn local_kube_server(cli: &str) -> Option<String> {
    let server = output(
        cli,
        &[
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ],
    )?;
    let local = url::Url::parse(server.trim()).ok().is_some_and(|url| {
        matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
            && url.username().is_empty()
            && url.password().is_none()
    });
    if !local {
        return None;
    }
    let server = server.trim();
    let request = "--request-timeout=3s";
    let nodes = output(
        cli,
        &["--server", server, request, "get", "nodes", "-o", "name"],
    )?;
    if nodes.lines().count() != 1 {
        return None;
    }
    Some(server.into())
}
pub(crate) const KUBE_DEPLOYMENT_INVENTORY: &str = r#"jsonpath={range .items[*]}{.metadata.uid}{"\t"}{.metadata.namespace}{"\t"}{.metadata.name}{"\t"}{.spec.replicas}{"\n"}{end}"#;
pub(crate) const KUBE_POD_IDS: &str = r#"jsonpath={range .items[*]}{.metadata.uid}{"\t"}{.metadata.namespace}{"\t"}{.metadata.name}{"\n"}{end}"#;
fn kubernetes(cli: &str) -> Vec<Workload> {
    let Some(server) = local_kube_server(cli) else {
        return vec![];
    };
    let root = ["--server", &server, "--request-timeout=3s"];
    let inventory = |resource| {
        let mut args = root.to_vec();
        args.extend(["get", resource, "-A", "-o", "json"]);
        output(cli, &args).and_then(|json| serde_json::from_str::<Value>(&json).ok())
    };
    let Some(pods) = inventory("pods") else {
        return vec![];
    };
    let Some(pods) = pods["items"].as_array() else {
        return vec![];
    };
    let objects = inventory("services,deployments,replicasets").unwrap_or_default();
    let objects = objects["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut args = root.to_vec();
    args.extend(["top", "pod", "-A", "--containers", "--no-headers"]);
    let top = output(cli, &args).unwrap_or_default();
    kubernetes_workloads(pods, objects, &top)
}

fn kube_owner(object: &Value) -> Option<&str> {
    object["metadata"]["ownerReferences"]
        .as_array()?
        .iter()
        .find(|owner| owner["controller"].as_bool() == Some(true))?["uid"]
        .as_str()
}

fn kube_metadata(object: &Value) -> Option<(&str, &str, &str)> {
    let metadata = &object["metadata"];
    Some((
        metadata["uid"].as_str()?,
        metadata["namespace"].as_str()?,
        metadata["name"].as_str()?,
    ))
}

fn kubernetes_workloads(pods: &[Value], objects: &[Value], top: &str) -> Vec<Workload> {
    let usage = top
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let key = (fields.next()?, fields.next()?, fields.next()?);
            Some((key, (cores(fields.next()?), bytes(fields.next()?))))
        })
        .collect::<HashMap<_, _>>();
    let mut rows = Vec::new();
    let mut pod_rows = Vec::new();
    for pod in pods.iter().take(80) {
        let Some((uid, namespace, pod_name)) = kube_metadata(pod) else {
            continue;
        };
        let id = uid.replace('-', "");
        if !valid_id(&id) {
            continue;
        }
        let Some(specs) = pod["spec"]["containers"].as_array() else {
            continue;
        };
        let running = pod["status"]["phase"].as_str() == Some("Running");
        let statuses = pod["status"]["containerStatuses"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut containers = specs
            .iter()
            .filter_map(|spec| {
                let name = spec["name"].as_str()?;
                let (cpu, memory) = usage
                    .get(&(namespace, pod_name, name))
                    .copied()
                    .unwrap_or_default();
                let limits = &spec["resources"]["limits"];
                let running = running
                    && statuses
                        .iter()
                        .find(|status| status["name"].as_str() == Some(name))
                        .is_none_or(|status| status["state"]["running"].is_object());
                Some(Workload {
                    kind: "kube-container",
                    id: vm_id(uid, name),
                    name: format!("{namespace}/{pod_name}/{name}"),
                    running,
                    compose_project: None,
                    compose_service: None,
                    usage: Usage {
                        cpu,
                        memory,
                        cpu_limit: limits["cpu"].as_str().and_then(cores),
                        memory_limit: limits["memory"].as_str().and_then(bytes),
                    },
                })
            })
            .collect::<Vec<_>>();
        let mut pod_usage = summed_usage(
            containers
                .iter()
                .filter(|container| container.running)
                .map(|c| &c.usage),
        );
        if !containers.iter().any(|container| container.running) {
            pod_usage = Usage::default();
        }
        pod_usage.cpu_limit = containers
            .iter()
            .try_fold(0., |sum, c| Some(sum + c.usage.cpu_limit?));
        pod_usage.memory_limit = containers
            .iter()
            .try_fold(0., |sum, c| Some(sum + c.usage.memory_limit?));
        pod_rows.push((
            pod,
            namespace,
            Workload {
                kind: "kubernetes",
                id,
                name: format!("{namespace}/{pod_name}"),
                running,
                compose_project: None,
                compose_service: None,
                usage: pod_usage,
            },
        ));
        let remaining = 80usize.saturating_sub(rows.len());
        containers.truncate(remaining);
        rows.append(&mut containers);
    }

    rows.push(aggregate(
        "kube-cluster",
        vm_id("kubernetes", "cluster"),
        "Kubernetes".into(),
        true,
        pod_rows.iter().map(|(_, _, row)| row),
    ));
    let mut namespaces: HashMap<&str, Vec<&Workload>> = HashMap::new();
    for (_, namespace, row) in &pod_rows {
        namespaces.entry(namespace).or_default().push(row);
    }
    let owners = objects
        .iter()
        .filter_map(|object| {
            (object["kind"].as_str() == Some("ReplicaSet")).then_some(())?;
            Some((object["metadata"]["uid"].as_str()?, kube_owner(object)?))
        })
        .collect::<HashMap<_, _>>();
    let mut resource_count = 0;
    for object in objects {
        let Some((uid, namespace, name)) = kube_metadata(object) else {
            continue;
        };
        let id = uid.replace('-', "");
        if !valid_id(&id) {
            continue;
        }
        let (kind, running) = match object["kind"].as_str() {
            Some("Deployment") => (
                "deployment",
                object["spec"]["replicas"].as_u64().unwrap_or(1) > 0,
            ),
            Some("Service") => ("kube-service", true),
            _ => continue,
        };
        if resource_count == 80 {
            break;
        }
        resource_count += 1;
        namespaces.entry(namespace).or_default();
        let selector = object["spec"]["selector"]
            .as_object()
            .filter(|selector| !selector.is_empty());
        let members = pod_rows
            .iter()
            .filter_map(|(pod, pod_namespace, row)| {
                if namespace != *pod_namespace {
                    return None;
                }
                let matches = if kind == "deployment" {
                    kube_owner(pod).and_then(|owner| owners.get(owner).copied()) == Some(uid)
                } else {
                    selector.is_some_and(|selector| {
                        selector
                            .iter()
                            .all(|(key, value)| pod["metadata"]["labels"].get(key) == Some(value))
                    })
                };
                matches.then_some(row)
            })
            .collect::<Vec<_>>();
        let label = if kind == "deployment" {
            format!("{namespace}/{name} (Deployment)")
        } else {
            format!("{namespace}/{name}")
        };
        let mut summary = aggregate(kind, id, label, running, members.iter().copied());
        // A service without a pod selector may route to external endpoints.
        // A deployment without observed members may have no accessible ReplicaSet inventory.
        if (kind == "kube-service" && selector.is_none())
            || (kind == "deployment" && running && members.is_empty())
        {
            summary.usage = Usage::default();
        }
        rows.push(summary);
    }
    rows.extend(namespaces.into_iter().map(|(name, members)| {
        aggregate(
            "kube-namespace",
            vm_id("namespace", name),
            name.into(),
            true,
            members,
        )
    }));
    rows.extend(pod_rows.into_iter().map(|(_, _, row)| row));
    rows
}

fn vm_id(provider: &str, name: &str) -> String {
    let hash = provider
        .bytes()
        .chain([0])
        .chain(name.bytes())
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}")
}

fn libvirt_machines(cli: &str, previous: &mut HashMap<String, (Instant, u64)>) -> Vec<Workload> {
    let mut seen = HashMap::new();
    let mut rows = Vec::new();
    for uri in ["qemu:///system", "qemu:///session"] {
        let Some(stats) = output(
            cli,
            &[
                "--readonly",
                "--connect",
                uri,
                "domstats",
                "--raw",
                "--state",
                "--cpu-total",
                "--balloon",
                "--vcpu",
            ],
        ) else {
            continue;
        };
        let now = Instant::now();
        for record in stats.split("Domain: ").skip(1).take(20) {
            let mut lines = record.lines();
            let Some(name) = lines
                .next()
                .and_then(|name| name.trim().strip_prefix('\'')?.strip_suffix('\''))
            else {
                continue;
            };
            let values = lines
                .filter_map(|line| line.trim().split_once('='))
                .collect::<HashMap<_, _>>();
            let key = vm_id(uri, name);
            let running = matches!(values.get("state.state"), Some(&"1" | &"2"));
            let counter = values
                .get("cpu.time")
                .and_then(|value| value.parse::<u64>().ok());
            if let Some(counter) = counter {
                seen.insert(key.clone(), (now, counter));
            }
            let cpu = previous.get(&key).and_then(|(at, old)| {
                let elapsed = now.duration_since(*at).as_secs_f64();
                if !running || !(0.5..=60.).contains(&elapsed) {
                    return None;
                }
                Some(counter?.checked_sub(*old)? as f64 / (elapsed * 1e9))
            });
            let number = |field| {
                values
                    .get(field)
                    .and_then(|value| value.parse::<f64>().ok())
            };
            let memory_limit = number("balloon.current").map(|kib| kib * 1024.);
            let memory = number("balloon.available")
                .zip(number("balloon.usable"))
                .map(|(available, usable)| (available - usable).max(0.) * 1024.);
            rows.push(Workload {
                kind: "vm",
                id: key,
                name: format!("{name} (libvirt)"),
                running,
                compose_project: None,
                compose_service: None,
                usage: Usage {
                    cpu,
                    cpu_limit: number("vcpu.current"),
                    memory,
                    memory_limit,
                },
            });
        }
    }
    *previous = seen;
    rows
}

fn virtual_machines(cli: &str) -> Vec<Workload> {
    if std::env::var_os("MULTIPASS_SERVER_ADDRESS").is_some() {
        return vec![];
    }
    let Some(json) = output(cli, &["info", "--format", "json"])
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return vec![];
    };
    let Some(info) = json.get("info").and_then(Value::as_object) else {
        return vec![];
    };
    let numeric = |value: &Value| value.as_f64().or_else(|| value.as_str()?.parse().ok());
    info.iter()
        .take(20)
        .map(|(name, row)| Workload {
            kind: "vm",
            id: vm_id("multipass", name),
            name: format!("{name} (Multipass)"),
            running: row["state"].as_str() == Some("Running"),
            compose_project: None,
            compose_service: None,
            usage: Usage {
                cpu_limit: numeric(&row["cpu_count"]),
                memory: numeric(&row["memory"]["used"])
                    .or_else(|| numeric(&row["memory"]["usage"])),
                memory_limit: numeric(&row["memory"]["total"]),
                ..Usage::default()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kube_services_use_all_matching_labels_within_their_namespace() {
        let pod = |uid, namespace, name, labels| {
            serde_json::json!({
                "metadata": {"uid": uid, "namespace": namespace, "name": name, "labels": labels},
                "spec": {"containers": [{"name": "app"}]},
                "status": {"phase": "Running"}
            })
        };
        let pods = [
            pod(
                "ab01",
                "apps",
                "first",
                serde_json::json!({"app":"api","tier":"stable"}),
            ),
            pod(
                "ab02",
                "apps",
                "second",
                serde_json::json!({"app":"api","tier":"canary"}),
            ),
            pod(
                "ab03",
                "elsewhere",
                "third",
                serde_json::json!({"app":"api","tier":"stable"}),
            ),
        ];
        let service = serde_json::json!({
            "kind":"Service", "metadata":{"uid":"ab04","namespace":"apps","name":"api"},
            "spec":{"selector":{"app":"api","tier":"stable"}}
        });
        let external = serde_json::json!({
            "kind":"Service", "metadata":{"uid":"ab05","namespace":"apps","name":"external"}
        });
        let rows = kubernetes_workloads(
            &pods,
            &[service, external],
            "apps first app 100m 4Mi\napps second app 200m 8Mi\nelsewhere third app 300m 16Mi",
        );
        let get = |name| rows.iter().find(|row| row.name == name).unwrap();
        assert_eq!(get("apps/api").usage.memory, Some(4. * 1024. * 1024.));
        assert_eq!(get("apps/first/app").usage.cpu, Some(0.1));
        assert_eq!(get("Kubernetes").usage.memory, Some(28. * 1024. * 1024.));
        assert_eq!(get("apps/external").usage.cpu, None);
    }

    #[test]
    fn docker_rollups_are_scoped_to_compose_project_and_preserve_missing_usage() {
        let container = |project: &str, service: &str, name: &str, cpu, running| Workload {
            kind: "docker",
            id: vm_id(project, name),
            name: name.into(),
            running,
            compose_project: Some(project.into()),
            compose_service: Some(service.into()),
            usage: Usage {
                cpu,
                memory: Some(10.),
                ..Usage::default()
            },
        };
        let rows = docker_aggregates(vec![
            container("one", "web", "custom", Some(0.25), true),
            container("one", "api", "worker", Some(0.5), true),
            container("two", "web", "other", None, true),
            container("one", "web", "stopped", Some(5.), false),
        ]);
        let get = |name| rows.iter().find(|row| row.name == name).unwrap();
        assert_eq!(get("Docker").usage.cpu, None);
        assert_eq!(get("Docker").usage.memory, Some(30.));
        assert_eq!(get("one").kind, "docker-project");
        assert_eq!(get("one").compose_project.as_deref(), Some("one"));
        assert!(get("one").running);
        assert_eq!(get("one").usage.cpu, Some(0.75));
        assert_eq!(get("one").usage.memory, Some(20.));
        assert_eq!(get("two").kind, "docker-project");
        assert_eq!(get("two").usage.cpu, None);
        assert_eq!(get("two").usage.memory, Some(10.));
        assert_eq!(get("one / web").usage.cpu, Some(0.25));
        assert_eq!(get("one / api").usage.cpu, Some(0.5));
        assert_eq!(get("two / web").usage.cpu, None);

        let project_row = Workload {
            kind: "docker-project",
            id: vm_id("compose-project", "one"),
            name: "one".into(),
            running: true,
            compose_project: Some("one".into()),
            compose_service: None,
            usage: Usage {
                cpu: Some(0.75),
                cpu_limit: None,
                memory: Some(20.),
                memory_limit: None,
            },
        };
        let readings = project_row.readings(8., 512.);
        let by_key = readings
            .iter()
            .map(|m| (m.key.as_str(), m))
            .collect::<HashMap<_, _>>();
        let prefix = format!("workload.docker.{}", vm_id("compose-project", "one"));
        assert_eq!(
            by_key[format!("{prefix}.present").as_str()].scope,
            "Docker Compose project"
        );
        assert_eq!(
            by_key[format!("{prefix}.present").as_str()]
                .compose_project
                .as_deref(),
            Some("one")
        );
        assert_eq!(
            by_key[format!("{prefix}.cpu.used").as_str()].scope,
            "Docker Compose project"
        );
        assert_eq!(
            by_key[format!("{prefix}.cpu.used").as_str()].value,
            Some(0.75)
        );
        assert_eq!(
            by_key[format!("{prefix}.memory.used").as_str()].scope,
            "Docker Compose project"
        );
        assert_eq!(
            by_key[format!("{prefix}.memory.used").as_str()].value,
            Some(20.)
        );
    }

    #[test]
    fn understands_runtime_units_and_rejects_remote_docker_endpoints() {
        assert_eq!(bytes("256MiB"), Some(268435456.));
        assert_eq!(bytes("8Mi"), Some(8388608.));
        assert_eq!(cores("250m"), Some(0.25));
        assert_eq!(cpu_set_count("0-3,7"), Some(5));
        assert!(local_docker_endpoint("unix:///var/run/docker.sock"));
        assert!(!local_docker_endpoint("ssh://other-server"));
    }
    #[test]
    fn ratios_use_independent_denominators() {
        let w = Workload {
            kind: "docker",
            id: "abcd".into(),
            name: "test".into(),
            running: true,
            compose_project: None,
            compose_service: None,
            usage: Usage {
                cpu: Some(0.5),
                cpu_limit: Some(1.),
                memory: Some(64.),
                memory_limit: None,
            },
        };
        let readings = w.readings(8., 512.);
        let values = readings
            .iter()
            .map(|m| (m.key.as_str(), m.value))
            .collect::<HashMap<_, _>>();
        assert_eq!(values["workload.docker.abcd.cpu.allocated"], Some(50.));
        assert_eq!(values["workload.docker.abcd.cpu.host"], Some(6.25));
        assert_eq!(values["workload.docker.abcd.memory.allocated"], None);
        assert_eq!(values["workload.docker.abcd.memory.host"], Some(12.5));
    }
}
