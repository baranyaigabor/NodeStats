# Changelog

All notable changes to the NodeStats Rust collector CLI are documented in this file.

## [0.2.5] - 2026-11-27

### Added
- Package metadata and artifacts for Homebrew (`homebrew-core` and tap formulas), Debian/Ubuntu `.deb` (`cargo-deb`), Fedora/RHEL `.rpm` (`cargo-generate-rpm`), and POSIX `scripts/install.sh`.
- Systemd template unit (`packaging/systemd/nodestats@.service`) and APT/RPM repository configuration files.
- Rich process telemetry fields for Task Manager (command lines with redacted flags/tokens, user attribution, disk throughput, executable metadata, and parent/child relationships).
- Sub-second per-node refresh cadence propagation down to `10ms` (`0.01s–60s`) across energy integration and subsystem collectors.

### Changed
- `nodestats setup` preserves package-manager-managed binary paths (`/opt/homebrew/bin/nodestats`, `/usr/bin/nodestats`, `/usr/local/bin/nodestats`) when registering `launchd`, `systemd`, or `OpenRC`, preventing upgrade drift.

## [0.2.4] - 2026-11-23

### Added
- Signed self-update verification using Ed25519 manifest signatures and SHA-256 verification.
- Cloudflare Access service token support (`cloudflare-access.json`) for Zero Trust protected servers.
- Task Manager process inspection and confirmed force-quit action support behind `--allow-task-management`.

## [0.2.0] - 2026-11-08

### Added
- Docker Compose, single-node Kubernetes, Multipass, and QEMU/libvirt workload discovery and telemetry.
- Remote workload controls (`Start`, `Stop`, `Restart`, `Get logs`) gated behind `--allow-workload-management`.
- Native background service installers (`launchd`, `systemd`, `OpenRC`, and Windows Task Scheduler).

## [0.1.0] - 2026-10-27

### Added
- Initial cross-platform Rust collector for macOS, Linux, Windows, Docker, and Kubernetes.
- One-time pairing flow, gzip-adaptive metric streaming, and local sample snapshot mode.
