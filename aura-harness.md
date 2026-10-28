# Aura harness: coordinating several coding agents with `handover.md`

The Aura harness lets several coding agents (Claude, Codex, Gemini, CodeBuddy, …) work on
one repository in parallel without stepping on each other. It has four parts:

1. **One git worktree and branch per agent.** Agents never share a working directory.
2. **One handover file, `handover.md`.** It holds the assignments, the reports and the
   watcher on/off switches. Agents coordinate only through this file and git; they do not
   message each other.
3. **One watcher per agent.** Each agent polls `handover.md` on a timer, picks up its
   assignment, does the work and hands it back without a human relaying messages.
4. **One manager at a time, chosen by the human.** Only the manager writes assignments,
   merges branches and commits to the main branch.

The human in the loop decides what gets built and who manages. The manager turns that
into assignments and integrates the results. Workers carry out the assignments.

---

## 1. Roles

| Role | Who | May do | May not do |
|---|---|---|---|
| **Human in the loop** | The user | Choose the manager, set priorities, approve scope changes, answer `BLOCKED` questions, tell any agent to stop | — |
| **Manager** (called "main agent" in `handover.md`) | Exactly one agent, named by the human | Write assignments; set watcher switches; review, merge and commit to `master`; archive handoffs; reset agent sections | Merge unreviewed work; merge a branch that isn't `READY FOR MERGE`; take scope decisions that belong to the human |
| **Worker** | Every other active agent | Work in its own worktree and branch; edit its own `handover.md` section; commit to its own branch | Commit to `master`; merge other branches; edit another agent's section or worktree; write assignments |
| **Excluded agent** | An agent the human has taken off tasks | Nothing: its watcher is `STOP` and it gets no assignments | — |

### Exactly one manager

- The human names the manager. By default it is Claude in the main repository, "unless the
  user says otherwise" (`AGENTS.md` rule 10). No agent makes itself manager.
- There is never more than one manager. Two managers merging into `master` at the same time
  would race on merges, assignments and the archive, so this rule has no exceptions.
- **Changing manager:** the human tells the outgoing manager to stop. The outgoing manager
  finishes or abandons any merge in progress, leaves `master` clean, and sets every
  watcher to `STOP`. The human then names the new manager, which updates the role table in
  `handover.md` and `AGENTS.md` rule 10 on `master` before it writes any assignment.
- The manager can do worker tasks itself. Since 2026-10-19 the manager (Claude) runs its
  session in its own worktree, `nebius-compass-claude`, and does its own tasks on the
  `claude` branch. `master` stays checked out in `nebius-compass` as the **integration
  checkout**: the manager uses it only through `git -C /home/makodev58/nebius-compass …`
  and by running the suites there, to write assignments, merge reviewed branches (its own
  `claude` branch included) and log merges. Git cannot check out `master` in two worktrees,
  so the integration checkout must stay on `master`.

---

## 2. Layout

```
/home/makodev58/nebius-compass              master     ← integration checkout, manager only
/home/makodev58/nebius-compass-codex        codex      ← worker
/home/makodev58/nebius-compass-gemini       gemini     ← worker (excluded since 2026-10-17)
/home/makodev58/nebius-compass-codebuddy    codebuddy  ← worker (excluded since 2026-10-17)
/home/makodev58/nebius-compass-claude       claude     ← manager's session and own work (since 2026-10-19)
```

All of these are worktrees of one repository, so every agent sees every branch through
git (`git show master:handover.md`, `git log codex`) without opening another agent's
folder.

Setting up a new worker (the manager does this once):

```bash
git -C /home/makodev58/nebius-compass worktree add /home/makodev58/nebius-compass-<agent> -b <agent>
cd /home/makodev58/nebius-compass-<agent> && npm install
```

Then add the agent to the role table, the **Ports** table, the **Watcher control** table
and the **Current task per agent** table in `handover.md`, and give it an empty
`## <agent>` section.

### Boundaries every agent keeps

- **Stay in `nebius` folders** (`AGENTS.md` rule 11; replaced the former `amd`-folder rule
  on 2026-10-19). Work only in `nebius-compass` and the `nebius-compass-<agent>` worktrees.
  Never touch other folders such as `~/amd-compass`, `~/compass` or `~/ResponseCompass`.
- **Own ports only.** Each agent has its own E2E port and preview port in the **Ports**
  table. It frees them as soon as it is done (check with `ss -ltn | grep :<port>`). It never
  binds ports that belong to other apps (3000, 3200) and never stops a process it did not
  start.
- **No secrets in worker worktrees.** Workers make no live provider calls; the manager runs
  any live check.

---

## 3. `handover.md`

