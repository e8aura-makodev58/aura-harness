# aura-harness

Run several coding agents on one repository, unattended. Each agent works in its own git
worktree and branch. A daemon plans, schedules, reviews and merges. You give direction
through the `aura` command or the live dashboard, and closing either one leaves the work
running.

- **Design:** [docs/autonomous-harness-design.md](docs/autonomous-harness-design.md)
- **Build, run, verification, limits:** [docs/implementation.md](docs/implementation.md)
- **Manual process this replaces:** [aura-harness.md](aura-harness.md)

## Layout

| Path | What it is |
|---|---|
| `crates/aura-core` | Domain model, state machines, scope rules, quota, config |
| `crates/aura-store` | SQLite state and event log |
| `crates/aura-git` | Worktrees, branches, merges, compare-and-swap ref updates |
| `crates/aura-adapters` | Provider CLIs and usage-limit detection |
| `crates/aurad` | The daemon (scheduler, integrator, local API) |
| `crates/aura-cli` | The `aura` command, `aura web` and `aura watch` |
| `crates/aurad/web` | The web interface (Dockview, vendored) |
| `contrib/` | systemd unit and example configuration |
| `docs/` | Design and implementation guide |

## Install and run

See [INSTALL.md](INSTALL.md) for prerequisites, installation, configuration, and troubleshooting.

## Quick start

```bash
cargo build --release
cp contrib/aura.example.toml ~/.config/aura/aura.toml   # set repo.path
target/release/aurad --config ~/.config/aura/aura.toml &
target/release/aura directive "Describe the change you want"
target/release/aura watch          # terminal dashboard
target/release/aura web            # link to the web interface (127.0.0.1:7878)
```

## Test

```bash
cargo test --workspace
```

The tests use real git repositories and the real engine. The agent CLIs are replaced by a
script that writes the records a real agent would write. What has and has not been verified
against the real CLIs is listed in [docs/implementation.md](docs/implementation.md).

## Status

Built, except the Telegram bridge, which is deferred until the bot is created. The quota
message formats are assumed until the phase 0.5 spike records real output.
