//! Structured records that agents write into their worktree under `.aura/`.
//! The daemon reads them; agents never talk to each other directly.

use crate::model::Scope;
use serde::{Deserialize, Serialize};

pub const RESULT_FILE: &str = ".aura/result.json";
pub const REVIEW_FILE: &str = ".aura/review.json";
pub const PLAN_FILE: &str = ".aura/plan.json";
pub const BRIEF_FILE: &str = ".aura/brief.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    ReadyForReview,
    Blocked,
    Failed,
}

/// Written by a worker run to `.aura/result.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub status: WorkerStatus,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub questions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
}

/// Written by a reviewer run to `.aura/review.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub verdict: Verdict,
    #[serde(default)]
    pub notes: String,
}

/// Written by a planner run to `.aura/plan.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub tasks: Vec<PlannedTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTask {
    pub title: String,
    pub brief: String,
    pub scope: Scope,
    /// Indices into `Plan::tasks`. Each must point at an earlier task, which
    /// keeps the plan acyclic by construction.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    #[serde(default)]
    pub acceptance: Vec<String>,
}

impl Plan {
    /// Checks the plan before any task is created from it.
    pub fn validate(&self) -> Result<(), String> {
        if self.tasks.is_empty() {
            return Err("plan has no tasks".to_string());
        }
        for (i, task) in self.tasks.iter().enumerate() {
            if task.title.trim().is_empty() {
                return Err(format!("task {i} has no title"));
            }
            if task.scope.write.is_empty() {
                return Err(format!(
                    "task {i} ({}) has an empty write scope",
                    task.title
                ));
            }
            crate::scope::validate_patterns(&task.scope.write)?;
            crate::scope::validate_patterns(&task.scope.read)?;
            for dep in &task.depends_on {
                if *dep >= i {
                    return Err(format!(
                        "task {i} ({}) depends on task {dep}, which is not an earlier task",
                        task.title
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Parses a record from text. Returns a readable error for a missing or
/// malformed file, which the scheduler turns into a failed run.
pub fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("invalid record: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_result_parses() {
        let r: WorkerResult = parse(r#"{"status":"ready_for_review","summary":"done"}"#).unwrap();
        assert_eq!(r.status, WorkerStatus::ReadyForReview);
        assert!(r.questions.is_empty());
    }

    #[test]
    fn unknown_status_is_rejected() {
        assert!(parse::<WorkerResult>(r#"{"status":"maybe"}"#).is_err());
    }

    #[test]
    fn review_verdict_parses_snake_case() {
        let r: ReviewVerdict =
            parse(r#"{"verdict":"request_changes","notes":"rename x"}"#).unwrap();
        assert_eq!(r.verdict, Verdict::RequestChanges);
    }

    fn task(title: &str, deps: Vec<usize>) -> PlannedTask {
        PlannedTask {
            title: title.into(),
            brief: "b".into(),
            scope: Scope {
                write: vec!["src/**".into()],
                read: vec![],
            },
            depends_on: deps,
            acceptance: vec![],
        }
    }

    #[test]
    fn plan_rejects_forward_dependency() {
        let plan = Plan {
            tasks: vec![task("a", vec![1]), task("b", vec![])],
        };
        assert!(plan.validate().unwrap_err().contains("not an earlier task"));
    }

    #[test]
    fn plan_accepts_earlier_dependency() {
        let plan = Plan {
            tasks: vec![task("a", vec![]), task("b", vec![0])],
        };
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn plan_rejects_empty_write_scope() {
        let mut t = task("a", vec![]);
        t.scope.write.clear();
        let plan = Plan { tasks: vec![t] };
        assert!(plan.validate().is_err());
    }
}
