# Autonomous Aura harness: design

Status: **draft for review**. Nothing here is implemented yet.

This document extends [aura-harness.md](../aura-harness.md). That file describes the manual
process as it runs today: humans start agents in zellij windows, and each agent polls a
`handover.md` file. This design keeps the parts that work (one worktree per agent, git as
the ledger, one manager, a clear merge gate) and replaces the parts that depend on a
human keeping terminals open.

---

## 0. Summary

- A **daemon** (`aurad`) runs on the local server as a user service. It is the only thing
  that starts, stops and schedules agents. It is also the only writer of the task state.
- Agents are **not long-lived sessions**. Each agent run is a short, headless invocation
  (`claude -p`, `codex exec`, …) that receives one task brief, works in one worktree,
  writes a structured result, and exits. The daemon decides when the next run happens.
- **Clients** (a local UI, a Telegram bot, a CLI) talk to the daemon over a local API. A
  client can be opened, closed or crashed at any time. Nothing running agents depends on
  it.
- The **human** gives direction in three places: a *directive* to the manager, *approvals*
  at gates set by an autonomy level, and *answers* to questions that agents raise.
- Git stays the durable record of code and of the decisions that touched it. The daemon's
  database is the live state. A daemon-written mirror in git keeps the history readable.

---

## 1. Goals and non-goals

### Goals

1. **Survive the UI.** Closing the window, the terminal or the phone must not stop, pause
   or lose any agent work.
2. **Coordinate without a human relaying messages.** Work passes between roles through
   task records and git, and the daemon enforces the protocol instead of trusting agents
   to follow it.
3. **Keep the human in control at the right points.** The human sets goals and budgets,
   approves what needs approval, and can stop anything at any time, from any client.
4. **Verify mechanically.** Test results and merge decisions come from the daemon running
   the checks, not from an agent's report that the checks passed.
5. **Be auditable.** Every run, decision, question and answer is an event that can be
   replayed.

### Non-goals (for now)

- Multi-machine scheduling. One local server first.
- Running untrusted code from outside the repository. Agents work on code the team owns.
- Replacing the agent CLIs. The harness drives them, it does not reimplement them.
- A web app with accounts. The UI is local and single-user.

---

## 2. Architecture

```
            ┌──────────────────────── local server ────────────────────────┐
            │                                                              │
 Telegram ──┤  telegram-bridge ──┐                                         │
 (phone)    │                    │   local API (unix socket + token)       │
            │                    ├──────────────►  aurad (daemon)  ◄───────┼── UI (TUI/web)
 CLI ───────┤  aura ─────────────┘                 │    ▲                   │   (optional,
            │                                      │    │ events (SSE)      │    open/close)
            │                       ┌──────────────┼────┴─────────┐         │
            │                       ▼              ▼              ▼         │
            │                  scheduler      merge queue     state store   │
            │                       │              │         (SQLite +     │
            │                       ▼              ▼          event log)     │
            │              agent runs         integrator                   │
            │          (headless CLI in     (runs tests on                 │
            │           its own worktree)    merge candidates)             │
            │                       │                                       │
            │                       ▼                                       │
            │        git: worktrees + branches + .aura/ mirror on `harness` │
            └──────────────────────────────────────────────────────────────┘
```

### Components

| Component | Responsibility | Notes |
|---|---|---|
| `aurad` | Owns scheduling, worktrees, agent processes, the merge queue, budgets, and the state store. | Runs as a systemd user service with `Restart=on-failure`, so it survives logouts and reboots. |
| State store | SQLite file in `~/.local/share/aura/`. Tables: `tasks`, `runs`, `questions`, `events`, `agents`, `budgets`. | Single writer (the daemon). WAL mode. Backed up with the repo. |
| Scheduler | Picks ready tasks, checks scope locks and budgets, starts runs. | Event-driven, not polling. A run finishing or a directive arriving wakes it. A 60 s timer is the fallback. |
| Agent runner | Starts one headless run for one role on one task, with a timeout and a log file. | One adapter per CLI (see §6). |
| Merge queue | Serializes merges into `master`. Rebases or merges each candidate, runs the checks, merges or rejects. | Only the daemon moves `master`. |
| Integrator | The check runner. Runs the repo's test suites on a candidate and returns a pass/fail record. | Results are stored, not reported by agents. |
| Local API | Versioned HTTP+JSON on a unix socket, plus an SSE event stream with a cursor. | Token-authenticated. No TCP port by default. |
| Clients | `aura` CLI, telegram-bridge, UI. | Stateless. Reconnect by replaying events from their last cursor. |