`handover.md` lives at the repository root and has the same structure on every branch:

| Section | Written by | Purpose |
|---|---|---|
| Role table and **Protocol** | Manager | Who works where and the rules below |
| **Main agent → Ports** | Manager | Port allocation per agent |
| **Main agent → Watcher control** | Manager | `RUN` / `STOP` switch per agent |
| **Main agent → Assignments** | Manager | The **Current task per agent** table (authoritative) plus a detailed brief per task, grouped into rounds |
| **Main agent → Merge log** | Manager | One line per merge: branch commit → merge commit, test counts, notes |
| `## <agent>` (one per worker) | That worker only | The handoff report form |

### Status values

`IDLE` · `IN PROGRESS` · `READY FOR MERGE` · `BLOCKED`

### Worker report form

A worker replaces its whole section at each handoff. It does not keep a running log,
because the manager archives each handoff when it merges.

```markdown
## codex

- **Status:** READY FOR MERGE
- **Updated:** 2026-10-18
- **Task:** <which assignment, and the master commit merged before starting>
- **<Topic>:** <what changed, one bullet per area>
- **Verification:** `npm test` N/N; E2E N/N on <port>; ports freed.
- **Boundaries / docs:** <what was deliberately not touched; project-state pages updated>
- **Risks / follow-ups:** <known limits>
- **Questions for main:** <None, or the question that caused BLOCKED>
```

### Why one shared file works

- **Each section has one writer.** The manager writes the `Main agent` section only on
  `master`; each worker writes only its own section on its own branch. Two agents never
  edit the same lines.
- **Merge conflicts have a fixed resolution.** If `handover.md` conflicts during a merge, the
  manager keeps the worker branch's version of that worker's section and `master`'s version
  of every other section.
- **Workers get assignments through git.** A worker runs `git merge master` and receives the
  latest assignments together with the latest code.
- **History is kept elsewhere.** After each merge the manager moves the handoff into
  `docs/handover-history.md` (newest first) and resets the worker's section to `IDLE`, so
  `handover.md` stays short enough to read on every poll.

---

## 4. The task lifecycle

```
 human ──priorities──▶ MANAGER (master)
                         │ 1. writes assignment + sets watcher RUN, commits to master
                         ▼
                       WORKER watcher sees a task for it on master
                         │ 2. git merge master
                         │ 3. Status: IN PROGRESS, does the work, both test suites
                         │ 4. Status: READY FOR MERGE (or BLOCKED + question), commits to own branch
                         ▼
                       MANAGER watcher sees READY FOR MERGE / BLOCKED on the branch
                         │ 5. reviews diff vs master, merges, reruns both suites on master
                         │ 6. Merge log line, archives handoff, resets section to IDLE
                         │ 7. sets current task "none" and watcher STOP (or assigns the next task)
                         ▼
                       WORKER watcher sees STOP / "none" and exits
```

### Worker steps

1. `git merge master` in its own worktree before starting.
2. Read its row in **Current task per agent** and its brief under **Assignments**.
3. Set its section to `IN PROGRESS` and commit.
4. Do the work in its own worktree only. Update the relevant `project-state/` pages and
   `project-state/log.md`. Add new entries at the bottom of append-only files
   (`docs/decisions.md`, `project-state/log.md`) and never edit earlier entries.
5. Run both suites (`AGENTS.md` rule 9) on its own E2E port:
   `npm test`, then
   `COMPASS_E2E_PORT=<port> LD_LIBRARY_PATH=/home/makodev58/rendertest/libs/usr/lib/x86_64-linux-gnu npm run test:e2e`.
6. Fill in the report form, set `READY FOR MERGE`, and commit the work and the report
   together. **The manager merges commits only**, so uncommitted work is never merged.
   Don't push unless told to.
7. If stuck or a decision is needed, set `BLOCKED`, write the question under **Questions
   for main**, and commit.

### Manager steps

1. Write the brief under **Assignments**, set the agent's row in **Current task per agent**,
   set its watcher to `RUN`, and commit to `master`. A good brief names the files the
   worker owns, the files it must not touch (especially files another agent owns in the
   same round), the tests expected, and the docs to update.
2. When a branch shows `READY FOR MERGE`: review `git diff master...<agent>`, then merge it.
3. Rerun both suites on `master`. If they fail, fix the problem on `master` or send the work
   back. Never record a merge as green when it wasn't.
4. Add a **Merge log** line (date, agent, branch commit → merge commit, what it delivered,
   conflicts and how they were resolved, test counts).
5. Copy the handoff into `docs/handover-history.md` under **Merged handoffs**, reset the
   worker's section on `master` to `IDLE`, and set the next task, or set the task to
   "none" and the watcher to `STOP`.
