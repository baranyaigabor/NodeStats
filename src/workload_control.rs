use crate::{
    platform::probe_runtime,
    workloads::{
        KUBE_DEPLOYMENT_INVENTORY, KUBE_POD_IDS, docker_is_local, local_kube_server, output, tool,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Start,
    Stop,
    Restart,
    Logs,
}

#[derive(Deserialize)]
pub struct Command {
    id: String,
    target: String,
    operation: Operation,
}

#[derive(Serialize)]
pub struct Result {
    id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs: Option<String>,
}

impl Command {
    pub fn unavailable(self) -> Result {
        Result {
            id: self.id,
            status: "unavailable",
            logs: None,
        }
    }

    pub fn execute(self, metrics: &[Value]) -> Result {
        let present = format!("{}.present", self.target);
        if !metrics
            .iter()
            .any(|metric| metric["key"].as_str() == Some(&present))
        {
            return self.unavailable();
        }
        let Some(target) = self.target.strip_prefix("workload.") else {
            return self.unavailable();
        };
        let Some((runtime, id)) = target.split_once('.') else {
            return self.unavailable();
        };
        if !(12..=64).contains(&id.len())
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return self.unavailable();
        }
        let outcome = match runtime {
            "docker" => docker(id, self.operation),
            "kubernetes" => kubernetes(id, self.operation),
            _ => return self.unavailable(),
        };
        match outcome {
            Some(output) => {
                let logs = (self.operation == Operation::Logs).then(|| log_excerpt(&output));
                Result {
                    id: self.id,
                    status: "done",
                    logs,
                }
            }
            None => Result {
                id: self.id,
                status: "failed",
                logs: None,
            },
        }
    }
}

fn log_excerpt(output: &[u8]) -> String {
    let clean: String = String::from_utf8_lossy(output)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let lines: Vec<_> = clean.lines().rev().take(100).collect();
    let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let mut first = tail.len().saturating_sub(6000);
    while !tail.is_char_boundary(first) {
        first += 1;
    }
    tail[first..].to_owned()
}

fn docker(id: &str, operation: Operation) -> Option<Vec<u8>> {
    let cli = tool("docker")?;
    if !docker_is_local(&cli) {
        return None;
    }
    let full = output(
        &cli,
        &["inspect", "--type", "container", "--format", "{{.Id}}", id],
    )?;
    let full = full.trim();
    if full.len() != 64 || !full.starts_with(id) || !full.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let options: &[&str] = match operation {
        Operation::Start => &["start", full],
        Operation::Stop => &["stop", "--time", "5", full],
        Operation::Restart => &["restart", "--time", "5", full],
        Operation::Logs => &["logs", "--tail", "100", "--timestamps", full],
    };
    probe_runtime(
        &cli,
        options,
        Duration::from_secs(14),
        operation == Operation::Logs,
    )
}

fn kubernetes(id: &str, operation: Operation) -> Option<Vec<u8>> {
    let cli = tool("kubectl")?;
    let server = local_kube_server(&cli)?;
    let base = ["--server", &server, "--request-timeout=5s"];
    let locate = |plural, selector| {
        let mut args = base.to_vec();
        args.extend(["get", plural, "-A", "-o", selector]);
        let lines = output(&cli, &args)?;
        lines.lines().find_map(|line| {
            let mut parts = line.split('\t');
            let uuid = parts.next()?;
            if uuid.replace('-', "") != id {
                return None;
            }
            let namespace = parts.next()?;
            let name = parts.next()?;
            let valid = |value: &str| {
                !value.is_empty()
                    && value.as_bytes()[0].is_ascii_alphanumeric()
                    && value.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                    })
            };
            (valid(namespace) && valid(name))
                .then(|| (uuid.to_string(), namespace.to_string(), name.to_string()))
        })
    };
    let deployment = locate("deployments", KUBE_DEPLOYMENT_INVENTORY);
    let (kind, (uid, namespace, name)) = match deployment {
        Some(deployment) => ("deployment", deployment),
        None if operation == Operation::Logs => ("pod", locate("pods", KUBE_POD_IDS)?),
        None => return None,
    };
    let resource = format!("{kind}/{name}");
    let mut args = base.to_vec();
    args.extend(["--namespace", &namespace]);
    if operation == Operation::Logs {
        args.extend([
            "logs",
            &resource,
            "--tail=100",
            "--timestamps",
            "--all-containers=true",
            "--prefix",
        ]);
        return probe_runtime(&cli, &args, Duration::from_secs(10), false);
    }
    let mut get = args.clone();
    get.extend(["get", &resource, "-o", "json"]);
    let current: Value = serde_json::from_str(&output(&cli, &get)?).ok()?;
    if current["metadata"]["uid"].as_str() != Some(&uid) {
        return None;
    }
    let patch = deployment_patch(&current, operation)?;
    let Some(patch) = patch else {
        return Some(Vec::new());
    };
    args.extend([
        "patch",
        &resource,
        "--type=json",
        "--patch",
        &patch,
        "-o",
        "name",
    ]);
    probe_runtime(&cli, &args, Duration::from_secs(10), false)
}