### Why a daemon and not more zellij windows

- A zellij window is a process the user owns. Closing it can kill the agent. The daemon's
  agents are children of a service, so they outlive every client.
- Polling `handover.md` from a loop inside an agent session breaks when the session is
  rate-limited, compacted or killed. A scheduler in the daemon does not depend on any agent
  staying alive.
- One place decides who runs and who merges. Today that is spread across per-agent loops.

---

## 3. Roles

The manual process had three roles. The autonomous version splits the work so that no
single agent both writes and approves its own code.

| Role | Count | Does | Never does |
|---|---|---|---|
| **Human** | 1 | Sets goals, budgets and autonomy level; approves gates; answers questions; can pause or stop anything. | — |
| **Planner** (the manager) | 1 at a time | Turns a directive into tasks with briefs, scope and acceptance checks. Re-plans when a task fails. Writes no product code by default. | Merge; approve its own plans past the autonomy gate; change budgets. |
| **Worker** | N (one per active task) | Implements one task in its own worktree and branch. Writes a result record. | Touch files outside its declared scope; move `master`; mark its own task merged. |
| **Reviewer** | 1 per candidate | Reads the diff against the brief and acceptance checks. Returns `approve` or `request-changes` with reasons. Ideally a different model from the worker. | Edit the code it reviews. |
| **Integrator** | daemon, not an agent | Runs the checks on a candidate and records pass/fail. | — |

The planner is an agent with a planning brief, not a special process. "Exactly one
manager" from the manual design becomes "exactly one planner at a time for a given
directive". Several directives can be planned in parallel, but the merge queue still
serializes what lands.

---

## 4. Task model

### 4.1 States

```
 PROPOSED ──approve──▶ QUEUED ──start──▶ RUNNING ──result──▶ REVIEW ──approve──▶ MERGE_QUEUED ──merged──▶ DONE
     │                    ▲                 │  │                 │                    │
     │                    │                 │  │ request-changes │                    │ checks fail
     │                    │                 │  ▼                 ▼                    ▼
     │                    └────retry────── FAILED ◀─────────── RUNNING            REJECTED ─▶ QUEUED (rework) or BLOCKED
     │                                      │
     └──reject──▶ CANCELLED                 └──budget or retries exhausted──▶ BLOCKED
                                                                              (waits for human)
```

- `PROPOSED`: the planner has written the task; the autonomy level decides whether a human
  must approve it.
- `QUEUED`: ready to run once dependencies are `DONE` and its write scope is free.
- `RUNNING`: a worker run is in progress in the task's worktree.
- `REVIEW`: a result exists; the reviewer runs.
- `MERGE_QUEUED`: approved and waiting for the integrator.
- `BLOCKED`: needs a human answer, a budget increase, or a plan change. Shows up in every
  client and in Telegram.
- `DONE`, `CANCELLED`: terminal. A `DONE` task has a merge commit on `master`.

### 4.2 Task record

```yaml
id: T-0042
directive: D-007                 # the human request this came from
title: Add retry to the upload client
role: worker
depends_on: [T-0040]
scope:                            # write scope; the scheduler locks these paths
  write: ["src/upload/**", "tests/upload/**"]
  read:  ["src/**"]
brief: |
  ...what to build, why, and what not to touch...
acceptance:                       # checks the integrator runs, not the agent
  - cmd: "npm test -- upload"
  - cmd: "npm run lint"
budget:
  max_runs: 3
  max_tokens: 2_000_000
  max_wall_minutes: 60
branch: task/T-0042
worktree: ~/aura/worktrees/T-0042
autonomy: auto                    # auto | ask-plan | ask-each
```

### 4.3 Result record

Each worker run must end by writing `.aura/result.json` in its worktree and committing it
on its branch:

