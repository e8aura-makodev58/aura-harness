# Implementation guide

This describes what is built, how to run it, what was verified, and what is still
assumed. The design it implements is [autonomous-harness-design.md](autonomous-harness-design.md).

## What is built

| Crate | Role |
|---|---|
| `aura-core` | Domain types, task and directive state machines, scope globs, quota logic, record formats, briefs, configuration. Pure logic, no I/O beyond reading the config file. |
| `aura-store` | SQLite persistence (WAL, schema version 1). Directives, tasks, runs, questions, quota, and an append-only event log. |
| `aura-git` | Git operations through the `git` CLI: worktrees, branches, merges with conflict detection, compare-and-swap ref updates, exclude entries. |
| `aura-adapters` | One adapter per provider CLI: argv construction and usage-limit detection with reset-time parsing. |
| `aurad` | The daemon: scheduler, process supervision, merge integrator, `harness` mirror, Unix-socket API, and the local web interface. |
| `aura-cli` | The `aura` command, `aura web` (prints the page link) and the `aura watch` terminal dashboard. All are clients of the daemon API. |
| `aurad/web` | The web interface: a page built with [Dockview](https://github.com/dockview/dockview) (MIT, vendored, no build step). Panels dock, split, resize and float; the layout is remembered per browser. Compiled into the daemon. |

Providers: **Claude Code** and **Codex** are configured. **Antigravity** and **Hermes**
ship disabled, because no headless mode has been confirmed for them. Enabling one requires
its `command` and `args`.

**Telegram** is not implemented. You will create the bot yourself, and we will design the
bridge when we reach that phase. The API it will use already exists.

## Quick start

Requirements: Rust 1.82 or newer, `git`, a C compiler (SQLite is bundled), and the provider
CLIs you want to use.

```bash
cargo build --release
install -Dm755 target/release/aurad ~/.local/bin/aurad
install -Dm755 target/release/aura  ~/.local/bin/aura

mkdir -p ~/.config/aura
cp contrib/aura.example.toml ~/.config/aura/aura.toml   # then set repo.path
aurad --config ~/.config/aura/aura.toml                 # or install the systemd unit below
```

In another terminal:

```bash
aura status
aura directive "Make uploads retry on network errors; keep the public API unchanged"
aura directives           # planning -> awaiting-approval (ask-plan) -> active -> done
aura approve D-0001       # only under ask-plan / ask-each
aura tasks
aura task show T-0001     # brief, notes, runs, questions
aura answer Q-0001 "exponential backoff capped at 30 s"
aura events --follow
aura watch                # terminal dashboard; q quits, the daemon keeps running
aura web                  # prints the web interface link (http://127.0.0.1:7878/#token=…)
```

Stop everything: `aura kill --yes` (stops runs and pauses), then `aura resume` to continue.

### systemd

```bash
install -Dm644 contrib/aurad.service ~/.config/systemd/user/aurad.service
systemctl --user daemon-reload
systemctl --user enable --now aurad
journalctl --user -u aurad -f
```

`KillMode=control-group` means stopping the service also stops every agent it started.
The daemon refuses to start a second instance on the same state directory (an `flock` on
`aurad.lock`).

## Lifecycle, as implemented

```
directive ─planner─▶ tasks
  ask-plan / ask-each: awaiting-approval ──approve──▶ active
  auto:                active

task:  queued ─worker─▶ running ─ready─▶ review ─reviewer approves─▶ merge-queued ─integrator─▶ done
                  │          │                  │                          │
                  │          └─failed/rejected─▶ queued (retry, budget)    └─check/conflict─▶ queued
                  │          └─budget spent────▶ blocked
                  │          └─question────────▶ blocked ──answer──▶ queued
                  └─provider limited──────────▶ queued (run not counted)
  ask-each adds: proposed ──approve──▶ queued, and review-approved ──approve──▶ merge-queued
```

- **Level-triggered scheduler.** Every tick (500 ms) reads the database and starts whatever
  is due: a planner for each planning directive, a reviewer for each task in review, a worker
  for each queued task whose dependencies are done and whose write scope does not overlap a
  running worker, and the next merge. Nothing is remembered in memory, so a restart loses
  nothing.
- **Reconciliation on start.** Runs recorded as running are killed (their process group) and
  recorded as interrupted. Their tasks return to the queue without using up a run.
- **Scope enforcement.** A worker's branch must change only files matching its write scope.
  The daemon checks the actual diff against the main branch, not the agent's claim. Violations
  fail the run.
- **Acceptance commands** run on the merge result, without a shell, and only programs on
  `acceptance.allowed_programs` may start. A plan that names any other program is rejected
  whole, and no tasks are created from it.
- **Merge integrator.** One merge at a time, on its own thread: merge the branch into a
  detached worktree of the main branch, run the checks, and move the main branch only if it
  has not moved since the merge began (compare-and-swap, or fast-forward when the main branch
  is checked out). On conflict the merge is aborted and the task goes back to a worker with
  the list of conflicting files.
- **Mirror.** The `harness` branch receives `tasks/*.md`, `directives/*.md` and an append-only
  `events.jsonl`, committed at most every five seconds. It is for reading and history; nothing
  is scheduled from it.

## Quota, and what is still assumed

Subscriptions refuse runs until a window refreshes. The daemon handles a refusal like this:

1. The adapter scans the failed run's output for a limit phrase (default patterns in
   `aura-adapters/src/limit.rs`, plus `limit_patterns` from the config).
