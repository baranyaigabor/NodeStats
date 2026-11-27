<div align="center">
  <img src="assets/logo.svg" alt="NodeStats" width="72" height="72" />
  <h1>NodeStats</h1>
  <p>
    <strong>Lightweight, local-first system telemetry agent written in Rust.</strong>
  </p>
  <p>
    Streams CPU, GPU, memory, power, thermal, disk, network, container, and process telemetry from macOS, Linux, Windows, Docker, and Kubernetes to your paired NodeStats workspace.
  </p>

  <p>
    <a href="https://github.com/baranyaigabor/NodeStats/actions/workflows/ci.yml"><img src="https://github.com/baranyaigabor/NodeStats/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/baranyaigabor/NodeStats/releases/latest"><img src="https://img.shields.io/github/v/release/baranyaigabor/NodeStats?color=ff783d" alt="Latest Release" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License" /></a>
    <img src="https://img.shields.io/badge/rust-2024-orange.svg" alt="Rust 2024" />
    <img src="https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-2b2d34" alt="Platforms" />
  </p>
</div>

---

## Quick Install

### macOS (Homebrew)

```sh
brew install nodestats
```

*(If installing directly from the tap before core acceptance: `brew install baranyaigabor/tap/nodestats`.)*

### Debian / Ubuntu (APT)

```sh
curl -fsSL https://baranyaigabor.github.io/NodeStats/apt/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/nodestats.gpg
echo "deb [signed-by=/usr/share/keyrings/nodestats.gpg] https://baranyaigabor.github.io/NodeStats/apt stable main" | sudo tee /etc/apt/sources.list.d/nodestats.list
sudo apt update && sudo apt install -y nodestats
```

### Fedora / RHEL / Rocky Linux (DNF)

```sh
sudo curl -fsSL https://raw.githubusercontent.com/baranyaigabor/NodeStats/main/packaging/rpm/nodestats.repo -o /etc/yum.repos.d/nodestats.repo
sudo dnf install -y nodestats
```

### Universal Shell Installer (macOS & Linux)

Auto-detects your OS and CPU architecture (`x86_64` / `arm64`), verifies SHA-256 checksums against `SHA256SUMS.txt`, and installs `nodestats` into `/usr/local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/baranyaigabor/NodeStats/main/scripts/install.sh | sh
```

### From Source (Cargo)

```sh
cargo install --locked --git https://github.com/baranyaigabor/NodeStats
```

---

## Pair & Start in 30 Seconds

Run setup as your regular user (never as root—the installer prompts for `sudo` only when registering the system service):

```sh
nodestats setup --server https://nodestats.hu
```

1. Copy the printed one-time pairing code.
2. Open your NodeStats dashboard and click **Connect node**.
3. Approve startup when prompted. You can close the terminal afterward.

When `nodestats` was installed via Homebrew (`/opt/homebrew/bin/nodestats`) or `apt`/`dnf` (`/usr/bin/nodestats`), `setup` wires the startup service directly to the package-managed binary so `brew upgrade`, `apt upgrade`, and `dnf upgrade` keep the background agent up to date automatically.

---

## CLI Commands

| Command | Purpose |
| --- | --- |
| `nodestats setup` | Pair this host and register the background boot service (`launchd`, `systemd`, `OpenRC`, or Windows Task Scheduler) |
| `nodestats status` | Check the background agent status |
| `nodestats start` | Start the configured background service |
| `nodestats stop` | Pause the background service until started again or rebooted |
| `nodestats run` | Stream live metrics in the foreground (supports `--server`, `--name`, `--workload`, `--allow-task-management`, `--allow-workload-management`) |
| `nodestats snapshot` | Collect and print a single JSON telemetry snapshot locally without pairing or uploading |

---

## Platform & Sensor Coverage

