//! Core of the Aura harness: domain types, state machines, scope rules,
//! quota logic and configuration. No I/O beyond reading the config file.

pub mod brief;
pub mod config;
pub mod model;
pub mod quota;
pub mod records;
pub mod scope;
pub mod transition;

pub use config::{Config, Paths};
pub use model::{
    Autonomy, Directive, DirectiveState, Question, QuotaState, Role, Run, RunState, Scope, Task,
    TaskState,
};
pub use quota::{LimitSignal, ProviderQuota};
