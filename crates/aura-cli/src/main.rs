//! `aura`: command-line client for the Aura daemon. Every command is a request
//! to `aurad`'s local API, so the CLI, the dashboard and any future client
//! share one set of rules. Closing a client never affects running work.

mod client;
mod tui;

use anyhow::{bail, Context, Result};
use aura_core::Config;
use clap::{Parser, Subcommand};
use client::Client;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aura",
    version,
    about = "Run coding agents on one repository, unattended"
)]
struct Cli {
    /// Configuration file. Defaults to $AURA_CONFIG or ~/.config/aura/aura.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print raw JSON instead of a summary.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show directives, tasks, active runs and provider quota.
    Status,
    /// Give the planner a goal.
    Directive {
        /// The goal, in plain words.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// ask-each, ask-plan or auto.
        #[arg(long)]
        autonomy: Option<String>,
    },
    /// List directives.
    Directives,
    /// List tasks, optionally for one directive.
    Tasks {
        #[arg(long)]
        directive: Option<String>,
    },
    #[command(subcommand)]
    Task(TaskCommand),
    /// Approve a plan (D-…), a proposed task (T-…), a gated merge (T-…), or unblock a task.
    Approve { id: String },
    /// Cancel a task and stop its run. The branch is kept.
    Cancel { id: String },
    /// Answer a question a worker asked.
    Answer {
        question: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Stop starting new runs. Running runs finish.
    Pause,
    /// Start new runs again.
    Resume,
    /// Stop every run now and pause. Requires --yes.
    Kill {
        #[arg(long)]
        yes: bool,
    },
    /// Show the end of a run's log.
    Logs {
        run: String,
        #[arg(long, default_value_t = 200)]
        tail: u64,
    },
    /// List recent runs.
    Runs {
        #[arg(long)]
        task: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: u64,
    },
    /// Show the event log. With --follow, keep printing new events.
    Events {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long)]
        follow: bool,
    },
    /// Show provider quota and when each provider can run again.
    Quota,
    /// Open the live dashboard. Quit with q; the daemon keeps running.
    Watch,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Show one task with its runs and questions.
    Show { id: String },
    /// Add a task by hand, without the planner.
    Add {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        brief: String,
        /// Glob the task may change. Repeat for more.
        #[arg(long = "write", required = true)]
        write: Vec<String>,
        /// Glob the task may read.
        #[arg(long = "read")]
        read: Vec<String>,
        /// Acceptance command, run without a shell. Repeat for more.
        #[arg(long = "accept")]
        accept: Vec<String>,
        /// Task that must finish first. Repeat for more.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
    },
}

fn main() -> Result<()> {
    // Let `aura … | head` end quietly instead of panicking on a closed pipe.
    // SAFETY: resetting a signal disposition to its default has no other effect.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("loading configuration")?;
    let client = Client::new(config.paths().socket);

    match cli.command {
        Command::Watch => return tui::run(&client),
        Command::Events { since, follow } => return events(&client, since, follow, cli.json),
        _ => {}
    }

    let (method, args) = request_for(&cli.command)?;
    let data = client.call(method, args)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }
    print_human(&cli.command, &data)
}

/// Maps a command to an API method and arguments.
fn request_for(command: &Command) -> Result<(&'static str, Value)> {
    Ok(match command {
        Command::Status => ("status", json!({})),
        Command::Directive { text, autonomy } => (
            "directive.create",
            json!({ "text": text.join(" "), "autonomy": autonomy }),
        ),
        Command::Directives => ("directive.list", json!({})),
        Command::Tasks { directive } => ("task.list", json!({ "directive": directive })),
        Command::Task(TaskCommand::Show { id }) => ("task.show", json!({ "id": id })),
        Command::Task(TaskCommand::Add {
            title,
            brief,
            write,
            read,
            accept,
            depends_on,
        }) => (
            "task.add",
            json!({
                "title": title,
                "brief": brief,
                "write": write,
                "read": read,
                "accept": accept,
                "depends_on": depends_on,
            }),
        ),
        Command::Approve { id } => ("approve", json!({ "id": id })),
        Command::Cancel { id } => ("cancel", json!({ "id": id })),
        Command::Answer { question, text } => (
            "answer",
            json!({ "question": question, "text": text.join(" ") }),
        ),
        Command::Pause => ("pause", json!({})),
        Command::Resume => ("resume", json!({})),
        Command::Kill { yes } => {
            if !yes {
                bail!("kill stops every run; repeat with --yes to confirm");
            }
            ("kill", json!({ "confirm": true }))
        }
        Command::Logs { run, tail } => ("logs", json!({ "run": run, "tail": tail })),
        Command::Runs { task, limit } => ("runs", json!({ "task": task, "limit": limit })),
        Command::Quota => ("quota", json!({})),
        Command::Events { .. } | Command::Watch => bail!("handled before dispatch"),
    })
}