| Subsystem | macOS | Linux | Windows | Docker / Kubernetes |
| --- | --- | --- | --- | --- |
| **CPU** | Total & per-core utilization, frequencies | Total & per-core utilization, clock frequencies | Total & per-core utilization, clock speeds | cgroup v1/v2 CPU usage & quota |
| **GPU** | IOAccelerator device/renderer/tiler, GPU RAM | DRM GPU counters & `nvidia-smi` | Driver GPU engine usage & `nvidia-smi` | Passed-through devices when exposed |
| **Memory** | Active, wired, swap, network buffer | RAM, swap, TCP buffer memory | Physical memory & commit charge | Container memory & limit |
| **Power & Energy** | Battery, system draw, integrated Wh, optional `powermetrics` | `hwmon`, `powercap` RAPL energy domains, battery, integrated Wh | ACPI power meters, battery discharge, board watts | Container energy counters when accessible |
| **Thermals** | CPU/GPU die & fan sensors | Kernel `hwmon` temperatures & fan RPMs | ACPI thermal zones & GPU temps | Host sensors if readable |
| **Storage & Network** | Volume usage, disk throughput, per-interface Rx/Tx, TCP ping | Volume usage, block I/O, per-interface Rx/Tx, TCP ping | Drive usage, I/O, per-interface Rx/Tx, TCP ping | Container storage & interface throughput |
| **Workloads** | Docker Compose/containers, local K8s, Multipass, QEMU | Docker Compose/containers, local K8s, Multipass, QEMU/libvirt | Docker Desktop, local K8s | Reports its own workload scope |

- **Cadence**: Default remote reporting cadence is configurable per node from the dashboard between **0.01s and 60s**. Payloads automatically gzip-compress when beneficial and fall back to plain JSON if a proxy rejects `Content-Encoding: gzip`.
- **Task Manager**: Pass `--allow-task-management` when running interactively to share the top 200 processes (CPU, memory, disk I/O, executable, owner) and permit confirmed force-quit requests from the dashboard.
- **Remote Workload Controls**: Pass `--allow-workload-management` to enable confirmed `Start`, `Stop`, `Restart`, and recent `Get logs` inspection for local Docker containers and single-node Kubernetes Deployments.

---

## Docker & Kubernetes

### Docker Compose

```yaml
services:
  nodestats-client:
    build: https://github.com/baranyaigabor/NodeStats.git
    image: nodestats-client:local
    restart: unless-stopped
    environment:
      NODESTATS_SERVER: https://nodestats.hu
      NODESTATS_NAME: Docker client
      NODESTATS_WORKLOAD: ${COMPOSE_PROJECT_NAME}/nodestats-client
    volumes:
      - nodestats-identity:/state

volumes:
  nodestats-identity: {}
```

```sh
docker compose up -d --build nodestats-client
docker compose logs -f nodestats-client
```

### Kubernetes

```sh
docker build -t nodestats-client:local https://github.com/baranyaigabor/NodeStats.git
kubectl apply -f kubernetes.yaml
kubectl logs -f deployment/nodestats-client
```

The persistent volume claim preserves pairing credentials across pod restarts without requiring Kubernetes API cluster permissions.

---

## Security & Credentials

- **Outbound-only connections**: No listening ports or inbound firewall holes on monitored servers.
- **No shared secret paste**: Every machine negotiates its own random token during one-time browser approval. Credentials are locked exclusively by the running process in `identity.json` under the OS user's data directory (or `NODESTATS_STATE_DIR`).
- **Signed updates**: Managed binaries verify an Ed25519 signature and SHA-256 hash before applying updates, falling back cleanly if an update server is unreachable.
- **Transport**: Remote servers require HTTPS (plain HTTP is rejected except for loopback and Docker host gateways).
- **Cloudflare Access**: Place a user-readable `cloudflare-access.json` file alongside `identity.json` for Zero Trust Service Auth origins:

```json
{
  "server": "https://nodestats.example.com",
  "clientId": "ACCESS_SERVICE_CLIENT_ID",
  "clientSecret": "ACCESS_SERVICE_CLIENT_SECRET"
}
```

See [SECURITY.md](SECURITY.md) to report vulnerabilities.

---

## Building & Testing from Source

Requires the current stable Rust toolchain (`edition = "2024"`).

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

Maintainer instructions for tagging releases, signing APT/RPM repositories, and updating Homebrew formulas live in [docs/DISTRIBUTION.md](docs/DISTRIBUTION.md).

## License

[MIT](LICENSE) © Gabor Baranyai
