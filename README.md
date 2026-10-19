# aura-harness

A way to run several coding agents on one repository in parallel. Each agent gets its own git worktree, they coordinate through a shared `handover.md`, each runs a watcher, and a single manager named by the human merges into the main branch.

Read [aura-harness.md](aura-harness.md) for the full process. It was written for the Compass project, so its paths and rule numbers refer to that repository.
