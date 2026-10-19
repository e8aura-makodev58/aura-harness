//! Legal state changes for tasks and directives. Every change goes through
//! `set_state`, so an illegal move is an error instead of silent corruption.

use crate::model::{Directive, DirectiveState, Task, TaskState};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal {kind} transition {from} -> {to}")]
pub struct TransitionError {
    pub kind: &'static str,
    pub from: String,
    pub to: String,
}

pub fn task_transition_allowed(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;
    matches!(
        (from, to),
        (Proposed, Queued)
            | (Proposed, Cancelled)
            | (Queued, Running)
            | (Queued, Blocked)
            | (Queued, Cancelled)
            | (Running, Review)
            | (Running, Queued)
            | (Running, Blocked)
            | (Running, Cancelled)
            | (Review, Queued)
            | (Review, Gated)
            | (Review, MergeQueued)
            | (Review, Blocked)
            | (Review, Cancelled)
            | (Gated, MergeQueued)
            | (Gated, Cancelled)
            | (MergeQueued, Done)
            | (MergeQueued, Queued)
            | (MergeQueued, Blocked)
            | (MergeQueued, Cancelled)
            | (Blocked, Queued)
            | (Blocked, Cancelled)
    )
}

pub fn directive_transition_allowed(from: DirectiveState, to: DirectiveState) -> bool {
    use DirectiveState::*;
    matches!(
        (from, to),
        (Planning, AwaitingApproval)
            | (Planning, Active)
            | (Planning, Blocked)
            | (Planning, Cancelled)
            | (AwaitingApproval, Active)
            | (AwaitingApproval, Cancelled)
            | (Active, Done)
            | (Active, Blocked)
            | (Active, Cancelled)
            | (Blocked, Active)
            | (Blocked, Cancelled)
    )
}

impl Task {
    pub fn set_state(&mut self, to: TaskState) -> Result<(), TransitionError> {
        if !task_transition_allowed(self.state, to) {
            return Err(TransitionError {
                kind: "task",
                from: self.state.to_string(),
                to: to.to_string(),
            });
        }
        self.state = to;
        Ok(())
    }

    /// A task no longer needs scheduling once it has finished or been cancelled.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, TaskState::Done | TaskState::Cancelled)
    }
}

impl Directive {
    pub fn set_state(&mut self, to: DirectiveState) -> Result<(), TransitionError> {
        if !directive_transition_allowed(self.state, to) {
            return Err(TransitionError {
                kind: "directive",
                from: self.state.to_string(),
                to: to.to_string(),
            });
        }
        self.state = to;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState::*;

    #[test]
    fn happy_path_is_allowed() {
        let path = [Proposed, Queued, Running, Review, MergeQueued, Done];
        for pair in path.windows(2) {
            assert!(task_transition_allowed(pair[0], pair[1]), "{pair:?}");
        }
    }

    #[test]
    fn done_and_cancelled_are_terminal() {
        for to in TaskState::ALL {
            assert!(!task_transition_allowed(Done, *to));
            assert!(!task_transition_allowed(Cancelled, *to));
        }
    }

    #[test]
    fn running_cannot_jump_to_merge() {
        assert!(!task_transition_allowed(Running, MergeQueued));
        assert!(!task_transition_allowed(Queued, Done));
    }
}