```json
{
  "task": "T-0042",
  "run": "R-0107",
  "status": "ready_for_review",       // or "blocked", "failed"
  "summary": "Added exponential backoff to upload client",
  "files_changed": ["src/upload/client.ts", "tests/upload/client.test.ts"],
  "questions": [],                    // filled when status is "blocked"
  "self_check": "not trusted: integrator reruns acceptance checks"
}
```

The daemon checks that `files_changed` stays inside `scope.write`. A run that writes outside
its scope is failed, whatever its summary says.

### 4.4 Scope locks

Two tasks may run at the same time only if their write scopes do not overlap. The scheduler
computes overlap with glob matching on `scope.write`. This replaces the manual rule "do not
touch files another agent owns in the same round". A brief can still name files, but the
lock is what enforces it.

---

## 5. How the agents coordinate

Agents never message each other. Coordination happens through four channels, each with a
single writer.

| Channel | Written by | Read by | Purpose |
|---|---|---|---|
| Task records (state store) | Planner, daemon | Everyone, via the scheduler | What to do, in what order, with what scope |
| Branch `task/<id>` | Worker | Reviewer, integrator | The code under review |
| Result record `.aura/result.json` | Worker | Daemon, reviewer | What the worker claims it did and any question it has |
| Mirror on the `harness` branch (`.aura/tasks/<id>.md`) | Daemon only | Humans, git history | Readable, versioned log of every task. Not used for scheduling. |

### 5.1 Worker run, step by step

1. The scheduler picks `T-0042`. It checks dependencies and the scope lock, creates
   `task/T-0042` from current `master`, and creates the worktree.
2. The daemon starts a worker run with the brief, the result-record format and the
   scope. The run gets the worktree as its working directory and nothing else.
3. The worker works. It may run the acceptance checks itself to iterate, but the result
   does not depend on that.
4. The worker writes `.aura/result.json` and commits. It exits.
5. The daemon records the run, checks the scope, and moves the task to `REVIEW`.
6. The reviewer run starts on the diff `master...task/T-0042` with the brief and the
   acceptance checks. It writes its verdict as a record. It does not edit code.
7. On `approve`, the task moves to `MERGE_QUEUED`. On `request-changes`, the reviewer's
   notes become input to a new worker run on the same branch (`RUNNING` again, counted
   against the budget).

### 5.2 Merge queue

The merge queue is the only path into `master`. For each candidate, in order:

1. Create a temporary worktree from current `master`, merge the candidate branch into it.
2. If the merge conflicts, do not resolve it with an agent by default. Mark the task
   `REJECTED` with the conflicting files and queue a rework run, which rebases on the
   current `master`. (Rebase is allowed here because the task branch is the worker's own
   and has not been published. See §8.)
3. Run every acceptance check on the merge result.
4. All green: fast-forward or merge `master` to the candidate, record the merge commit,
   mark `DONE`, and update the mirror.
5. Any red: `REJECTED`. The failure output goes to the worker's next run.

Merges are serialized. The queue never runs two candidates at once, so every check runs
against the exact `master` it merges into.

### 5.3 Planner loop

The planner is triggered by a directive, by a task failing past its retry budget, or by a
human answer that changes the plan. It:

1. Reads the directive, the current `master`, open tasks, and the recent events.
2. Writes new `PROPOSED` tasks with briefs, scopes, dependencies and acceptance checks.
3. Writes a short plan summary as a `directive_plan` event.
4. Stops. It does not start workers. The scheduler does, after the gate in §7.

### 5.4 Failure and recovery

| Failure | Detection | Response |
|---|---|---|
| Run crashes or hits the wall-clock timeout | Process exit or timer | Retry while the run budget allows, then `BLOCKED` |
| No output or commits for N minutes | Log-idle watchdog | Kill, count as a failed run |
| Rate limit or provider outage | Adapter exit code | Back off the whole provider, not the task. Other providers keep running. |
| Write outside scope | Daemon checks `files_changed` and the diff | Fail the run; the task reruns with the violation as feedback |
| Acceptance checks fail on merge | Integrator | `REJECTED`, failure output to the next worker run |
| Budget exceeded | Daemon counter | `BLOCKED`; human decides whether to raise it |
| Two tasks need the same scope | Scheduler | Second task waits. No failure. |

Every failure is an event. Nothing is silently retried forever: each task has a run
budget, and each directive has a total token and time budget.

---

