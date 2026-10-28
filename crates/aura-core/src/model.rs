//! Domain types shared by every crate: directives, tasks, runs, questions and
//! the enums that drive their state machines.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Declares a string-backed enum. The text form is what is stored in SQLite,
/// sent over the API and printed by the CLI.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err(format!("unknown {}: {other}", stringify!($name))),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = String::deserialize(d)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

string_enum!(
    /// How much the human must approve before work moves on.
    Autonomy {
        AskEach => "ask-each",
        AskPlan => "ask-plan",
        Auto => "auto",
    }
);

string_enum!(
    DirectiveState {
        Planning => "planning",
        AwaitingApproval => "awaiting-approval",
        Active => "active",
        Done => "done",
        Blocked => "blocked",
        Cancelled => "cancelled",
    }
);

string_enum!(
    TaskState {
        Proposed => "proposed",
        Queued => "queued",
        Running => "running",
        Review => "review",
        Gated => "gated",
        MergeQueued => "merge-queued",
        Done => "done",
        Blocked => "blocked",
        Cancelled => "cancelled",
    }
);

string_enum!(
    Role {
        Planner => "planner",
        Worker => "worker",
        Reviewer => "reviewer",
    }
);

string_enum!(
    RunState {
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
        Limited => "limited",
        TimedOut => "timed-out",
        Interrupted => "interrupted",
        Killed => "killed",
    }
);

string_enum!(
    /// Quota state as stored. `Available` is also the effective state of a
    /// limited provider once its retry time has passed (see `quota::effective`).
    QuotaState {
        Available => "available",
        Limited => "limited",
        EstimatedLimited => "estimated-limited",
        Unknown => "unknown",
    }
);

/// Write and read scope of a task, as glob patterns relative to the repo root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directive {
    pub id: String,
    pub text: String,
    pub autonomy: Autonomy,
    pub state: DirectiveState,
    pub planner_attempts: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub directive_id: Option<String>,
    pub title: String,
    pub brief: String,
    pub scope: Scope,
    pub depends_on: Vec<String>,
    /// Commands the integrator runs on the merge result. Each entry is a
    /// whitespace-split argv whose program must be on the allowlist.
    pub acceptance: Vec<String>,
    pub state: TaskState,
    pub autonomy: Autonomy,
    pub branch: String,
    pub worktree: Option<String>,
    /// Provider that produced the latest worker run. The reviewer must differ.
    pub worker_provider: Option<String>,
    pub runs_used: u32,
    pub max_runs: u32,
    /// Feedback for the next worker run: review notes, merge failures, answers.
    pub notes: Vec<String>,
    /// Monotonic position used to order queued work first-in first-out.
    pub queue_seq: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: Option<String>,
    pub directive_id: Option<String>,
    pub role: Role,
    pub provider: String,
    pub state: RunState,
    pub pid: Option<u32>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub task_id: String,
    pub text: String,
    pub answer: Option<String>,
    pub created_at: String,
    pub answered_at: Option<String>,
}
