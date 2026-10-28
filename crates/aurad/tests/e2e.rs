//! End-to-end tests. A real temporary git repository, the real engine and the
//! real git operations run. The agent CLIs are replaced by a shell script that
//! writes the same records a real agent would, and that can be told to hit a
//! subscription limit, hang, or misbehave.

use aura_core::config::{Acceptance, Config, ProviderConfig};
use aura_core::model::{Autonomy, DirectiveState, RunState, Scope, TaskState};
use aura_core::Role;
use aurad::engine::Engine;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FAKE_AGENT: &str = r#"#!/bin/sh
# Stand-in for an agent CLI. It reads the same role variables the daemon sets
# and writes the same records a real agent is asked to write.
set -u
case "$AURA_ROLE" in
  planner)
    mkdir -p .aura
    if [ -n "${FAKE_BAD_PLAN:-}" ]; then
      printf '%s' '{"tasks":[{"title":"x","brief":"b","scope":{"write":["hello/**"]},"acceptance":["rm -rf /"]}]}' > .aura/plan.json
    else
      printf '%s' '{"tasks":[{"title":"Add hello file","brief":"Create hello/<task>.txt","scope":{"write":["hello/**"]},"acceptance":["true"]}]}' > .aura/plan.json
    fi
    ;;
  worker)
    if [ -n "${FAKE_LIMIT_MARKER:-}" ] && [ ! -f "$FAKE_LIMIT_MARKER" ]; then
      : > "$FAKE_LIMIT_MARKER"
      echo "Rate limit reached. Try again in 2 seconds."
      exit 1
    fi
    if [ -n "${FAKE_SLEEP_MARKER:-}" ] && [ ! -f "$FAKE_SLEEP_MARKER" ]; then
      : > "$FAKE_SLEEP_MARKER"
      sleep 60
    fi
    if [ -n "${FAKE_ASK:-}" ] && ! grep -q "Answer:" .aura/brief.md 2>/dev/null; then
      printf '%s' '{"status":"blocked","summary":"need a decision","questions":["Which name?"]}' > .aura/result.json
      exit 0
    fi
    mkdir -p .aura hello
    echo "work for $AURA_TASK" > "hello/$AURA_TASK.txt"
    if [ -n "${FAKE_OUT_OF_SCOPE:-}" ]; then
      mkdir -p src && echo "not allowed" > src/evil.txt
    fi
    git add -A
    git commit -q -m "work $AURA_TASK"
    printf '%s' '{"status":"ready_for_review","summary":"added file"}' > .aura/result.json
    ;;
  reviewer)
    mkdir -p .aura
    printf '%s' '{"verdict":"approve","notes":"looks fine"}' > .aura/review.json
    ;;
  *)
    exit 2
    ;;
esac
"#;

struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    agent: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        let bin = root.path().join("bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&bin).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("README.md"), "fixture\n").unwrap();
        run_git(&repo, &["add", "-A"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let agent = bin.join("fake-agent.sh");
        fs::write(&agent, FAKE_AGENT).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        Fixture {
            _root: root,
            repo,
            state,
            agent,
        }
    }

    /// Two providers so that the reviewer differs from the worker.
    fn config(&self, knobs: &[(&str, &str)]) -> Config {
        let mut env: std::collections::BTreeMap<String, String> = [
            ("GIT_AUTHOR_NAME", "aura"),
            ("GIT_AUTHOR_EMAIL", "aura@localhost"),
            ("GIT_COMMITTER_NAME", "aura"),
            ("GIT_COMMITTER_EMAIL", "aura@localhost"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in knobs {
            env.insert(k.to_string(), v.to_string());
        }
        let provider = |env: &std::collections::BTreeMap<String, String>| ProviderConfig {
            command: self.agent.to_string_lossy().into_owned(),
            args: vec!["{prompt}".into()],
            max_parallel: 2,
            env: env.clone(),
            ..ProviderConfig::default()
        };
        let mut cfg = Config::default();
        cfg.repo.path = self.repo.clone();
        cfg.repo.main_branch = "main".into();
        cfg.state_dir = Some(self.state.clone());
        cfg.providers.insert("fake-a".into(), provider(&env));
        cfg.providers.insert("fake-b".into(), provider(&env));
        cfg.roles.planner = vec!["fake-a".into()];
        cfg.roles.worker = vec!["fake-a".into()];
        cfg.roles.reviewer = vec!["fake-b".into()];
        cfg.defaults.autonomy = Autonomy::Auto;
        cfg.budgets.per_task_runs = 2;
        cfg.acceptance = Acceptance {
            allowed_programs: vec!["true".into(), "false".into()],
            default_checks: vec![],
        };
        cfg
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// Ticks the engine until `done` holds, or fails with a dump of what happened.
fn drive(engine: &mut Engine, limit: Duration, what: &str, done: impl Fn(&Engine) -> bool) {
    let start = Instant::now();
    loop {
        engine.tick().unwrap();
        if done(engine) {
            return;
        }
        if start.elapsed() > limit {
            let directives = engine.store.list_directives().unwrap();
            let tasks = engine.store.list_tasks(None).unwrap();
            let runs = engine.store.list_runs(None, 20).unwrap();
            panic!(
                "timed out waiting for {what}\ndirectives: {directives:#?}\ntasks: {tasks:#?}\nruns: {runs:#?}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn roles_of(engine: &Engine) -> Vec<(Role, RunState)> {
    engine
        .store
        .list_runs(None, 100)
        .unwrap()
        .into_iter()
        .map(|r| (r.role, r.state))
        .collect()
}

#[test]
fn directive_flows_from_plan_to_main() {
    let fx = Fixture::new();
    let mut engine = Engine::open(fx.config(&[])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(
        &mut engine,
        Duration::from_secs(120),
        "directive done",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Done,
    );

    let tasks = engine.store.list_tasks(Some(&directive.id)).unwrap();
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.state, TaskState::Done);
    assert!(
        fx.repo
            .join("hello")
            .join(format!("{}.txt", task.id))
            .exists(),
        "file landed on main"
    );

    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args(["log", "--oneline", "main"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout).to_string();
    assert!(
        log.contains(&format!("Merge aura/{}", task.id)),
        "merge commit on main:\n{log}"
    );

    let roles = roles_of(&engine);
    assert!(roles.contains(&(Role::Planner, RunState::Succeeded)));
    assert!(roles.contains(&(Role::Worker, RunState::Succeeded)));
    assert!(roles.contains(&(Role::Reviewer, RunState::Succeeded)));
    assert!(engine
        .store
        .events_since(0, 1000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "task.merged"));

    // The harness branch mirrors task state and the event log, and does not touch main.
    engine.sync_mirror().unwrap();
    let git_show = |spec: &str| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&fx.repo)
            .args(["show", spec])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let mirrored = git_show(&format!("harness:tasks/{}.md", task.id));
    assert!(
        mirrored.contains("- state: done"),
        "mirror shows the task as done:\n{mirrored}"
    );
    assert!(git_show("harness:events.jsonl").contains("task.merged"));
    assert!(
        !fx.repo.join("tasks").exists(),
        "mirror never writes into main"
    );
}

#[test]
fn subscription_limit_pauses_provider_and_resumes() {
    let fx = Fixture::new();
    let marker = fx._root.path().join("limit-hit");
    let knob = marker.to_string_lossy().into_owned();
    let mut engine = Engine::open(fx.config(&[("FAKE_LIMIT_MARKER", knob.as_str())])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(
        &mut engine,
        Duration::from_secs(120),
        "directive done after limit",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Done,
    );

    let runs = engine.store.list_runs(None, 100).unwrap();
    let limited: Vec<_> = runs
        .iter()
        .filter(|r| r.state == RunState::Limited)
        .collect();
    assert_eq!(limited.len(), 1, "exactly one refused run");
    assert_eq!(limited[0].role, Role::Worker);
    assert!(limited[0]
        .reason
        .as_deref()
        .unwrap_or("")
        .contains("Rate limit"));

    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    assert_eq!(
        task.runs_used, 1,
        "the refused run did not count against the budget"
    );
    // The refusal was recorded with the reset time the agent quoted, and the
    // provider was marked available again once that time passed.
    let events = engine.store.events_since(0, 1000).unwrap();
    let limited = events
        .iter()
        .position(|e| e.kind == "quota.limited")
        .expect("quota.limited event");
    assert_eq!(events[limited].payload["source"], "message");
    assert!(
        events[limited + 1..]
            .iter()
            .any(|e| e.kind == "quota.available"),
        "recovery recorded"
    );
}

#[test]
fn restart_mid_run_recovers_without_losing_the_task() {
    let fx = Fixture::new();
    let marker = fx._root.path().join("slept");
    let knob = marker.to_string_lossy().into_owned();
    let directive_id;
    {
        let mut first = Engine::open(fx.config(&[("FAKE_SLEEP_MARKER", knob.as_str())])).unwrap();
        directive_id = first.create_directive("Add a hello file", None).unwrap().id;
        drive(&mut first, Duration::from_secs(60), "worker running", |e| {
            e.store.tasks_in_state(TaskState::Running).unwrap().len() == 1
        });
        // The daemon "dies" here: the engine is dropped and its child keeps running.
    }

    let mut second = Engine::open(fx.config(&[("FAKE_SLEEP_MARKER", knob.as_str())])).unwrap();
    let interrupted = second.store.runs_in_state(RunState::Interrupted).unwrap();
    assert_eq!(
        interrupted.len(),
        1,
        "the orphaned run is recorded as interrupted"
    );

    drive(
        &mut second,
        Duration::from_secs(120),
        "directive done after restart",
        |e| e.store.get_directive(&directive_id).unwrap().unwrap().state == DirectiveState::Done,
    );
    let task = second
        .store
        .list_tasks(Some(&directive_id))
        .unwrap()
        .remove(0);
    assert_eq!(task.state, TaskState::Done);
    assert_eq!(task.runs_used, 1, "the interrupted run was refunded");
}

#[test]
fn ask_plan_waits_for_approval_before_work() {
    let fx = Fixture::new();
    let mut cfg = fx.config(&[]);
    cfg.defaults.autonomy = Autonomy::AskPlan;
    let mut engine = Engine::open(cfg).unwrap();
    let directive = engine
        .create_directive("Add a hello file", Some(Autonomy::AskPlan))
        .unwrap();

    drive(
        &mut engine,
        Duration::from_secs(60),
        "plan awaiting approval",
        |e| {
            e.store.get_directive(&directive.id).unwrap().unwrap().state
                == DirectiveState::AwaitingApproval
        },
    );
    for _ in 0..5 {
        engine.tick().unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    assert_eq!(
        task.state,
        TaskState::Proposed,
        "nothing runs before approval"
    );
    assert!(!roles_of(&engine).iter().any(|(r, _)| *r == Role::Worker));

    engine.approve(&directive.id).unwrap();
    drive(
        &mut engine,
        Duration::from_secs(120),
        "directive done after approval",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Done,
    );
}

#[test]
fn out_of_scope_changes_never_reach_main() {
    let fx = Fixture::new();
    let mut engine = Engine::open(fx.config(&[("FAKE_OUT_OF_SCOPE", "1")])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(&mut engine, Duration::from_secs(120), "task blocked", |e| {
        e.store
            .list_tasks(Some(&directive.id))
            .unwrap()
            .iter()
            .any(|t| t.state == TaskState::Blocked)
    });
    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    assert!(
        task.notes
            .iter()
            .any(|n| n.contains("outside the write scope")),
        "scope violation recorded: {:?}",
        task.notes
    );
    assert!(
        !fx.repo.join("src").exists(),
        "out-of-scope file never landed on main"
    );
}

#[test]
fn plan_with_disallowed_acceptance_command_is_rejected() {
    let fx = Fixture::new();
    let mut engine = Engine::open(fx.config(&[("FAKE_BAD_PLAN", "1")])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(
        &mut engine,
        Duration::from_secs(60),
        "directive blocked",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Blocked,
    );
    assert!(
        engine.store.list_tasks(None).unwrap().is_empty(),
        "a bad plan creates no tasks"
    );
}

#[test]
fn failing_acceptance_check_sends_task_back_to_worker() {
    let fx = Fixture::new();
    let mut cfg = fx.config(&[]);
    // The planner's check is `true`, so make the integrator's check fail by
    // adding a default check that always fails.
    cfg.acceptance.default_checks = vec!["false".into()];
    let mut engine = Engine::open(cfg).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(
        &mut engine,
        Duration::from_secs(120),
        "task blocked after failing checks",
        |e| {
            e.store
                .list_tasks(Some(&directive.id))
                .unwrap()
                .iter()
                .any(|t| t.state == TaskState::Blocked)
        },
    );
    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    assert!(task
        .notes
        .iter()
        .any(|n| n.contains("Checks failed on the merge result")));
    assert_eq!(
        task.runs_used, 2,
        "each failed merge check used a worker run"
    );
    assert!(!fx.repo.join("hello").exists(), "nothing landed on main");
    assert_eq!(
        task.scope,
        Scope {
            write: vec!["hello/**".into()],
            read: vec![]
        }
    );
}

#[test]
fn blocked_worker_resumes_after_human_answer() {
    let fx = Fixture::new();
    let mut engine = Engine::open(fx.config(&[("FAKE_ASK", "1")])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();

    drive(
        &mut engine,
        Duration::from_secs(60),
        "task blocked on a question",
        |e| e.store.open_questions().unwrap().len() == 1,
    );
    let question = engine.store.open_questions().unwrap().remove(0);
    assert_eq!(question.text, "Which name?");
    let task_id = question.task_id.clone();
    assert_eq!(
        engine.store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );

    engine.answer(&question.id, "hello").unwrap();
    drive(
        &mut engine,
        Duration::from_secs(120),
        "directive done after answer",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Done,
    );

    let task = engine.store.get_task(&task_id).unwrap().unwrap();
    assert!(
        task.notes.iter().any(|n| n.contains("Answer: hello")),
        "answer reached the next brief"
    );
    assert_eq!(
        task.runs_used, 2,
        "the question run and the answered run both counted"
    );
    let answered = engine.store.questions_for(&task_id).unwrap();
    assert_eq!(answered[0].answer.as_deref(), Some("hello"));
}

#[test]
fn rejected_worker_run_is_recorded_as_failed() {
    let fx = Fixture::new();
    let mut engine = Engine::open(fx.config(&[("FAKE_OUT_OF_SCOPE", "1")])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();
    drive(&mut engine, Duration::from_secs(120), "task blocked", |e| {
        e.store
            .list_tasks(Some(&directive.id))
            .unwrap()
            .iter()
            .any(|t| t.state == TaskState::Blocked)
    });
    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    let runs = engine.store.list_runs(Some(&task.id), 10).unwrap();
    assert!(
        runs.iter().all(|r| r.state == RunState::Failed),
        "every rejected run is failed: {runs:#?}"
    );
    assert!(runs
        .iter()
        .all(|r| r.reason.as_deref().unwrap_or("").contains("write scope")));
}

#[test]
fn kill_switch_returns_running_work_to_the_queue() {
    let fx = Fixture::new();
    let marker = fx._root.path().join("slept");
    let knob = marker.to_string_lossy().into_owned();
    let mut engine = Engine::open(fx.config(&[("FAKE_SLEEP_MARKER", knob.as_str())])).unwrap();
    let directive = engine.create_directive("Add a hello file", None).unwrap();
    drive(
        &mut engine,
        Duration::from_secs(60),
        "worker running",
        |e| e.store.tasks_in_state(TaskState::Running).unwrap().len() == 1,
    );

    assert_eq!(engine.kill_all().unwrap(), 1);
    assert!(engine.is_paused().unwrap());
    let task = engine
        .store
        .list_tasks(Some(&directive.id))
        .unwrap()
        .remove(0);
    assert_eq!(
        task.state,
        TaskState::Queued,
        "killed work is queued, not stranded"
    );
    assert_eq!(task.runs_used, 0, "a kill does not use up the run budget");

    engine.set_paused(false).unwrap();
    drive(
        &mut engine,
        Duration::from_secs(120),
        "directive done after resume",
        |e| e.store.get_directive(&directive.id).unwrap().unwrap().state == DirectiveState::Done,
    );
}