2. A reset time is read from it, if one is printed: a Unix epoch after `|`, an ISO timestamp,
   "try again in N minutes", or "resets at 6:30 PM" in server local time.
3. If none is printed, the plan's `quota_window` (for example `5h`) gives the reset time.
4. If there is no window either, exponential back-off runs from 5 to 60 minutes.

The provider is then skipped until that time, and the run does not count against its task's
budget. `aura quota` and the dashboard show the state and the time it is expected to refresh.

**These message formats are assumptions.** `aura-adapters/tests/fixtures/` holds stand-in
samples, labelled as such in its README. The phase 0.5 spike replaces them with real output:

1. Run each CLI with no quota left, or capture a refusal from an earlier session.
2. Save the full output (stdout and stderr) as a fixture with the same file name.
3. Run `cargo test -p aura-adapters`. Adjust the patterns only where the real text differs.

Check the adapter argument lists in the same pass. `claude -p ... --output-format json
--permission-mode acceptEdits` and `codex exec --sandbox workspace-write` are the best known
headless forms and are configurable. Neither has been run against real installations here.

## Verification

Run it yourself with `cargo test --workspace`. Results at the time of writing:

| Suite | Tests | What it covers |
|---|---|---|
| `aura-core` | 29 | State machines, scope globs and overlap, quota transitions and back-off, records, briefs, configuration |
| `aura-store` | 6 | Migrations, id counters, round-trips, events and cursors, corrupt rows reported |
| `aura-git` | 4 | Worktrees, commit and merge, conflict abort, compare-and-swap, exclude file |
| `aura-adapters` | 15 | Argv building, disabled providers, limit detection and reset parsing against fixtures |
| `aurad` (merge) | 3 | Landing on a checkout, failing checks block landing, conflicts leave main untouched |
| `aurad` (e2e) | 10 | See below |
| `aurad` (web) | 4 | Page served with security headers; API refuses requests without the token; foreign Host headers refused; token file created private |

The end-to-end suite runs the real engine on a temporary repository. A shell script stands
in for the agent CLIs and writes the same records a real agent is asked to write. It covers:

- a directive from plan to merge on `main`, including the reviewer and the harness mirror;
- a subscription limit: the run is refused, the provider pauses, the task requeues without
  using a run, and it completes after the reset;
- a daemon restart mid-run: the orphaned run is interrupted and the task completes;
- `ask-plan`: nothing runs before approval;
- an out-of-scope change: it never reaches main and the task blocks with the reason;
- a plan with a disallowed acceptance command: rejected, no tasks created;
- a failing acceptance check: nothing lands, the task returns to the worker with the failing output,
  and it blocks once its run budget is spent;
- a worker question: the task blocks, an answer reaches the next brief, and it completes;
- a rejected run is recorded as failed, not succeeded;
- the kill switch: running work returns to the queue and is not stranded.

A smoke run also exercised the real daemon, socket and CLI end to end (plan, work, review,
merge, pause, resume, manual task, out-of-scope rejection after the budget, and the
dashboard in a pseudo-terminal).

**Not verified:** the real provider CLIs and their limit messages; the systemd unit; the
dashboard under interactive use beyond rendering and quitting; behaviour under concurrent
load beyond the two-run tests; long-running merges with real test suites.

## Web interface