6. When a branch shows `BLOCKED`: answer it under the assignment if the answer is within the
   human's direction. Otherwise ask the human, then write the answer down.

---

## 5. Watchers

A watcher is a loop each agent runs in its own session. It reads `handover.md` on a timer
and acts on what it finds, so work moves between agents without the human copying
messages around. The polling interval is **300 seconds** (5 minutes) by convention.

### What every watcher reads

Watchers always read the **manager's copy on `master`**, never a worker's local copy:

```bash
git show master:handover.md
```

`master` is the only place assignments and watcher switches are authoritative. A worker's
own checkout may be behind until it runs `git merge master`.

### Worker watcher

On each tick:

1. Read `git show master:handover.md`.
2. If its row in **Watcher control** is `STOP`, **exit now and do not restart** until the
   manager sets `RUN` again.
3. If its row in **Current task per agent** is "none", exit.
4. If there is a task it has not handed back yet (its own section on its own branch is not
   `READY FOR MERGE` for that task), run the worker steps in §4.
5. After setting `READY FOR MERGE` or `BLOCKED`, keep polling. The next relevant change is
   either a new assignment, an answer to its question, or `STOP` / "none" after the merge.

### Manager watcher

On each tick:

1. For each active worker, read that worker's section on its branch
   (`git show <agent>:handover.md`). Reading through git means the manager never opens
   another agent's worktree.
2. `READY FOR MERGE` → run the manager merge steps in §4.
3. `BLOCKED` → answer it or escalate to the human.
4. Stop once no agent has an open assignment.

### Stop conditions

Watchers that only waited on each other would poll forever, so a watcher stops when any of
these is true:

- **`STOP`** in its **Watcher control** row. The manager sets this as soon as an agent has
  no work, and for excluded agents permanently.
- **"none"** in its **Current task per agent** row.
- **The human says stop.** This overrides everything. A watcher the human stopped stays
  stopped even if the table says `RUN`, until the human restarts it. Workers note this in
  their report ("Watcher remains stopped per user instruction").
- For the manager: no open assignments remain.

### Running a watcher

The harness does not depend on a particular loop mechanism. Each agent uses whatever its
CLI provides:

- **Claude Code:** `/loop 5m <prompt>`, or a self-paced `/loop` using `ScheduleWakeup`
  with `delaySeconds: 300`. The prompt tells it to follow the worker (or manager) watcher
  steps above, and the loop ends with `ScheduleWakeup(stop: true)` when a stop condition
  holds.
- **Other CLIs:** their equivalent scheduled or recurring task, or a shell loop that starts
  a headless run on each tick.

A shell form of the worker check, for reference:

```bash
AGENT=codex
while :; do
  board=$(git -C /home/makodev58/nebius-compass-$AGENT show master:handover.md)
  # Watcher control row, e.g. "| codex | RUN |"
  echo "$board" | grep -Eq "^\| $AGENT \| STOP" && break
  # Current task row, e.g. "| codex | none | — |"
  echo "$board" | grep -Eq "^\| $AGENT \| none" && break
  <run the agent headlessly with: "Follow aura-harness.md §4 worker steps for your assignment">
  sleep 300
done
```

Example watcher prompts:

> **Worker (codex):** Every 5 minutes, read `git show master:handover.md`. Stop if the
> codex Watcher control row is `STOP` or its current task is "none". Otherwise, if you
> have an assignment you have not handed back, `git merge master`, do it per the
> handover protocol, run both suites on port 3021, fill in your `## codex` section, set
> `READY FOR MERGE` or `BLOCKED`, and commit to the `codex` branch.

> **Manager:** Every 5 minutes, read each active worker's section with
> `git show <agent>:handover.md`. For `READY FOR MERGE`, review, merge into master,
> rerun both suites, log, archive and reset to IDLE. For `BLOCKED`, answer it or ask the
> human. Set the watcher to `STOP` for any agent left with no task. Stop when no agent
> has an open assignment.

---

## 6. Rules summary

1. The human picks the manager. Only one manager exists at a time.
2. Only the manager commits to `master`, merges branches, writes assignments and sets
   watcher switches.
3. Each worker works only in its own worktree, on its own branch, and edits only its own
   `handover.md` section.
4. Assignments and switches are read from `master`; reports are read from each worker's
   branch. Both are read through git.
5. Nothing is `READY FOR MERGE` until both test suites pass and docs are updated. Nothing
   is merged until the manager has reviewed it and reruns both suites on `master`.
6. Watchers poll every 300s and stop on `STOP`, "none", or the human's word.
7. Merged handoffs move to `docs/handover-history.md`. `handover.md` holds only current
   state.
