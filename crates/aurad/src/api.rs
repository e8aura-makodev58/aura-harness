//! Local API. Requests and responses are JSON, one per line, over a Unix socket.
//! Every client (the `aura` CLI, the TUI, a future Telegram bridge) talks only
//! to this interface, so none of them can bypass the scheduler's rules.

use crate::engine::{Engine, ManualTask};
use anyhow::{anyhow, Result};
use aura_core::model::{Autonomy, Scope};
use aura_store::ts;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok { ok: bool, data: Value },
    Err { ok: bool, error: String },
}

impl Response {
    pub fn ok(data: Value) -> Response {
        Response::Ok { ok: true, data }
    }

    pub fn err(message: impl Into<String>) -> Response {
        Response::Err {
            ok: false,
            error: message.into(),
        }
    }
}

/// Runs one request against the engine. Errors become `ok: false` responses.
pub fn handle(engine: &mut Engine, request: Request) -> Response {
    match dispatch(engine, &request.cmd, &request.args) {
        Ok(data) => Response::ok(data),
        Err(err) => Response::err(format!("{err:#}")),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing argument {key:?}"))
}

fn arg_opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn dispatch(engine: &mut Engine, cmd: &str, args: &Value) -> Result<Value> {
    match cmd {
        "ping" => Ok(json!({ "pong": true })),
        "status" => status(engine),
        "directive.create" => {
            let text = arg_str(args, "text")?;
            let autonomy = arg_opt_str(args, "autonomy")
                .map(|a| a.parse::<Autonomy>())
                .transpose()
                .map_err(|e| anyhow!(e))?;
            Ok(serde_json::to_value(
                engine.create_directive(text, autonomy)?,
            )?)
        }
        "directive.list" => Ok(serde_json::to_value(engine.store.list_directives()?)?),
        "task.list" => {
            let tasks = engine.store.list_tasks(arg_opt_str(args, "directive"))?;
            Ok(serde_json::to_value(tasks)?)
        }
        "task.show" => {
            let id = arg_str(args, "id")?;
            let task = engine
                .store
                .get_task(id)?
                .ok_or_else(|| anyhow!("no task {id}"))?;
            Ok(json!({
                "task": task,
                "runs": engine.store.list_runs(Some(id), 20)?,
                "questions": engine.store.questions_for(id)?,
            }))
        }
        "task.add" => {
            let task = engine.add_task(ManualTask {
                title: arg_str(args, "title")?.to_string(),
                brief: arg_opt_str(args, "brief").unwrap_or_default().to_string(),
                scope: Scope {
                    write: arg_strings(args, "write"),
                    read: arg_strings(args, "read"),
                },
                depends_on: arg_strings(args, "depends_on"),
                acceptance: arg_strings(args, "accept"),
            })?;
            Ok(serde_json::to_value(task)?)
        }
        "approve" => Ok(json!(engine.approve(arg_str(args, "id")?)?)),
        "cancel" => Ok(json!(engine.cancel(arg_str(args, "id")?)?)),
        "answer" => Ok(json!(
            engine.answer(arg_str(args, "question")?, arg_str(args, "text")?)?
        )),
        "pause" => {
            engine.set_paused(true)?;
            Ok(json!("paused"))
        }
        "resume" => {
            engine.set_paused(false)?;
            Ok(json!("resumed"))
        }
        "kill" => {
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                anyhow::bail!("kill needs {{\"confirm\": true}}");
            }
            let stopped = engine.kill_all()?;
            Ok(json!({ "stopped_runs": stopped, "paused": true }))
        }
        "logs" => {
            let run_id = arg_str(args, "run")?;
            let run = engine
                .store
                .get_run(run_id)?
                .ok_or_else(|| anyhow!("no run {run_id}"))?;
            let tail = args.get("tail").and_then(Value::as_u64).unwrap_or(200) as usize;
            let text = std::fs::read_to_string(&run.log_path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(tail);
            Ok(json!({ "run": run.id, "lines": lines[start..] }))
        }
        "runs" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30) as usize;
            Ok(serde_json::to_value(
                engine.store.list_runs(arg_opt_str(args, "task"), limit)?,
            )?)
        }
        "events" => {
            let since = args.get("since").and_then(Value::as_i64).unwrap_or(0);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
            Ok(serde_json::to_value(
                engine.store.events_since(since, limit)?,
            )?)
        }
        "quota" => Ok(Value::Array(engine.quota_views(Utc::now())?)),
        other => Err(anyhow!("unknown command {other:?}")),
    }
}

fn status(engine: &mut Engine) -> Result<Value> {
    let now = Utc::now();
    let directives = engine.store.list_directives()?;
    let tasks = engine.store.list_tasks(None)?;
    let mut counts = serde_json::Map::new();
    for task in &tasks {
        let key = task.state.as_str();
        let n = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key.to_string(), json!(n + 1));
    }
    Ok(json!({
        "time": ts(now),
        "paused": engine.is_paused()?,
        "repo": engine.repo,
        "main_branch": engine.cfg.repo.main_branch,
        "directives": directives,
        "task_counts": counts,
        "tasks": tasks,
        "active_runs": engine.active_runs(),
        "merge_in_progress": engine.merge_in_progress(),
        "quotas": engine.quota_views(now)?,
        "next_quota_wake": engine.next_quota_wake(now)?.map(ts),
        "open_questions": engine.store.open_questions()?,
    }))
}
