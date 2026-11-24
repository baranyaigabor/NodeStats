# Security Policy

## Supported Versions

Only the latest tagged `0.2.x` release receives security patches and signed binary updates.

## Security Design Highlights

- The NodeStats agent opens outbound connections only; no inbound socket or listener is exposed.
- Cleartext `http://` is permitted only for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`) and `host.docker.internal`; remote hosts require `https://`.
- Process commands collected for Task Manager redact common secret flags, bearer headers, and sensitive query parameters before leaving the host.
- Privileged controls (`--allow-task-management`, `--allow-workload-management`) are opt-in and disabled by default.
- Managed updates verify an Ed25519 signature over the JSON feed and a SHA-256 digest over the downloaded binary before replacing files.

## Reporting a Vulnerability

Please report security issues privately rather than opening a public GitHub issue:

- Email: `baranyaigl06@gmail.com`

Include reproduction steps, affected platform/architecture, and any relevant configuration details.
