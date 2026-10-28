# Installing and running Aura

Aura runs coding agents on one git repository, unattended. A daemon (`aurad`)
schedules the work. The `aura` command, the terminal dashboard, and the web page
are clients of it. This guide covers installing, configuring, starting, and
stopping Aura. For how it works, see [docs/implementation.md](docs/implementation.md).

## 1. Prerequisites

| Needed | Why | Check |
|---|---|---|
| Rust 1.82 or newer | Builds Aura. Install with [rustup](https://rustup.rs). | `cargo --version` |
| `git` | Worktrees, branches, merges | `git --version` |
| A C compiler | SQLite is bundled and compiled | `cc --version` |
| `claude` (Claude Code) | Default planner and worker | `claude --version` |
| `codex` (Codex) | Default reviewer | `codex --version` |
| A git repository with a `main` branch | The work target | `git -C <repo> branch --list main` |

Claude Code and Codex must be installed and signed in with your subscription.
Without Codex, reviews fail and tasks block. Either install it or change the
`[roles]` section of the config (see step 3).

## 2. Build and install

From a clone of this repository:

```bash
cargo build --release

install -Dm755 target/release/aurad ~/.local/bin/aurad
install -Dm755 target/release/aura  ~/.local/bin/aura
```

Make sure `~/.local/bin` is on your `PATH`:

```bash
aura --version
```

## 3. Configure

```bash
mkdir -p ~/.config/aura
cp contrib/aura.example.toml ~/.config/aura/aura.toml
```

Edit `~/.config/aura/aura.toml`:

- **`[repo] path`**: the absolute path of the repository the agents will work on.
  This is required. There is no default. The example's `/absolute/path/to/your/repo`
  placeholder must be replaced. See "Choosing the repository" below.
- **`[repo] main_branch`**: the branch to merge into. Defaults to `main`.
- **`[acceptance] default_checks`**: the commands every merge must pass. The
  example runs `cargo test --workspace --quiet`. Change it to suit your project.
- **`[defaults] autonomy`**: `ask-plan` (the default in the example) means you
  approve each plan before work starts. Use `auto` only once you trust the checks.
- **`[roles]`**: which provider plans, works, and reviews. Reviewers should differ
  from workers.
- **`[web]`**: the web page address. Defaults to `127.0.0.1:7878`. It must be a
  loopback address.

Optional: `state_dir` sets where Aura keeps its database, worktrees, and logs.
It defaults to `~/.local/share/aura`. Keep the path short, because a Unix socket
path must be under 108 bytes.

### Choosing the repository

Approved work is merged into the `main_branch` of the repository in `[repo] path`,
without you looking at it. Pick the target deliberately:

- **A separate repository (recommended for the first runs).** Its checks are the
  only safety net, so use one whose `default_checks` you trust. Set `path`, `main_branch`,
  and `default_checks` for that project.
- **The Aura checkout itself.** Agents then edit the code that runs the daemon. Changes
  reach the running daemon only after you rebuild and restart it, which makes review
  harder. If you choose this, keep `autonomy = "ask-plan"` and read every plan before
  approving it. Use the checkout path in `path`, `main` in `main_branch`, and
  `cargo test --workspace --quiet` in `default_checks`.

Either way, the directory must be a git repository. Aura does not land a merge into a
checkout that has uncommitted changes. It holds the merge and retries, so keep that
checkout clean while Aura runs.

## 4. Start the daemon

### Option A: foreground (simplest, good for a first try)

```bash
aurad
```

Leave it running. It logs to the terminal and stops with Ctrl-C. Agents that were
running at that moment keep running until the next daemon start, which stops them and
returns their tasks to the queue. Use `aura kill --yes` first if you want them stopped
now.

### Option B: background service (survives logouts and reboots)

```bash
install -Dm644 contrib/aurad.service ~/.config/systemd/user/aurad.service
systemctl --user daemon-reload
systemctl --user enable --now aurad
```

Watch its logs:

```bash
journalctl --user -u aurad -f
```

The unit file expects `aurad` in `~/.local/bin` and the config in
`~/.config/aura/aura.toml`. Stopping the service also stops every agent it
started.

## 5. First run

```bash
aura status                                    # overview: nothing yet, quota per provider
aura directive "Describe the change you want"  # send a goal to the planner
aura directives                                # wait for "awaiting-approval" (ask-plan)
aura tasks                                     # the proposed tasks, once the planner finishes
aura approve D-0001                            # approve the plan (use your directive id)
aura watch                                     # live terminal dashboard; q quits
```

Replace `D-0001` with the id `aura directives` shows. The daemon keeps running
when you close `aura watch`.

Useful follow-ups:

```bash
aura task show T-0001      # brief, notes, runs, questions
aura answer Q-0001 "text"  # answer a question a worker asked
aura events --follow       # live event log
aura logs R-0007           # the end of one run's log
aura quota                 # when each provider can run again
```

## 6. Web interface

```bash
aura web
```

This prints a link with an access token, for example
`http://127.0.0.1:7878/#token=…`. Open it in a browser. The page has panels for
tasks, task detail, directives, a live log, quota, and events. It can dock, split,
and resize them.

The token is stored in `~/.local/share/aura/web.token` (mode 0600). Anyone who has
the token can control Aura, so treat the link as a password. Do not share it, and
keep the address on loopback.

## 7. Stop, pause, resume

| To | Run |
|---|---|
| Stop new work, let running agents finish | `aura pause` |
| Start new work again | `aura resume` |
| Stop every agent now and pause | `aura kill --yes` |
| Stop the daemon (service) | `systemctl --user stop aurad` |
| Stop the daemon (foreground) | Ctrl-C |

Stopping the daemon never loses queued work. The database is the source of truth.

## 8. Update

```bash
git pull
cargo build --release
install -Dm755 target/release/aurad ~/.local/bin/aurad
install -Dm755 target/release/aura  ~/.local/bin/aura
systemctl --user restart aurad      # if you use the service
```

Installing new binaries does not change a running daemon. Restart the service
(or start `aurad` again) to load them.

## 9. Uninstall

```bash
systemctl --user disable --now aurad 2>/dev/null
rm -f ~/.config/systemd/user/aurad.service
rm -f ~/.local/bin/aurad ~/.local/bin/aura
rm -rf ~/.config/aura
rm -rf ~/.local/share/aura    # deletes the database, worktrees, logs, and the access token
```

Task branches (`aura/T-…`) and the `harness` branch stay in your repository.
Delete them with `git branch -D` if you no longer want them.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `cannot reach aurad … is the daemon running?` | Start the daemon (step 4). |
| `another aurad is already running` | A daemon is already using this state directory. Stop it, or check `systemctl --user status aurad`. |
| `socket path … is too long` | Set `state_dir` to a shorter path. |
| `repository path … does not exist` | Fix `[repo] path`. |
| `branch main does not exist` | Set `[repo] main_branch` to your branch name. |
| Tasks end up `blocked` with "cannot start reviewer" | Codex is missing. Install it, or change `[roles] reviewer`. |
| Workers fail on shell commands such as `git` or `cargo` | The headless permission mode may block them. See the note in [docs/implementation.md](docs/implementation.md) and add an allow list to the `claude-code` provider. |
| The web page says "not authorised" | Run `aura web` again and open the new link. The token is stored in the browser after the first visit. |
| The web page does not load | Check the daemon printed `web UI at …`. Check that nothing else uses port 7878, or change `[web] listen`. |

## What is verified

- The build, the `aura` CLI, the daemon start, `aura status`, and `aura web` were
  run end to end on a fresh machine with a fresh repository.
- The web page was tested in headless Chromium against a live daemon.
- The flags used for Claude Code exist in the installed version (2.1.311). A real
  worker run has not been performed, because it uses a subscription.

Not verified: the systemd unit, Codex (not installed when this was written), and
headless shell permissions for workers.
