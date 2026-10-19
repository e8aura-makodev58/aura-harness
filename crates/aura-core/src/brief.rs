//! Text handed to agents. Each run gets its brief as a file in its worktree
//! (`.aura/brief.md`), so the CLI prompt stays short and the full context is
//! inspectable after the fact.

use crate::model::Task;
use crate::records::{PLAN_FILE, RESULT_FILE, REVIEW_FILE};

/// The short prompt every adapter passes to its CLI.
pub const PROMPT: &str = "Read .aura/brief.md and carry out exactly what it describes. \
Write the record it asks for before you finish.";

pub fn worker(task: &Task, main_branch: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Task {}: {}\n\n", task.id, task.title));
    out.push_str("You are a worker. Work only in this worktree, on this branch.\n\n");
    out.push_str("## Brief\n\n");
    out.push_str(task.brief.trim());
    out.push_str("\n\n## Scope\n\n");
    out.push_str("You may change files matching:\n");
    for p in &task.scope.write {
        out.push_str(&format!("- `{p}`\n"));
    }
    if !task.scope.read.is_empty() {
        out.push_str("\nYou may read, but not change:\n");
        for p in &task.scope.read {
            out.push_str(&format!("- `{p}`\n"));
        }
    }
    out.push_str("\nChanges outside the write scope fail the run.\n");
    out.push_str(&format!(
        "\n## Base\n\nYour branch `{}` started from `{main_branch}`. Commit your work on this branch.\n",
        task.branch
    ));
    if !task.acceptance.is_empty() {
        out.push_str("\n## Checks\n\nThe integrator runs these on the merge result. Run them yourself first:\n");
        for c in &task.acceptance {
            out.push_str(&format!("- `{c}`\n"));
        }
    }
    if !task.notes.is_empty() {
        out.push_str("\n## Notes from earlier runs\n\n");
        for (i, note) in task.notes.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, note.trim()));
        }
    }
    out.push_str(&format!(
        "\n## Result record\n\nWhen you finish, write `{RESULT_FILE}` as JSON:\n\n\
```json\n{{\"status\": \"ready_for_review\" | \"blocked\" | \"failed\", \"summary\": \"...\", \"questions\": []}}\n```\n\n\
Use `blocked` and list questions only when you need a decision a human must make. \
Do not commit `.aura/`.\n"
    ));
    out
}

pub fn review(task: &Task, main_branch: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Review of {} ({})\n\n", task.id, task.title));
    out.push_str(&format!(
        "You are a reviewer. This worktree is the head of `{}`, to be merged into `{main_branch}`. \
Do not change any file except `{REVIEW_FILE}`.\n\n",
        task.branch
    ));
    out.push_str("## Brief the worker received\n\n");
    out.push_str(task.brief.trim());
    out.push_str(&format!(
        "\n\n## Scope\n\nAllowed to change: {}\n",
        task.scope.write.join(", ")
    ));
    out.push_str("\n## Check\n\nCompare the branch diff with `git diff ");
    out.push_str(main_branch);
    out.push_str("...HEAD`. Look for bugs, scope violations and missing tests.\n\n");
    out.push_str(&format!(
        "## Verdict record\n\nWrite `{REVIEW_FILE}` as JSON:\n\n\
```json\n{{\"verdict\": \"approve\" | \"request_changes\", \"notes\": \"...\"}}\n```\n"
    ));
    out
}

pub fn planner(directive_text: &str, main_branch: &str, allowed_checks: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# Planning\n\n");
    out.push_str(&format!(
        "You are the planner for the repository at `{main_branch}`. Turn the directive below \
into a small set of tasks. Do not change any tracked file.\n\n"
    ));
    out.push_str("## Directive\n\n");
    out.push_str(directive_text.trim());
    out.push_str(&format!(
        "\n\n## Rules\n\n\
- Each task names the files it may change in `scope.write`. Keep these disjoint when tasks can run in parallel.\n\
- `depends_on` lists indices of earlier tasks in the same plan.\n\
- Acceptance commands must start with one of: {}. No shell syntax.\n\
- Keep briefs concrete: what to build, why, what not to touch.\n\n",
        allowed_checks.join(", ")
    ));
    out.push_str(&format!(
        "## Plan record\n\nWrite `{PLAN_FILE}` as JSON:\n\n\
```json\n{{\"tasks\": [{{\"title\": \"...\", \"brief\": \"...\", \"scope\": {{\"write\": [\"src/**\"], \"read\": []}}, \"depends_on\": [], \"acceptance\": [\"cargo test\"]}}]}}\n```\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Autonomy, Scope, TaskState};

    #[test]
    fn worker_brief_contains_scope_notes_and_contract() {
        let task = Task {
            id: "T-0001".into(),
            directive_id: None,
            title: "Add retry".into(),
            brief: "Retry uploads.".into(),
            scope: Scope {
                write: vec!["src/upload/**".into()],
                read: vec![],
            },
            depends_on: vec![],
            acceptance: vec!["cargo test".into()],
            state: TaskState::Queued,
            autonomy: Autonomy::Auto,
            branch: "aura/T-0001".into(),
            worktree: None,
            worker_provider: None,
            runs_used: 0,
            max_runs: 3,
            notes: vec!["merge conflict in src/lib.rs".into()],
            queue_seq: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let text = worker(&task, "main");
        assert!(text.contains("`src/upload/**`"));
        assert!(text.contains("merge conflict in src/lib.rs"));
        assert!(text.contains(RESULT_FILE));
        assert!(text.contains("`cargo test`"));
    }
}
