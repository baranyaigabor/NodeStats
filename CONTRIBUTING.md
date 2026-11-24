# Contributing to NodeStats

Thanks for helping improve NodeStats. Keep pull requests focused, small, and easy to verify.

## Local Development

1. Install the latest stable Rust toolchain via [rustup](https://rustup.rs/).
2. Run formatting, lints, and the unit test suite before submitting changes:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

3. Test a local snapshot safely without connecting to any server:

```sh
cargo run -- snapshot
```

## Platform Notes

- Avoid shell injection when calling OS tools (`docker`, `kubectl`, `systemctl`, `launchctl`, `sc`): pass arguments individually via `std::process::Command::arg` / `.args` rather than invoking a shell.
- Redact secrets, tokens, passwords, and authentication query strings from any reported command lines or diagnostics.
- Gate platform-specific hardware counters cleanly behind `#[cfg(target_os = "...")]` so cross-platform CI stays green on Linux, macOS, and Windows.
