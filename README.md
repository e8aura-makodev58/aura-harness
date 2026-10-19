# aura-harness

A way to run several coding agents on one repository in parallel. Each agent gets its own git worktree, they coordinate through a shared `handover.md`, each runs a watcher, and a single manager named by the human merges into the main branch.

Read [aura-harness.md](aura-harness.md) for the full process. It was written for the Compass project, so its paths and rule numbers refer to that repository.

Planned successor: [docs/autonomous-harness-design.md](docs/autonomous-harness-design.md) describes a daemon-driven version where agents run headless, the human gives direction through a local UI or Telegram, and nothing depends on a terminal staying open. Draft, not implemented.