## 6. Agent adapters

The daemon knows nothing about a specific CLI. An adapter implements:

```
start(role, brief, worktree, env, timeout) -> run_handle
poll(run_handle) -> running | exited(code) | rate_limited(retry_at)
stop(run_handle)
usage(run_handle) -> tokens, cost
```

- **Claude Code:** `claude -p` with a permission mode limited to the worktree; output in
  JSON for usage accounting.
- **Codex, Gemini, CodeBuddy:** the same contract through their headless modes. Each
  adapter is optional. The harness works with one provider.
- The reviewer should default to a different provider from the worker. The config sets
  that pairing, so the same model does not review its own code.

Each run gets its own log file under `~/.local/share/aura/runs/<run-id>.log`. Logs are
viewable from every client.

### Sandboxing

- Worker and reviewer runs are started with a working directory of their worktree and a
  restricted tool list. They have no access to `~/.ssh`, the state store, the bot token or
  other worktrees.
- Secrets are passed as environment variables only to the adapter that needs them. They
  are never written into a worktree, a brief, a result record or a Telegram message.
- Live provider calls from workers are not needed for normal work. The planner and the
  integrator are the only places that run anything that touches external services, and
  only when the task says so.

---

## 7. The human in the loop

### 7.1 Three kinds of input

| Input | Meaning | Example |
|---|---|---|
| **Directive** | A goal for the planner | "Make uploads retry on network errors. Keep the public API unchanged." |
| **Approval** | Permission to move a gated step | "Approve plan D-007" or "Approve T-0042 to merge" |
| **Answer** | Reply to a `BLOCKED` question | "Use exponential backoff capped at 30 s" |

Plus two controls that apply at any time: **pause** and **stop**.

### 7.2 Autonomy levels

The human sets the level per directive, with a default for the whole harness.

| Level | Plan | Each task starts | Merge | Notify on |
|---|---|---|---|---|
| `ask-each` | Human approves | Human approves | Human approves | Everything |
| `ask-plan` | Human approves | Automatic | Automatic if checks green | Blocks, merges, failures |
| `auto` (default) | Automatic | Automatic | Automatic if checks green | Blocks, failures, budget warnings, end of directive |

Every level still stops for: a `BLOCKED` question, a budget overrun, a scope violation that
repeats, and a failure after the retry budget. Autonomy never removes the human's right to
stop.

### 7.3 Directive flow

1. Human sends `directive "Make uploads retry..."` from any client, with an optional level.
2. Daemon stores the directive and starts the planner.
3. Planner writes tasks as `PROPOSED`. The daemon posts a plan summary to every client
   and to Telegram.
4. Under `ask-plan` or `ask-each`, the human approves (`approve D-007`). Under `auto`, the
   scheduler starts immediately and the summary is only a notification.
5. Tasks run, review and merge. Progress shows in each client. The directive ends when
   every task is `DONE` or `CANCELLED`, and the daemon sends a final report.

### 7.4 Questions

A worker that needs a decision writes `status: blocked` and a question in its result
record. The daemon:

1. Creates a `questions` row, marks the task `BLOCKED`, and notifies the human.
2. Accepts the answer from any client. The answer is stored and attached to the task.
3. Starts a new run with the question and the answer in the brief. The run's worktree and
   branch are kept.

If the planner can answer a question inside the directive's scope, it may do so and log
that it did. Anything about scope, budget, or a product decision goes to the human.

### 7.5 Controls

| Control | Scope | Effect |
|---|---|---|
| `pause` | Global, per directive, per agent | Scheduler starts nothing new. Running runs finish their current step and stop at the next checkpoint. |
| `resume` | Same | Reverses pause |
| `stop` | Per task or per directive | Kills the run, marks the task `CANCELLED`, removes the worktree after the branch is kept. |
| `kill-switch` | Global | Kills all runs immediately and pauses the scheduler. Needs a confirmation step. |

Pause and stop are the only controls a client has to be able to send in a hurry, so they
are designed to be sent from a phone.

---

## 8. Git layout and rules

- `master`: moved only by the merge queue.
- `task/<id>`: one per task, created from `master` when the task starts. Owned by its
  worker. Rebased by the daemon only while unpublished. Never force-pushed to a shared
  remote.
