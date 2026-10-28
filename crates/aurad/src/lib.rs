//! The Aura daemon: scheduler, process supervision, merge queue and the local
//! API. `main.rs` only parses arguments and starts the threads.

pub mod api;
pub mod engine;
pub mod merge;
pub mod proc;
pub mod server;
pub mod web;