fn deployment_patch(current: &Value, operation: Operation) -> Option<Option<String>> {
    let uid = current["metadata"]["uid"].as_str()?;
    let version = current["metadata"]["resourceVersion"].as_str()?;
    let mut patch = vec![
        json!({"op":"test", "path":"/metadata/uid", "value":uid}),
        json!({"op":"test", "path":"/metadata/resourceVersion", "value":version}),
    ];
    let replicas = current["spec"]["replicas"].as_u64()?;
    const SAVED: &str = "nodestats.io/previous-replicas";
    const RESTARTED: &str = "nodestats.io/restarted-at";
    match operation {
        Operation::Start if replicas > 0 => return Some(None),
        Operation::Stop if replicas == 0 => return Some(None),
        Operation::Restart if replicas == 0 => return None,
        Operation::Start => {
            let target = current["metadata"]["annotations"][SAVED]
                .as_str()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|replicas| *replicas > 0)
                .unwrap_or(1);
            patch.push(json!({"op":"replace", "path":"/spec/replicas", "value":target}));
            if current["metadata"]["annotations"][SAVED].is_string() {
                patch.push(json!({"op":"remove", "path":"/metadata/annotations/nodestats.io~1previous-replicas"}));
            }
        }
        Operation::Stop => {
            if current["metadata"]["annotations"].is_null() {
                patch.push(json!({"op":"add", "path":"/metadata/annotations", "value":{}}));
            }
            patch.push(json!({"op":"add", "path":"/metadata/annotations/nodestats.io~1previous-replicas", "value":replicas.to_string()}));
            patch.push(json!({"op":"replace", "path":"/spec/replicas", "value":0}));
        }
        Operation::Restart => {
            let now = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .ok()?;
            if current["spec"]["template"]["metadata"]["annotations"].is_null() {
                patch.push(json!({"op":"add", "path":"/spec/template/metadata/annotations", "value":{RESTARTED:now}}));
            } else {
                patch.push(json!({"op":"add", "path":"/spec/template/metadata/annotations/nodestats.io~1restarted-at", "value":now}));
            }
        }
        Operation::Logs => return None,
    }
    Some(serde_json::to_string(&patch).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_patch_guards_identity_and_preserves_replica_count() {
        let mut deployment = json!({
            "metadata": { "uid":"old", "resourceVersion":"41" },
            "spec": { "replicas":3, "template": { "metadata": {} } }
        });
        let stop: Value = serde_json::from_str(
            &deployment_patch(&deployment, Operation::Stop)
                .flatten()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stop[0]["value"], "old");
        assert_eq!(stop[1]["value"], "41");
        assert_eq!(stop[3]["value"], "3");
        assert_eq!(stop[4]["value"], 0);
        deployment["metadata"]["annotations"] = json!({"nodestats.io/previous-replicas":"3"});
        deployment["spec"]["replicas"] = json!(0);
        let start: Value = serde_json::from_str(
            &deployment_patch(&deployment, Operation::Start)
                .flatten()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(start[2]["value"], 3);
        assert!(deployment_patch(&deployment, Operation::Restart).is_none());
        deployment["spec"]["replicas"] = json!(3);
        let restart: Value = serde_json::from_str(
            &deployment_patch(&deployment, Operation::Restart)
                .flatten()
                .unwrap(),
        )
        .unwrap();
        assert!(restart[2]["value"]["nodestats.io/restarted-at"].is_string());
    }

    #[test]
    fn trims_logs_to_recent_text() {
        assert_eq!(log_excerpt(b"hello\x00\nthere\r\n"), "hello\nthere");
        assert!(log_excerpt("☃".repeat(4000).as_bytes()).len() <= 6000);
    }
}