`aurad` serves the page on `127.0.0.1:7878` (set `[web] listen` to change it; the daemon refuses
non-loopback addresses). Run `aura web` to print the link. It carries an access token in the
URL fragment, which the browser never sends to the server. The page stores it in the browser and
removes it from the address bar.

- **Access:** the token lives in `state_dir/web.token` (mode 0600, created on first start). The
  API needs it as a bearer token. Requests whose `Host` header is not `127.0.0.1` or `localhost`
  are refused, which blocks DNS-rebinding attacks.
- **Safety in the page:** agent-written text is always inserted as text, never as HTML. The page
  sends a Content-Security-Policy that allows scripts only from the daemon.
- **Panels:** Tasks, Task (detail, approve, cancel, answer questions, runs), Directives (send a
  goal to the planner), Live log (follows the newest run), Quota & runs, Events. Pause, resume,
  kill (with confirmation) and reset-layout are in the top bar.
- **Refresh:** the page polls the daemon once a second. It does not hold a long connection, so
  closing it or losing the network changes nothing about running work.

**Checked:** the four HTTP tests above, and a headless Chromium run against the live daemon: the
page connects, shows six panels and live tasks, strips the token from the address bar, logs no
errors, submits a directive from the form, pauses and resumes, and follows the newest run's log.

**Not checked:** drag-to-dock and resize by mouse, and layout persistence across reloads. These
come from Dockview and have not been exercised by an automated test.

## Known limitations

- **Agents run with your privileges.** The scope check governs which files a task may change.
  It does not sandbox the agent process. Acceptance commands run `cargo` (or whatever you
  allow), so code the agent wrote can run during a merge check. Treat the worktrees as code
  you have reviewed, or add OS-level sandboxing before running unattended on untrusted work.
- **Planner output is trusted** within the rules above: the scope must be non-empty and
  glob-valid, dependencies must point to earlier tasks, and acceptance commands must use an
  allowed program. A plan can still ask for bad work inside those limits. `ask-plan` is the
  safety valve.
- **No timeout on merge checks.** A check that hangs stalls the merge queue (not the API).
  Cancelling that task waits for the merge to finish.
- **Single machine, single repository.** One daemon, one repo, one state directory.
- **Weekly caps** are not modelled separately. They appear only if the provider's refusal
  message names a time, or through back-off.
- **Quota for both workers and reviewers** is handled per provider. Reviews use a different
  provider from the worker unless `allow_self_review` is set.
- **Killed merges** are not interrupted. `kill` stops runs, not the merge thread.

## Operating it

- **State:** `~/.local/share/aura` (or `state_dir`). `aura.db` is the source of truth. `worktrees/`
  holds one checkout per task branch, `runs/` the logs, `mirror/` the `harness` worktree.
- **Logs:** `aura logs R-0007 --tail 300`, or the files in `runs/`.
- **Paths:** the socket path must stay under 108 bytes; the daemon refuses to start otherwise.
- **Repository side effect:** the daemon adds `.aura/` to the repository's shared exclude file
  (`.git/info/exclude`), so agent records never show up as changes in any worktree.
- **Back up** `aura.db` and push the `harness` branch to keep history off the machine.
- **Clean-up:** scratch and merge worktrees are removed at start. Task worktrees are removed when
  their task is done or cancelled, but task branches are kept, so the history of every attempt survives.

## Deviations from the design

| Design said | Built as | Why |
|---|---|---|
| Result record committed on the task branch | Written untracked under `.aura/` and read by the daemon; `.aura/` is excluded for every worktree | Keeps records out of `main`. Agents only commit code. |
| Rebase a conflicting branch in the integrator | Abort the merge; send the task back with the conflicting files; the worker merges main itself | Keeps the worker's history intact and needs no agent in the integrator |
| Scheduler wakes on events | Level-triggered tick every 500 ms, derived from the database | Simpler and restart-proof. Latency is at most half a second. |
| Quota check before each run only | Also recorded from refusals, and the provider is marked available once the reset time has passed | The probe is the next real run, so no separate probe job is needed |
| Reviewer always differs from the worker | Default; `allow_self_review` relaxes it when nothing else is available | A single-provider setup would otherwise never review |
| Acceptance commands as shell lines | Argv with an allowlist and no shell | Prevents a plan from chaining commands |
| Roles as single providers | Ordered lists; the first usable provider is taken | Lets the scheduler route around a limited provider |