- `harness`: a branch the daemon writes to, holding the mirror in `.aura/tasks/`,
  `.aura/directives/` and `.aura/events.jsonl`. Humans read it; agents do not write it.
  Keeping it separate means mirror commits never conflict with code.
- Worktrees live under `~/aura/worktrees/<task-id>` and are removed after the task is
  `DONE` or `CANCELLED` and its branch is kept.
- The **merge rule** from the manual process still holds: a branch merges only when its
  checks are green on the merge result. The integrator now enforces it.

The `handover.md` file from the manual process becomes a generated view. Its sections map
to records:

| Old `handover.md` section | New home |
|---|---|
| Role table | `agents` table, config file |
| Ports | `agents.ports`, allocated by the daemon |
| Watcher control | Agent state (`active`, `paused`, `stopped`) |
| Current task per agent | `tasks` table |
| Assignments | Task briefs |
| Merge log | `events` table, mirrored to `harness` |
| Worker sections | `runs` and result records |

---

## 9. Local interface (later phase)

The UI is a client of the daemon's API. It has no state of its own beyond view settings.

Requirements, from the request:

- Open and close at any time without affecting agent runs.
- Reconnects and shows current state: resume from the last event cursor, not a full
  refresh.

What it shows:

- Directives and their progress (a list with a progress bar per directive).
- Task board by state, with the scope locks in use.
- Live run logs, one tab per run.
- Questions waiting for an answer, at the top.
- Pause, resume, stop, and approval buttons.
- Budget use per directive and per provider.

Implementation options, to decide once the daemon exists:

1. **Terminal UI** (for example Ratatui in Rust or Textual in Python). Fits how zellij is
   used today. Works over SSH.
2. **Local web UI** served by `aurad` on `127.0.0.1` only, opened in a browser. Easiest
   to make rich (diffs, logs). Needs a token.

Recommendation: start with a small TUI for the first version, since the user is in a
terminal already. Add the web UI later if diffs and logs are needed in a browser.

---

## 10. Telegram bridge

Telegram is a client, not a second control path. Every command maps to a daemon API call,
and the bridge keeps no state.

- **Transport:** long polling from the local server to the Telegram Bot API. No inbound
  port is opened. Webhooks are not needed.
- **Auth:** an allowlist of Telegram user IDs in config. Messages from other IDs are
  dropped and logged. The bot token lives in an env file readable only by the service
  user.
- **Commands:**

| Command | Maps to |
|---|---|
| `/status` | Summary of directives, running tasks, blocked items, budget |
| `/directive <text>` | New directive, default autonomy |
| `/directive <level> <text>` | New directive with a given autonomy level |
| `/approve <id>` | Approve a plan or a merge, by id |
| `/answer <question-id> <text>` | Answer a blocked question |
| `/pause`, `/resume` | Global pause and resume |
| `/stop <task-id>` | Cancel a task |
| `/kill` | Kill switch, with a confirmation reply required |
| `/log <run-id>` | Last lines of a run log |

- **Pushes:** blocked questions, merge results, failures, budget warnings, end-of-directive
  reports. Quiet hours can be set so only blocks and failures notify.
- **Safety:** `/kill` and `/stop` need a confirmation reply. Free text sent to the bot
  becomes a directive only with the `/directive` prefix, so an accidental message is not
  an instruction.
- **Message content:** summaries, not diffs. Long output is linked to the local UI or
  shown as a truncated log tail.

---

## 11. Configuration

One file, `~/.config/aura/aura.toml`, read by the daemon at start and on `reload`:

```toml
[repo]
path = "/home/user/aura-harness"
main_branch = "master"
harness_branch = "harness"

[defaults]
autonomy = "auto"
max_parallel_runs = 3

[providers.claude]
command = "claude"
max_parallel = 2

[providers.codex]
command = "codex"
max_parallel = 1

[roles]
planner  = "claude"
worker   = ["claude", "codex"]     # used in order, or by task label
reviewer = "codex"                 # different from the worker by default

[budgets]
per_directive_tokens = 20_000_000
per_task_runs = 3
run_timeout_minutes = 60
idle_timeout_minutes = 10

[telegram]
enabled = true
allowed_user_ids = [123456789]
quiet_hours = "23:00-07:00"        # blocks and failures still notify
```