fn print_human(command: &Command, data: &Value) -> Result<()> {
    match command {
        Command::Status => print_status(data),
        Command::Directive { .. } => {
            println!(
                "{} created (autonomy {}, planning)",
                text(&data["id"]),
                text(&data["autonomy"])
            );
            Ok(())
        }
        Command::Directives => {
            for d in as_list(data) {
                println!(
                    "{}  {:<18} {:<8} {}",
                    text(&d["id"]),
                    text(&d["state"]),
                    text(&d["autonomy"]),
                    text(&d["text"])
                );
            }
            Ok(())
        }
        Command::Tasks { .. } => {
            print_tasks(as_list(data));
            Ok(())
        }
        Command::Task(TaskCommand::Show { .. }) => print_task_detail(data),
        Command::Task(TaskCommand::Add { .. }) => {
            println!("{} created ({})", text(&data["id"]), text(&data["state"]));
            Ok(())
        }
        Command::Runs { .. } => {
            for r in as_list(data) {
                println!(
                    "{}  {:<6} {:<10} {:<12} {:<12} {}",
                    text(&r["id"]),
                    text(&r["role"]),
                    text(&r["state"]),
                    text(&r["provider"]),
                    text(&r["task_id"]),
                    text(&r["reason"])
                );
            }
            Ok(())
        }
        Command::Logs { .. } => {
            for line in as_list(&data["lines"]) {
                println!("{}", line.as_str().unwrap_or_default());
            }
            Ok(())
        }
        Command::Quota => {
            print_quota(as_list(data));
            Ok(())
        }
        Command::Kill { .. } => {
            println!("stopped {} run(s); scheduler paused", data["stopped_runs"]);
            Ok(())
        }
        _ => {
            println!(
                "{}",
                data.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| data.to_string())
            );
            Ok(())
        }
    }
}

fn events(client: &Client, mut cursor: i64, follow: bool, json_out: bool) -> Result<()> {
    loop {
        let batch = client.call("events", json!({ "since": cursor, "limit": 500 }))?;
        for event in as_list(&batch) {
            cursor = event["seq"].as_i64().unwrap_or(cursor);
            if json_out {
                println!("{event}");
            } else {
                println!(
                    "{} {:<20} {:<12} {}",
                    text(&event["ts"]),
                    text(&event["kind"]),
                    text(&event["subject"]),
                    event["payload"]
                );
            }
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub fn print_status(data: &Value) -> Result<()> {
    let paused = data["paused"].as_bool().unwrap_or(false);
    println!(
        "aura: {} on {} {}",
        if paused { "PAUSED" } else { "running" },
        text(&data["repo"]),
        text(&data["main_branch"])
    );
    if let Some(wake) = data["next_quota_wake"].as_str() {
        println!("next quota refresh: {wake}");
    }
    if let Some(counts) = data["task_counts"].as_object() {
        let parts: Vec<String> = counts.iter().map(|(k, v)| format!("{k} {v}")).collect();
        println!(
            "tasks: {}",
            if parts.is_empty() {
                "none".into()
            } else {
                parts.join(", ")
            }
        );
    }
    println!("\nQuota:");
    print_quota(as_list(&data["quotas"]));
    let runs = as_list(&data["active_runs"]);
    println!("\nActive runs: {}", runs.len());
    for r in runs {
        println!(
            "  {}  {:<8} {:<12} task {}",
            text(&r["id"]),
            text(&r["role"]),
            text(&r["provider"]),
            text(&r["task_id"])
        );
    }
    if let Some(task) = data["merge_in_progress"].as_str() {
        println!("Merging: {task}");
    }
    let questions = as_list(&data["open_questions"]);
    if !questions.is_empty() {
        println!("\nWaiting for an answer:");
        for q in questions {
            println!(
                "  {}  ({}) {}",
                text(&q["id"]),
                text(&q["task_id"]),
                text(&q["text"])
            );
        }
    }
    println!("\nDirectives:");
    for d in as_list(&data["directives"]) {
        println!(
            "  {}  {:<18} {}",
            text(&d["id"]),
            text(&d["state"]),
            text(&d["text"])
        );
    }
    Ok(())
}

fn print_tasks(tasks: &[Value]) {
    for t in tasks {
        println!(
            "{}  {:<13} {:<7} {}",
            text(&t["id"]),
            text(&t["state"]),
            format!("{}/{}", t["runs_used"], t["max_runs"]),
            text(&t["title"])
        );
    }
}

fn print_task_detail(data: &Value) -> Result<()> {
    let task = &data["task"];
    println!(
        "{}  {}  [{}]",
        text(&task["id"]),
        text(&task["title"]),
        text(&task["state"])
    );
    println!("branch: {}", text(&task["branch"]));
    let scope: Vec<&str> = as_list(&task["scope"]["write"])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    println!("scope: {}", scope.join(", "));
    println!("\n{}", text(&task["brief"]));
    if let Some(notes) = task["notes"].as_array().filter(|n| !n.is_empty()) {
        println!("\nNotes:");
        for n in notes {
            println!("  - {}", text(n));
        }
    }
    println!("\nRuns:");
    for r in as_list(&data["runs"]) {
        println!(
            "  {}  {:<6} {:<10} {}",
            text(&r["id"]),
            text(&r["role"]),
            text(&r["state"]),
            text(&r["reason"])
        );
    }
    for q in as_list(&data["questions"]) {
        println!("\nQuestion {}: {}", text(&q["id"]), text(&q["text"]));
        match q["answer"].as_str() {
            Some(a) => println!("  answer: {a}"),
            None => println!("  unanswered: aura answer {} <text>", text(&q["id"])),
        }
    }
    Ok(())
}

pub fn print_quota(quotas: &[Value]) {
    for q in quotas {
        let state = text(&q["state"]);
        let when = match q["retry_at"].as_str() {
            Some(t) if state != "available" => format!("retry at {t} ({})", text(&q["source"])),
            _ => String::new(),
        };
        println!(
            "  {:<12} {:<18} running {}  {}",
            text(&q["provider"]),
            state,
            text(&q["running"]),
            when
        );
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn as_list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