Secrets are not in this file. They live in `~/.config/aura/env` (mode 600) and are loaded
by the systemd unit.

---

## 12. Operating the daemon

- **Service:** `systemctl --user enable --now aurad`. Logs go to the journal and to
  `~/.local/share/aura/logs/`.
- **Startup:** on start, the daemon reconciles state. Runs marked `RUNNING` whose process is
  gone are marked `FAILED` and their tasks retried or blocked. Worktrees without an open
  task are removed after a grace period.
- **Upgrades:** the daemon finishes or pauses before it restarts. A schema version in the
  database gates migrations.
- **Backups:** the SQLite file is copied on a timer. The `harness` branch is pushed to the
  remote, so the history is off-machine even if the server is not.
- **Resources:** `max_parallel_runs` caps concurrency. The daemon checks free disk before
  creating a worktree, because worktrees are full checkouts.
- **Ports:** the daemon allocates per-task ports from a range and frees them when a task
  ends. This replaces the fixed port table.

---

## 13. Rollout

Each phase is usable on its own, and the manual process keeps working until phase 3.

| Phase | Deliverable | Done when |
|---|---|---|
| **0. Records** | Task and directive schema; `aura` CLI to create and list them; the `harness` mirror | A manual task can be created, listed and mirrored without any agent running |
| **1. One worker** | Daemon, scheduler, claude adapter, one worker run per task, result record, scope check | A task runs headless, leaves a result record, and its state is correct after a daemon restart |
| **2. Review and merge queue** | Reviewer role, integrator, serialized merge queue, rework loop | A task goes from directive to `DONE` on `master` with no human action under `auto` |
| **3. Parallel and multi-provider** | Scope locks, concurrency limits, codex/gemini adapters, budgets | Two non-overlapping tasks run at once; a provider outage does not stop the others |
| **4. Telegram** | Bridge, auth, commands, pushes, confirmations | Directive, approve, answer, pause, and kill all work from a phone |
| **5. Local UI** | TUI (or web) as a client; reconnect from event cursor | Closing and reopening the UI changes nothing about running work |
| **6. Hardening** | Sandbox review, reconciliation tests, chaos tests (kill daemon mid-run), backups | Daemon killed at each state transition recovers to a consistent state |

Phase 0 and phase 1 are the first things to build. The UI is last on purpose: it is the
part the user asked to be able to close, and it should be the easiest to replace.

---

## 14. Decisions

Settled by this draft (change them if you disagree):

1. Agents are one-shot headless runs, not kept-alive sessions. The daemon schedules them.
2. The daemon is the only writer of state and of `master`.
3. Reviewer is a separate role and defaults to a different provider from the worker.
4. Acceptance checks are run by the integrator, never self-reported.
5. Coordination is by records and git only. Agents do not message each other.
6. Telegram uses long polling and an allowlist. No inbound port.
7. The UI is a client, and it is built after the daemon.

Open questions for you:

1. **Language and stack.** Rust (matches a long-running daemon well) or TypeScript/Python
   (faster to iterate, and the agent CLIs are already in those ecosystems)? I lean Rust
   for the daemon and a TUI, but either works.
2. **Which providers you actually run.** This sets the adapters for phase 1 and 3. Claude
   Code only is the smallest first step.
3. **Target repos.** Only `aura-harness`, or also the Compass repo that the manual process
   was written for? The config supports several repos, but the first version targets one.
4. **Default autonomy.** `auto` assumes you trust the checks. `ask-plan` is the safer
   starting point for the first few directives.
5. **Budget caps.** The numbers in §11 are placeholders. What spend or token ceiling per
   directive would you accept?
6. **Telegram.** Is a bot you create yourself with your Telegram user ID allowed, and do
   you want quiet hours?

---

## 15. Glossary

- **Directive:** a human goal given to the planner.
- **Task:** one unit of work with a brief, scope, dependencies and acceptance checks.
- **Run:** one headless invocation of an agent for one task and one role.
- **Scope lock:** the write paths a running task has claimed.
- **Gate:** a point where autonomy level decides whether a human must approve.
- **Integrator:** the daemon component that runs acceptance checks on merge candidates.
- **Mirror:** the human-readable copy of state written to the `harness` branch.
