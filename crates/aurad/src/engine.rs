//! The scheduler. Every tick reads the database and decides what should
//! happen next, so nothing is remembered in memory that a restart could lose.
//!
//! Order within a tick: reap finished runs, check the merge job, then start
//! planners, reviewers, workers and the next merge, and finally close out
//! directives whose tasks are all finished.

use crate::merge::{self, LandRequest, Landed};
use crate::proc;
use anyhow::{anyhow, bail, Context, Result};
use aura_adapters::CliAdapter;
use aura_core::brief;
use aura_core::config::Config;
use aura_core::model::{
    Autonomy, Directive, DirectiveState, Question, Role, Run, RunState, Scope, Task, TaskState,
};
use aura_core::quota::{self, LimitSignal, ProviderQuota};
use aura_core::records::{
    self, Plan, ReviewVerdict, Verdict, WorkerResult, WorkerStatus, BRIEF_FILE, PLAN_FILE,
    RESULT_FILE, REVIEW_FILE,
};
use aura_core::{scope, Paths};
use aura_store::{ts, Store};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes of log read when looking for a limit message.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

pub struct Engine {
    pub cfg: Config,
    pub repo: PathBuf,
    pub paths: Paths,
    pub store: Store,
    adapters: BTreeMap<String, CliAdapter>,
    active: BTreeMap<String, ActiveRun>,
    merge: Option<MergeJob>,
    last_mirror: Option<Instant>,
}

/// Branch that holds the human-readable mirror of task and event state.
pub const MIRROR_BRANCH: &str = "harness";
const MIRROR_EVERY: Duration = Duration::from_secs(5);
const MIRROR_EVENT_BATCH: usize = 500;

struct ActiveRun {
    run: Run,
    child: Child,
    started: Instant,
    scope_write: Vec<String>,
    worktree: PathBuf,
}

struct MergeJob {
    task_id: String,
    handle: JoinHandle<Landed>,
}

fn now_ts() -> String {
    ts(Utc::now())
}

fn read_record<T: DeserializeOwned>(path: &Path) -> std::result::Result<T, String> {
    let text =
        fs::read_to_string(path).map_err(|_| format!("no record written at {}", path.display()))?;
    records::parse(&text)
}

impl Engine {
    /// Opens the repository and database, adapts to whatever a previous daemon
    /// left behind, and returns an engine ready to tick.
    pub fn open(cfg: Config) -> Result<Engine> {
        cfg.validate()?;
        let repo = fs::canonicalize(&cfg.repo.path).with_context(|| {
            format!("repository path {} does not exist", cfg.repo.path.display())
        })?;
        if !aura_git::is_repo(&repo) {
            bail!("{} is not a git repository", repo.display());
        }
        if !aura_git::branch_exists(&repo, &cfg.repo.main_branch) {
            bail!(
                "branch {} does not exist in {}",
                cfg.repo.main_branch,
                repo.display()
            );
        }
        let paths = cfg.paths();
        for dir in [
            &paths.state_dir,
            &paths.worktrees,
            &paths.scratch,
            &paths.merge,
            &paths.logs,
        ] {
            fs::create_dir_all(dir)?;
        }
        aura_git::exclude(&repo, ".aura/")?;
        let store = Store::open(&paths.db)?;
        let adapters = aura_adapters::build_all(&cfg.providers)?;
        let mut engine = Engine {
            cfg,
            repo,
            paths,
            store,
            adapters,
            active: BTreeMap::new(),
            merge: None,
            last_mirror: None,
        };
        engine.reconcile()?;
        engine.clear_scratch()?;
        engine.ensure_mirror()?;
        Ok(engine)
    }

    /// Makes the database match reality after a daemon restart. Runs that were
    /// active are stopped and recorded as interrupted. Their tasks go back to
    /// the queue without using up a run, because the daemon was at fault.
    fn reconcile(&mut self) -> Result<()> {
        for mut run in self.store.runs_in_state(RunState::Running)? {
            if let Some(pid) = run.pid {
                if proc::is_alive(pid) {
                    proc::kill_group(pid);
                }
            }
            run.state = RunState::Interrupted;
            run.ended_at = Some(now_ts());
            run.reason = Some("daemon restarted during the run".into());
            self.store.save_run(&run)?;
            self.event(
                "run.interrupted",
                Some(&run.id),
                json!({"reason": "daemon restarted"}),
            )?;
            if let Some(task_id) = run.task_id.clone() {
                if let Some(mut task) = self.store.get_task(&task_id)? {
                    if task.state == TaskState::Running {
                        task.runs_used = task.runs_used.saturating_sub(1);
                        self.move_task(&mut task, TaskState::Queued, None)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes leftover scratch and merge worktrees from earlier runs.
    fn clear_scratch(&mut self) -> Result<()> {
        for dir in [self.paths.scratch.clone(), self.paths.merge.clone()] {
            for entry in fs::read_dir(&dir)?.flatten() {
                let _ = aura_git::remove_worktree(&self.repo, &entry.path());
            }
        }
        aura_git::run(&self.repo, &["worktree", "prune"])?;
        Ok(())
    }

    // ---- public operations (called by the API) ------------------------------

    pub fn is_paused(&self) -> Result<bool> {
        Ok(self.store.get_meta("paused")?.as_deref() == Some("1"))
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.store
            .set_meta("paused", if paused { "1" } else { "0" })?;
        self.event("engine.paused", None, json!({ "paused": paused }))
    }

    pub fn create_directive(
        &mut self,
        text: &str,
        autonomy: Option<Autonomy>,
    ) -> Result<Directive> {
        let text = text.trim();
        if text.is_empty() {
            bail!("directive text is empty");
        }
        let now = now_ts();
        let directive = Directive {
            id: self.store.next_id("D")?,
            text: text.to_string(),
            autonomy: autonomy.unwrap_or(self.cfg.defaults.autonomy),
            state: DirectiveState::Planning,
            planner_attempts: 0,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.save_directive(&directive)?;
        self.event(
            "directive.created",
            Some(&directive.id),
            json!({ "autonomy": directive.autonomy }),
        )?;
        Ok(directive)
    }

    /// Adds a task directly, without a planner. It starts at once unless the
    /// default autonomy asks for approval first.
    pub fn add_task(&mut self, spec: ManualTask) -> Result<Task> {
        if spec.title.trim().is_empty() {
            bail!("task title is empty");
        }
        scope::validate_patterns(&spec.scope.write).map_err(|e| anyhow!(e))?;
        scope::validate_patterns(&spec.scope.read).map_err(|e| anyhow!(e))?;
        if spec.scope.write.is_empty() {
            bail!("a task needs at least one write pattern");
        }
        for dep in &spec.depends_on {
            if self.store.get_task(dep)?.is_none() {
                bail!("unknown dependency {dep}");
            }
        }
        let acceptance = self.acceptance_for(&spec.acceptance)?;
        let autonomy = self.cfg.defaults.autonomy;
        let id = self.store.next_id("T")?;
        let now = now_ts();
        let mut task = Task {
            branch: format!("aura/{id}"),
            id,
            directive_id: None,
            title: spec.title,
            brief: spec.brief,
            scope: spec.scope,
            depends_on: spec.depends_on,
            acceptance,
            state: TaskState::Proposed,
            autonomy,
            worktree: None,
            worker_provider: None,
            runs_used: 0,
            max_runs: self.cfg.budgets.per_task_runs,
            notes: vec![],
            queue_seq: None,
            created_at: now.clone(),
            updated_at: now,
        };
        if autonomy != Autonomy::AskEach {
            task.set_state(TaskState::Queued)?;
            task.queue_seq = Some(self.store.next_counter("queue")?);
        }
        self.store.save_task(&task)?;
        self.event(
            "task.created",
            Some(&task.id),
            json!({ "state": task.state }),
        )?;
        Ok(task)
    }

    /// Approves a plan (`D-…`), a proposed task (`T-…`), a gated merge, or
    /// unblocks a task whose run budget ran out.
    pub fn approve(&mut self, id: &str) -> Result<String> {
        if id.starts_with("D-") {
            let mut directive = self
                .store
                .get_directive(id)?
                .ok_or_else(|| anyhow!("no directive {id}"))?;
            if directive.state != DirectiveState::AwaitingApproval {
                bail!(
                    "directive {id} is {}, not awaiting approval",
                    directive.state
                );
            }
            self.move_directive(&mut directive, DirectiveState::Active)?;
            let mut started = 0;
            for mut task in self.store.list_tasks(Some(id))? {
                if task.state == TaskState::Proposed && task.autonomy == Autonomy::AskPlan {
                    self.move_task(&mut task, TaskState::Queued, None)?;
                    started += 1;
                }
            }
            return Ok(format!("approved {id}; {started} task(s) queued"));
        }
        let mut task = self
            .store
            .get_task(id)?
            .ok_or_else(|| anyhow!("no task {id}"))?;
        match task.state {
            TaskState::Proposed => self.move_task(&mut task, TaskState::Queued, None)?,
            TaskState::Gated => self.move_task(&mut task, TaskState::MergeQueued, None)?,
            TaskState::Blocked => {
                task.runs_used = 0;
                self.move_task(&mut task, TaskState::Queued, Some("unblocked by operator"))?;
            }
            other => bail!("task {id} is {other}; nothing to approve"),
        }
        Ok(format!("approved {id}; now {}", task.state))
    }

    pub fn cancel(&mut self, id: &str) -> Result<String> {
        let mut task = self
            .store
            .get_task(id)?
            .ok_or_else(|| anyhow!("no task {id}"))?;
        if task.is_terminal() {
            bail!("task {id} is already {}", task.state);
        }
        if self.merge.as_ref().is_some_and(|m| m.task_id == id) {
            bail!("task {id} is being merged; cancel it after the merge finishes");
        }
        let active_run = self
            .active
            .iter()
            .find(|(_, a)| a.run.task_id.as_deref() == Some(id))
            .map(|(rid, _)| rid.clone());
        if let Some(run_id) = active_run {
            self.kill_run(&run_id, RunState::Killed, "cancelled by operator")?;
        }
        self.move_task(
            &mut task,
            TaskState::Cancelled,
            Some("cancelled by operator"),
        )?;
        if let Some(wt) = task.worktree.take() {
            aura_git::remove_worktree(&self.repo, Path::new(&wt))?;
            self.store.save_task(&task)?;
        }
        Ok(format!("cancelled {id}; branch {} kept", task.branch))
    }

    pub fn answer(&mut self, question_id: &str, text: &str) -> Result<String> {
        let mut question: Question = self
            .store
            .get_question(question_id)?
            .ok_or_else(|| anyhow!("no question {question_id}"))?;
        if question.answer.is_some() {
            bail!("question {question_id} is already answered");
        }
        question.answer = Some(text.trim().to_string());
        question.answered_at = Some(now_ts());
        self.store.save_question(&question)?;
        self.event(
            "question.answered",
            Some(&question.task_id),
            json!({ "question": question_id }),
        )?;

        let mut task = self
            .store
            .get_task(&question.task_id)?
            .ok_or_else(|| anyhow!("question {question_id} refers to a missing task"))?;
        let still_open = self
            .store
            .open_questions()?
            .iter()
            .any(|q| q.task_id == task.id);
        if task.state == TaskState::Blocked && !still_open {
            let note = format!("Question: {} Answer: {}", question.text, text.trim());
            self.move_task(&mut task, TaskState::Queued, Some(&note))?;
        }
        Ok(format!("answered {question_id}"))
    }

    /// Stops every run now, sends running tasks back to the queue, and pauses
    /// the scheduler. The merge in progress, if any, is left to finish.
    pub fn kill_all(&mut self) -> Result<usize> {
        self.set_paused(true)?;
        let runs: Vec<(String, Option<String>)> = self
            .active
            .iter()
            .map(|(id, a)| (id.clone(), a.run.task_id.clone()))
            .collect();
        for (run_id, task_id) in &runs {
            self.kill_run(run_id, RunState::Killed, "kill switch")?;
            // A killed worker goes back to the queue. The kill was not the agent's fault.
            if let Some(mut task) = task_id
                .as_deref()
                .map(|t| self.store.get_task(t))
                .transpose()?
                .flatten()
            {
                if task.state == TaskState::Running {
                    task.runs_used = task.runs_used.saturating_sub(1);
                    self.move_task(
                        &mut task,
                        TaskState::Queued,
                        Some("stopped by the kill switch"),
                    )?;
                }
            }
        }
        self.event("engine.killed", None, json!({ "runs": runs.len() }))?;
        Ok(runs.len())
    }

    // ---- the tick ----------------------------------------------------------

    /// One pass of the scheduler. Errors on one item are recorded as events and
    /// do not stop the others.
    pub fn tick(&mut self) -> Result<()> {
        let now = Utc::now();
        self.reap(now)?;
        self.poll_merge()?;
        if !self.is_paused()? {
            self.guarded("planners", |e| e.start_planners(now));
            self.guarded("reviews", |e| e.start_reviews(now));
            self.guarded("workers", |e| e.start_workers(now));
            self.guarded("merge", |e| e.start_merge());
        }
        self.guarded("directives", |e| e.complete_directives());
        if self.last_mirror.is_none_or(|t| t.elapsed() >= MIRROR_EVERY) {
            self.guarded("mirror", |e| e.sync_mirror());
            self.last_mirror = Some(Instant::now());
        }
        Ok(())
    }

    /// Makes sure the `harness` branch exists and has a worktree.
    fn ensure_mirror(&self) -> Result<()> {
        let main = self.cfg.repo.main_branch.clone();
        let mirror = self.paths.mirror();
        if mirror.exists() {
            return Ok(());
        }
        aura_git::run(&self.repo, &["worktree", "prune"])?;
        if !aura_git::branch_exists(&self.repo, MIRROR_BRANCH) {
            aura_git::run(&self.repo, &["branch", MIRROR_BRANCH, &main])?;
        }
        aura_git::add_worktree_for_branch(&self.repo, &mirror, MIRROR_BRANCH, &main)?;
        Ok(())
    }

    /// Writes tasks, directives and new events into the mirror worktree and
    /// commits them to the `harness` branch. Nothing here affects scheduling.
    pub fn sync_mirror(&mut self) -> Result<()> {
        self.ensure_mirror()?;
        let mirror = self.paths.mirror();
        let tasks_dir = mirror.join("tasks");
        let directives_dir = mirror.join("directives");
        fs::create_dir_all(&tasks_dir)?;
        fs::create_dir_all(&directives_dir)?;

        for task in self.store.list_tasks(None)? {
            let questions = self.store.questions_for(&task.id)?;
            let mut text = format!(
                "# {} {}\n\n- state: {}\n- directive: {}\n- branch: {}\n- runs: {}/{}\n- scope: {}\n\n## Brief\n\n{}\n",
                task.id,
                task.title,
                task.state,
                task.directive_id.as_deref().unwrap_or("-"),
                task.branch,
                task.runs_used,
                task.max_runs,
                task.scope.write.join(", "),
                task.brief.trim()
            );
            if !task.notes.is_empty() {
                text.push_str("\n## Notes\n\n");
                for note in &task.notes {
                    text.push_str(&format!("- {}\n", note.replace('\n', " ")));
                }
            }
            for q in questions {
                text.push_str(&format!(
                    "\n## Question {}\n\n{}\n\nAnswer: {}\n",
                    q.id,
                    q.text,
                    q.answer.as_deref().unwrap_or("(unanswered)")
                ));
            }
            fs::write(tasks_dir.join(format!("{}.md", task.id)), text)?;
        }
        for d in self.store.list_directives()? {
            let text = format!(
                "# {}\n\n- state: {}\n- autonomy: {}\n- planner attempts: {}\n\n{}\n",
                d.id,
                d.state,
                d.autonomy,
                d.planner_attempts,
                d.text.trim()
            );
            fs::write(directives_dir.join(format!("{}.md", d.id)), text)?;
        }

        // The event log is append-only, so only new events are written.
        let cursor: i64 = self
            .store
            .get_meta("mirror_seq")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let events = self.store.events_since(cursor, MIRROR_EVENT_BATCH)?;
        if !events.is_empty() {
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(mirror.join("events.jsonl"))?;
            let mut last = cursor;
            for event in &events {
                writeln!(log, "{}", serde_json::to_string(event)?)?;
                last = event.seq;
            }
            self.store.set_meta("mirror_seq", &last.to_string())?;
        }

        aura_git::commit_all(&mirror, "harness: sync task and event state")?;
        Ok(())
    }

    fn guarded(&mut self, what: &str, f: impl FnOnce(&mut Engine) -> Result<()>) {
        if let Err(err) = f(self) {
            let _ = self.event(
                "engine.error",
                None,
                json!({ "during": what, "error": format!("{err:#}") }),
            );
        }
    }

    fn capacity_left(&self) -> bool {
        self.active.len() < self.cfg.defaults.max_parallel_runs
    }

    fn running_on(&self, provider: &str) -> usize {
        self.active
            .values()
            .filter(|a| a.run.provider == provider)
            .count()
    }

    /// First provider from the role's list that is enabled, below its
    /// parallel limit, and not refused by its quota right now.
    fn pick(
        &mut self,
        role: Role,
        exclude: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<String>> {
        let names: Vec<String> = match role {
            Role::Planner => self.cfg.roles.planner.clone(),
            Role::Worker => self.cfg.roles.worker.clone(),
            Role::Reviewer => self.cfg.roles.reviewer.clone(),
        };
        for name in &names {
            if Some(name.as_str()) != exclude && self.usable(name, now)? {
                return Ok(Some(name.clone()));
            }
        }
        if role == Role::Reviewer && self.cfg.roles.allow_self_review {
            for name in &names {
                if self.usable(name, now)? {
                    return Ok(Some(name.clone()));
                }
            }
        }
        Ok(None)
    }

    fn usable(&mut self, name: &str, now: DateTime<Utc>) -> Result<bool> {
        if !self.adapters.contains_key(name) {
            return Ok(false);
        }
        let max = self.cfg.providers.get(name).map_or(1, |p| p.max_parallel);
        if self.running_on(name) >= max {
            return Ok(false);
        }
        self.provider_available(name, now)
    }

    /// Whether a provider may be used now. Persists the moment a refused
    /// provider's retry time passes, so the transition is recorded once.
    fn provider_available(&mut self, provider: &str, now: DateTime<Utc>) -> Result<bool> {
        let Some(q) = self.store.get_quota(provider)? else {
            return Ok(true);
        };
        if quota::is_available(&q, now) {
            if q.state != aura_core::QuotaState::Available {
                let recovered = ProviderQuota {
                    state: aura_core::QuotaState::Available,
                    retry_at: None,
                    source: None,
                    updated_at: now,
                    ..q
                };
                self.store.save_quota(&recovered)?;
                self.event("quota.available", Some(provider), json!({}))?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn record_limit(
        &mut self,
        provider: &str,
        signal: &LimitSignal,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let prev = self
            .store
            .get_quota(provider)?
            .unwrap_or_else(|| quota::fresh(provider, now));
        let window = self.adapters.get(provider).and_then(|a| a.window());
        let next = quota::on_limit(&prev, signal, now, window);
        self.store.save_quota(&next)?;
        self.event(
            "quota.limited",
            Some(provider),
            json!({
                "state": next.state,
                "retry_at": next.retry_at.map(ts),
                "source": next.source,
                "excerpt": signal.excerpt,
            }),
        )
    }

    fn reset_quota_after_success(&mut self, provider: &str, now: DateTime<Utc>) -> Result<()> {
        if let Some(q) = self.store.get_quota(provider)? {
            if q.backoff_minutes > 0 {
                self.store.save_quota(&quota::on_success(&q, now))?;
            }
        }
        Ok(())
    }

    // ---- starting runs -----------------------------------------------------

    fn start_planners(&mut self, now: DateTime<Utc>) -> Result<()> {
        for directive in self.store.list_directives()? {
            if directive.state != DirectiveState::Planning {
                continue;
            }
            if self
                .active
                .values()
                .any(|a| a.run.directive_id.as_deref() == Some(&directive.id))
            {
                continue;
            }
            if !self.capacity_left() {
                break;
            }
            let Some(provider) = self.pick(Role::Planner, None, now)? else {
                continue;
            };
            if let Err(err) = self.start_planner(directive.clone(), &provider) {
                self.record_planner_failure(directive, &format!("cannot start planner: {err:#}"))?;
            }
        }
        Ok(())
    }

    fn start_planner(&mut self, directive: Directive, provider: &str) -> Result<()> {
        let run = self.new_run(Role::Planner, provider, None, Some(&directive.id))?;
        let scratch = self.paths.scratch.join(&run.id);
        aura_git::add_detached_worktree(&self.repo, &scratch, &self.cfg.repo.main_branch)?;
        let text = brief::planner(
            &directive.text,
            &self.cfg.repo.main_branch,
            &self.cfg.acceptance.allowed_programs,
        );
        write_brief(&scratch, &text)?;
        self.spawn(run, scratch, vec![], None)
    }

    fn start_reviews(&mut self, now: DateTime<Utc>) -> Result<()> {
        for task in self.store.tasks_in_state(TaskState::Review)? {
            let reviewing = self.active.values().any(|a| {
                a.run.role == Role::Reviewer && a.run.task_id.as_deref() == Some(&task.id)
            });
            if reviewing {
                continue;
            }
            if !self.capacity_left() {
                break;
            }
            let Some(provider) = self.pick(Role::Reviewer, task.worker_provider.as_deref(), now)?
            else {
                continue;
            };
            let run = self.new_run(
                Role::Reviewer,
                &provider,
                Some(&task.id),
                task.directive_id.as_deref(),
            )?;
            let scratch = self.paths.scratch.join(&run.id);
            let started = aura_git::add_detached_worktree(&self.repo, &scratch, &task.branch)
                .map_err(anyhow::Error::from)
                .and_then(|_| {
                    write_brief(&scratch, &brief::review(&task, &self.cfg.repo.main_branch))
                });
            if let Err(err) = started
                .and_then(|_| self.spawn(run.clone(), scratch.clone(), vec![], Some(&task.id)))
            {
                self.record_start_failure(
                    run,
                    &scratch,
                    &format!("cannot start reviewer: {err:#}"),
                )?;
            }
        }
        Ok(())
    }

    fn start_workers(&mut self, now: DateTime<Utc>) -> Result<()> {
        let mut queued = self.store.tasks_in_state(TaskState::Queued)?;
        queued.sort_by_key(|t| (t.queue_seq.unwrap_or(i64::MAX), t.id.clone()));
        for mut task in queued {
            if !self.capacity_left() {
                break;
            }
            if !self.dependencies_done(&task)? {
                continue;
            }
            if self.scope_busy(&task) {
                continue;
            }
            if task.runs_used >= task.max_runs {
                let note = format!("run budget of {} exhausted", task.max_runs);
                self.move_task(&mut task, TaskState::Blocked, Some(&note))?;
                continue;
            }
            let Some(provider) = self.pick(Role::Worker, None, now)? else {
                continue;
            };
            if let Err(err) = self.start_worker(&mut task, &provider) {
                let reason = format!("cannot start worker: {err:#}");
                self.event("task.error", Some(&task.id), json!({ "error": reason }))?;
            }
        }
        Ok(())
    }

    fn dependencies_done(&self, task: &Task) -> Result<bool> {
        for dep in &task.depends_on {
            match self.store.get_task(dep)? {
                Some(d) if d.state == TaskState::Done => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// True when an active worker run could touch files this task will change.
    fn scope_busy(&self, task: &Task) -> bool {
        self.active
            .values()
            .filter(|a| a.run.role == Role::Worker)
            .any(|a| scope::overlaps(&a.scope_write, &task.scope.write))
    }

    fn start_worker(&mut self, task: &mut Task, provider: &str) -> Result<()> {
        let main = self.cfg.repo.main_branch.clone();
        let wt = self.paths.worktrees.join(&task.id);
        if !wt.exists() {
            // Forget worktrees whose directories were deleted by hand.
            aura_git::run(&self.repo, &["worktree", "prune"])?;
            aura_git::add_worktree_for_branch(&self.repo, &wt, &task.branch, &main)?;
        }
        let _ = fs::remove_file(wt.join(RESULT_FILE));
        write_brief(&wt, &brief::worker(task, &main))?;

        let run = self.new_run(
            Role::Worker,
            provider,
            Some(&task.id),
            task.directive_id.as_deref(),
        )?;
        task.worktree = Some(wt.to_string_lossy().into_owned());
        task.runs_used += 1;
        task.worker_provider = Some(provider.to_string());
        task.updated_at = now_ts();
        self.move_task(task, TaskState::Running, None)?;
        let scope_write = task.scope.write.clone();
        if let Err(err) = self.spawn(run.clone(), wt.clone(), scope_write, Some(&task.id)) {
            let mut run = run;
            run.ended_at = Some(now_ts());
            self.fail_worker(task, &run, &format!("cannot start worker: {err:#}"))?;
        }
        Ok(())
    }

    fn start_merge(&mut self) -> Result<()> {
        if self.merge.is_some() {
            return Ok(());
        }
        let mut ready = self.store.tasks_in_state(TaskState::MergeQueued)?;
        ready.sort_by_key(|t| (t.queue_seq.unwrap_or(i64::MAX), t.id.clone()));
        let Some(task) = ready.into_iter().next() else {
            return Ok(());
        };
        let mut checks = task.acceptance.clone();
        checks.retain(|c| !c.trim().is_empty());
        let req = LandRequest {
            repo: self.repo.clone(),
            scratch: self.paths.merge.join(&task.id),
            main_branch: self.cfg.repo.main_branch.clone(),
            branch: task.branch.clone(),
            title: task.title.clone(),
            checks,
        };
        let handle = std::thread::spawn(move || merge::land(&req));
        self.event(
            "merge.started",
            Some(&task.id),
            json!({ "branch": task.branch }),
        )?;
        self.merge = Some(MergeJob {
            task_id: task.id,
            handle,
        });
        Ok(())
    }

    fn poll_merge(&mut self) -> Result<()> {
        let finished = match &self.merge {
            Some(job) => job.handle.is_finished(),
            None => return Ok(()),
        };
        if !finished {
            return Ok(());
        }
        let job = self.merge.take().expect("checked above");
        let outcome = job
            .handle
            .join()
            .unwrap_or_else(|_| Landed::Error("merge thread panicked".into()));
        let Some(mut task) = self.store.get_task(&job.task_id)? else {
            return Ok(());
        };
        match outcome {
            Landed::Merged { sha } => {
                self.move_task(&mut task, TaskState::Done, None)?;
                if let Some(wt) = task.worktree.take() {
                    aura_git::remove_worktree(&self.repo, Path::new(&wt))?;
                }
                self.store.save_task(&task)?;
                self.event("task.merged", Some(&task.id), json!({ "commit": sha }))?;
            }
            Landed::Conflict(files) => {
                let note = format!(
                    "Merging into {} conflicts in: {}. Merge {} into your branch, resolve the conflicts, and commit.",
                    self.cfg.repo.main_branch,
                    files.join(", "),
                    self.cfg.repo.main_branch
                );
                self.move_task(&mut task, TaskState::Queued, Some(&note))?;
            }
            Landed::CheckFailed { command, output } => {
                let note = format!("Checks failed on the merge result: `{command}`\n{output}");
                self.move_task(&mut task, TaskState::Queued, Some(&note))?;
            }
            Landed::Stale => {
                // The main branch moved. The task stays in merge-queued and is retried.
                self.event(
                    "merge.retry",
                    Some(&task.id),
                    json!({ "reason": "main branch moved" }),
                )?;
            }
            Landed::Error(message) => {
                let note = format!("Merge could not run: {message}");
                self.move_task(&mut task, TaskState::Blocked, Some(&note))?;
            }
        }
        Ok(())
    }

    // ---- reaping -------------------------------------------------------------

    fn reap(&mut self, now: DateTime<Utc>) -> Result<()> {
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            let exited = match self.active.get_mut(&id) {
                Some(a) => a.child.try_wait()?,
                None => continue,
            };
            let (status, forced) = match exited {
                Some(status) => (status, None),
                None => match self.timeout_reason(&id) {
                    None => continue,
                    Some(reason) => {
                        let a = self.active.get_mut(&id).expect("present");
                        proc::kill_group(a.child.id());
                        let status = a.child.wait()?;
                        (status, Some(reason))
                    }
                },
            };
            self.finish(&id, status, forced, now)?;
        }
        Ok(())
    }

    fn timeout_reason(&self, id: &str) -> Option<String> {
        let a = self.active.get(id)?;
        let wall = Duration::from_secs(self.cfg.budgets.run_timeout_minutes * 60);
        if a.started.elapsed() > wall {
            return Some(format!(
                "exceeded the run timeout of {} minutes",
                self.cfg.budgets.run_timeout_minutes
            ));
        }
        let idle = Duration::from_secs(self.cfg.budgets.idle_timeout_minutes * 60);
        match proc::idle_for(Path::new(&a.run.log_path)) {
            Some(quiet) if quiet > idle => Some(format!(
                "no output for {} minutes",
                self.cfg.budgets.idle_timeout_minutes
            )),
            _ => None,
        }
    }

    /// Stops a run now and records how it ended.
    fn kill_run(&mut self, id: &str, state: RunState, reason: &str) -> Result<()> {
        let Some(mut active) = self.active.remove(id) else {
            return Ok(());
        };
        proc::kill_group(active.child.id());
        let status = active.child.wait().ok();
        let mut run = active.run;
        run.state = state;
        run.reason = Some(reason.to_string());
        run.ended_at = Some(now_ts());
        run.exit_code = status.and_then(|s| s.code());
        self.store.save_run(&run)?;
        self.event(
            "run.stopped",
            Some(&run.id),
            json!({ "state": state, "reason": reason }),
        )?;
        if run.role != Role::Worker {
            self.discard_scratch(&active.worktree);
        }
        Ok(())
    }

    fn finish(
        &mut self,
        id: &str,
        status: ExitStatus,
        forced: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Some(active) = self.active.remove(id) else {
            return Ok(());
        };
        let ActiveRun {
            mut run, worktree, ..
        } = active;
        run.exit_code = status.code();
        run.ended_at = Some(ts(now));
        let failure = match &forced {
            Some(reason) => Some(reason.clone()),
            None if status.success() => None,
            None => Some(match status.code() {
                Some(code) => format!("exited with status {code}"),
                None => "terminated by a signal".to_string(),
            }),
        };

        // A refusal shows up as a failed run whose output names the limit.
        if failure.is_some() && forced.is_none() {
            let tail = proc::read_tail(Path::new(&run.log_path), LOG_TAIL_BYTES);
            let signal = self
                .adapters
                .get(&run.provider)
                .and_then(|a| a.detect_limit(&tail, now));
            if let Some(signal) = signal {
                run.state = RunState::Limited;
                run.reason = Some(signal.excerpt.clone());
                self.store.save_run(&run)?;
                self.record_limit(&run.provider.clone(), &signal, now)?;
                return self.on_limited(&run, &worktree);
            }
        }

        let state = match (&forced, &failure) {
            (Some(_), _) => RunState::TimedOut,
            (None, None) => RunState::Succeeded,
            (None, Some(_)) => RunState::Failed,
        };
        run.state = state;
        run.reason = failure.clone();
        self.store.save_run(&run)?;
        if failure.is_none() {
            self.reset_quota_after_success(&run.provider.clone(), now)?;
        }

        match run.role {
            Role::Worker => self.finish_worker(run, worktree, failure),
            Role::Reviewer => self.finish_reviewer(run, worktree, failure),
            Role::Planner => self.finish_planner(run, worktree, failure),
        }
    }

    fn on_limited(&mut self, run: &Run, worktree: &Path) -> Result<()> {
        match run.role {
            Role::Worker => {
                if let Some(task_id) = &run.task_id {
                    if let Some(mut task) = self.store.get_task(task_id)? {
                        if task.state == TaskState::Running {
                            // A refusal is not the agent's fault, so the run does not count.
                            task.runs_used = task.runs_used.saturating_sub(1);
                            let scope_ok = aura_git::uncommitted_paths(worktree)
                                .map(|paths| {
                                    scope::violations(&task.scope.write, &paths).is_empty()
                                })
                                .unwrap_or(false);
                            if scope_ok {
                                let _ = aura_git::commit_all(
                                    worktree,
                                    &format!("{}: WIP at limit", task.id),
                                );
                            }
                            self.move_task(&mut task, TaskState::Queued, None)?;
                        }
                    }
                }
            }
            Role::Reviewer | Role::Planner => self.discard_scratch(worktree),
        }
        Ok(())
    }

    fn finish_worker(
        &mut self,
        run: Run,
        worktree: PathBuf,
        failure: Option<String>,
    ) -> Result<()> {
        let Some(task_id) = run.task_id.clone() else {
            return Ok(());
        };
        let Some(mut task) = self.store.get_task(&task_id)? else {
            return Ok(());
        };
        if task.state != TaskState::Running {
            return Ok(()); // cancelled while the run was going
        }
        if let Some(reason) = failure {
            return self.fail_worker(&mut task, &run, &reason);
        }
        let record: Result<WorkerResult, String> = read_record(&worktree.join(RESULT_FILE));
        match record {
            Err(reason) => self.fail_worker(&mut task, &run, &reason),
            Ok(result) => match result.status {
                WorkerStatus::Failed => {
                    let reason = format!("worker reported failure: {}", result.summary);
                    self.fail_worker(&mut task, &run, &reason)
                }
                WorkerStatus::Blocked => {
                    for text in &result.questions {
                        let question = Question {
                            id: self.store.next_id("Q")?,
                            task_id: task.id.clone(),
                            text: text.clone(),
                            answer: None,
                            created_at: now_ts(),
                            answered_at: None,
                        };
                        self.store.insert_question(&question)?;
                        self.event(
                            "question.asked",
                            Some(&task.id),
                            json!({ "question": question.id }),
                        )?;
                    }
                    self.move_task(
                        &mut task,
                        TaskState::Blocked,
                        Some(&format!("worker is blocked: {}", result.summary)),
                    )
                }
                WorkerStatus::ReadyForReview => match self.verify_scope(&task, &worktree) {
                    Ok(()) => self.move_task(&mut task, TaskState::Review, None),
                    Err(reason) => self.fail_worker(&mut task, &run, &reason),
                },
            },
        }
    }

    /// Commits leftover work and checks that the branch changes only files in
    /// the task's write scope.
    fn verify_scope(&self, task: &Task, worktree: &Path) -> std::result::Result<(), String> {
        let leftovers = aura_git::uncommitted_paths(worktree).map_err(|e| e.to_string())?;
        let stray = scope::violations(&task.scope.write, &leftovers);
        if !stray.is_empty() {
            return Err(format!(
                "uncommitted changes outside the write scope: {}",
                stray.join(", ")
            ));
        }
        aura_git::commit_all(worktree, &format!("{}: {}", task.id, task.title))
            .map_err(|e| e.to_string())?;
        let changed = aura_git::changed_files(&self.repo, &self.cfg.repo.main_branch, &task.branch)
            .map_err(|e| e.to_string())?;
        if changed.is_empty() {
            return Err("the branch has no changes against the main branch".into());
        }
        let stray = scope::violations(&task.scope.write, &changed);
        if !stray.is_empty() {
            return Err(format!(
                "changes outside the write scope: {}",
                stray.join(", ")
            ));
        }
        Ok(())
    }

    /// A worker run's output was rejected or the run failed. The run is recorded
    /// as failed, then the task is retried while the budget allows, otherwise blocked.
    fn fail_worker(&mut self, task: &mut Task, run: &Run, reason: &str) -> Result<()> {
        let mut run = run.clone();
        if run.state == RunState::Succeeded {
            run.state = RunState::Failed;
        }
        run.reason = Some(reason.to_string());
        self.store.save_run(&run)?;
        if task.runs_used < task.max_runs {
            self.move_task(
                task,
                TaskState::Queued,
                Some(&format!("previous run failed: {reason}")),
            )
        } else {
            self.move_task(
                task,
                TaskState::Blocked,
                Some(&format!("run budget exhausted; last failure: {reason}")),
            )
        }
    }

    fn finish_reviewer(
        &mut self,
        run: Run,
        scratch: PathBuf,
        failure: Option<String>,
    ) -> Result<()> {
        let Some(task_id) = run.task_id.clone() else {
            return Ok(());
        };
        let Some(mut task) = self.store.get_task(&task_id)? else {
            return Ok(());
        };
        // Read the record before the scratch worktree is removed.
        let verdict: std::result::Result<ReviewVerdict, String> = match &failure {
            Some(reason) => Err(reason.clone()),
            None => read_record(&scratch.join(REVIEW_FILE)),
        };
        self.discard_scratch(&scratch);
        if task.state != TaskState::Review {
            return Ok(());
        }
        let mut run = run;
        if let Err(reason) = &verdict {
            run.state = RunState::Failed;
            run.reason = Some(reason.clone());
            self.store.save_run(&run)?;
        }
        match verdict {
            Ok(v) => match v.verdict {
                Verdict::Approve => {
                    let next = if task.autonomy == Autonomy::AskEach {
                        TaskState::Gated
                    } else {
                        TaskState::MergeQueued
                    };
                    self.move_task(&mut task, next, None)
                }
                Verdict::RequestChanges => {
                    let note = format!("Review requested changes: {}", v.notes.trim());
                    self.move_task(&mut task, TaskState::Queued, Some(&note))
                }
            },
            Err(reason) => {
                // A reviewer that fails again and again should not loop forever.
                let failed = self
                    .store
                    .count_runs(&task.id, Role::Reviewer, RunState::Failed)?;
                let _ = reason;
                if failed >= task.max_runs {
                    self.move_task(
                        &mut task,
                        TaskState::Blocked,
                        Some("review failed repeatedly"),
                    )
                } else {
                    Ok(()) // stays in review; the next tick starts another reviewer
                }
            }
        }
    }

    fn finish_planner(
        &mut self,
        run: Run,
        scratch: PathBuf,
        failure: Option<String>,
    ) -> Result<()> {
        let Some(directive_id) = run.directive_id.clone() else {
            return Ok(());
        };
        let Some(directive) = self.store.get_directive(&directive_id)? else {
            return Ok(());
        };
        // Read the record before the scratch worktree is removed.
        let plan: std::result::Result<Plan, String> = match &failure {
            Some(reason) => Err(reason.clone()),
            None => read_record(&scratch.join(PLAN_FILE)),
        };
        self.discard_scratch(&scratch);
        if directive.state != DirectiveState::Planning {
            return Ok(());
        }
        let mut run = run;
        if let Err(reason) = &plan {
            run.state = RunState::Failed;
            run.reason = Some(reason.clone());
            self.store.save_run(&run)?;
        }
        match plan {
            Ok(plan) => match self.materialize_plan(directive.clone(), &plan) {
                Ok(count) => self.event(
                    "plan.created",
                    Some(&directive_id),
                    json!({ "tasks": count }),
                ),
                Err(reason) => self.record_planner_failure(directive, &reason),
            },
            Err(reason) => self.record_planner_failure(directive, &reason),
        }
    }

    fn record_planner_failure(&mut self, mut directive: Directive, reason: &str) -> Result<()> {
        directive.planner_attempts += 1;
        directive.updated_at = now_ts();
        if directive.planner_attempts >= self.cfg.budgets.per_task_runs {
            directive.set_state(DirectiveState::Blocked)?;
            self.store.save_directive(&directive)?;
            self.event(
                "directive.blocked",
                Some(&directive.id),
                json!({ "reason": reason }),
            )
        } else {
            self.store.save_directive(&directive)?;
            self.event(
                "plan.failed",
                Some(&directive.id),
                json!({ "reason": reason }),
            )
        }
    }

    /// Creates tasks from a planner's output. Validates the whole plan first,
    /// so a bad plan creates nothing.
    fn materialize_plan(
        &mut self,
        mut directive: Directive,
        plan: &Plan,
    ) -> std::result::Result<usize, String> {
        plan.validate()?;
        for planned in &plan.tasks {
            self.acceptance_for(&planned.acceptance)
                .map_err(|e| e.to_string())?;
        }
        let mut ids = Vec::new();
        for _ in &plan.tasks {
            ids.push(self.store.next_id("T").map_err(|e| e.to_string())?);
        }
        let auto = directive.autonomy == Autonomy::Auto;
        for (i, planned) in plan.tasks.iter().enumerate() {
            let acceptance = self
                .acceptance_for(&planned.acceptance)
                .map_err(|e| e.to_string())?;
            let mut task = Task {
                id: ids[i].clone(),
                directive_id: Some(directive.id.clone()),
                title: planned.title.clone(),
                brief: planned.brief.clone(),
                scope: Scope {
                    write: planned.scope.write.clone(),
                    read: planned.scope.read.clone(),
                },
                depends_on: planned.depends_on.iter().map(|d| ids[*d].clone()).collect(),
                acceptance,
                state: TaskState::Proposed,
                autonomy: directive.autonomy,
                branch: format!("aura/{}", ids[i]),
                worktree: None,
                worker_provider: None,
                runs_used: 0,
                max_runs: self.cfg.budgets.per_task_runs,
                notes: vec![],
                queue_seq: None,
                created_at: now_ts(),
                updated_at: now_ts(),
            };
            if auto {
                task.set_state(TaskState::Queued)
                    .map_err(|e| e.to_string())?;
                task.queue_seq = Some(
                    self.store
                        .next_counter("queue")
                        .map_err(|e| e.to_string())?,
                );
            }
            self.store.save_task(&task).map_err(|e| e.to_string())?;
            self.event(
                "task.created",
                Some(&task.id),
                json!({ "directive": directive.id }),
            )
            .ok();
        }
        let next = if auto {
            DirectiveState::Active
        } else {
            DirectiveState::AwaitingApproval
        };
        directive.set_state(next).map_err(|e| e.to_string())?;
        directive.updated_at = now_ts();
        self.store
            .save_directive(&directive)
            .map_err(|e| e.to_string())?;
        Ok(plan.tasks.len())
    }

    fn complete_directives(&mut self) -> Result<()> {
        for mut directive in self.store.list_directives()? {
            if directive.state != DirectiveState::Active {
                continue;
            }
            let tasks = self.store.list_tasks(Some(&directive.id))?;
            if !tasks.is_empty() && tasks.iter().all(|t| t.is_terminal()) {
                let done = tasks.iter().filter(|t| t.state == TaskState::Done).count();
                self.move_directive(&mut directive, DirectiveState::Done)?;
                self.event(
                    "directive.done",
                    Some(&directive.id),
                    json!({ "merged": done, "total": tasks.len() }),
                )?;
            }
        }
        Ok(())
    }

    // ---- helpers -----------------------------------------------------------

    /// Every acceptance command must start with an allowed program. Commands
    /// run without a shell, so nothing else can be chained or substituted.
    fn acceptance_for(&self, commands: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for command in commands
            .iter()
            .chain(self.cfg.acceptance.default_checks.iter())
        {
            let command = command.trim();
            if command.is_empty() {
                continue;
            }
            let program = command.split_whitespace().next().unwrap_or_default();
            if !self
                .cfg
                .acceptance
                .allowed_programs
                .iter()
                .any(|p| p == program)
            {
                bail!("acceptance command {command:?} starts with {program:?}, which is not an allowed program");
            }
            out.push(command.to_string());
        }
        Ok(out)
    }

    fn new_run(
        &mut self,
        role: Role,
        provider: &str,
        task: Option<&str>,
        directive: Option<&str>,
    ) -> Result<Run> {
        let id = self.store.next_id("R")?;
        let run = Run {
            log_path: self
                .paths
                .logs
                .join(format!("{id}.log"))
                .to_string_lossy()
                .into_owned(),
            id,
            task_id: task.map(str::to_string),
            directive_id: directive.map(str::to_string),
            role,
            provider: provider.to_string(),
            state: RunState::Running,
            pid: None,
            started_at: now_ts(),
            ended_at: None,
            exit_code: None,
            reason: None,
        };
        self.store.save_run(&run)?;
        Ok(run)
    }

    fn spawn(
        &mut self,
        mut run: Run,
        worktree: PathBuf,
        scope_write: Vec<String>,
        task: Option<&str>,
    ) -> Result<()> {
        let adapter = self
            .adapters
            .get(&run.provider)
            .ok_or_else(|| anyhow!("no adapter for provider {}", run.provider))?;
        let inv = adapter.invocation(brief::PROMPT);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&run.log_path)?;
        let log_err = log.try_clone()?;
        let child = Command::new(&inv.program)
            .args(&inv.args)
            .envs(&inv.env)
            .current_dir(&worktree)
            .env("AURA_ROLE", run.role.as_str())
            .env("AURA_RUN", &run.id)
            .env("AURA_TASK", task.unwrap_or(""))
            .env("AURA_DIRECTIVE", run.directive_id.as_deref().unwrap_or(""))
            .env("AURA_WORKTREE", &worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot start {}", inv.program))?;
        run.pid = Some(child.id());
        self.store.save_run(&run)?;
        self.event(
            "run.started",
            Some(&run.id),
            json!({ "role": run.role, "provider": run.provider, "task": run.task_id }),
        )?;
        self.active.insert(
            run.id.clone(),
            ActiveRun {
                run,
                child,
                started: Instant::now(),
                scope_write,
                worktree,
            },
        );
        Ok(())
    }

    fn record_start_failure(&mut self, mut run: Run, scratch: &Path, reason: &str) -> Result<()> {
        run.state = RunState::Failed;
        run.reason = Some(reason.to_string());
        run.ended_at = Some(now_ts());
        self.store.save_run(&run)?;
        self.discard_scratch(scratch);
        self.event("run.failed", Some(&run.id), json!({ "reason": reason }))
    }

    fn discard_scratch(&self, dir: &Path) {
        if dir.starts_with(&self.paths.scratch) || dir.starts_with(&self.paths.merge) {
            let _ = aura_git::remove_worktree(&self.repo, dir);
        }
    }

    fn move_task(&mut self, task: &mut Task, to: TaskState, note: Option<&str>) -> Result<()> {
        let from = task.state;
        task.set_state(to)?;
        if matches!(to, TaskState::Queued | TaskState::MergeQueued) {
            task.queue_seq = Some(self.store.next_counter("queue")?);
        }
        if let Some(note) = note {
            task.notes.push(note.to_string());
        }
        task.updated_at = now_ts();
        self.store.save_task(task)?;
        self.event(
            "task.state",
            Some(&task.id),
            json!({ "from": from, "to": to, "note": note }),
        )
    }

    fn move_directive(&mut self, directive: &mut Directive, to: DirectiveState) -> Result<()> {
        let from = directive.state;
        directive.set_state(to)?;
        directive.updated_at = now_ts();
        self.store.save_directive(directive)?;
        self.event(
            "directive.state",
            Some(&directive.id),
            json!({ "from": from, "to": to }),
        )
    }

    pub fn event(&self, kind: &str, subject: Option<&str>, payload: Value) -> Result<()> {
        self.store.append_event(kind, subject, payload)?;
        Ok(())
    }

    /// Current effective quota of every provider the config names.
    pub fn quota_views(&self, now: DateTime<Utc>) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for (name, provider) in &self.cfg.providers {
            if !provider.enabled {
                continue;
            }
            let stored = self.store.get_quota(name)?;
            let (state, retry_at, source, backoff) = match &stored {
                Some(q) => (
                    quota::effective(q, now),
                    q.retry_at,
                    q.source.clone(),
                    q.backoff_minutes,
                ),
                None => (aura_core::QuotaState::Available, None, None, 0),
            };
            out.push(json!({
                "provider": name,
                "state": state,
                "retry_at": retry_at.map(ts),
                "source": source,
                "backoff_minutes": backoff,
                "running": self.running_on(name),
            }));
        }
        Ok(out)
    }

    pub fn active_runs(&self) -> Vec<Run> {
        self.active.values().map(|a| a.run.clone()).collect()
    }

    pub fn merge_in_progress(&self) -> Option<String> {
        self.merge.as_ref().map(|m| m.task_id.clone())
    }

    /// The soonest time a refused provider can run again.
    pub fn next_quota_wake(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        Ok(quota::next_wake(&self.store.list_quotas()?, now))
    }
}

/// A task created by hand, without a planner.
#[derive(Debug, Clone)]
pub struct ManualTask {
    pub title: String,
    pub brief: String,
    pub scope: Scope,
    pub depends_on: Vec<String>,
    pub acceptance: Vec<String>,
}

fn write_brief(dir: &Path, text: &str) -> Result<()> {
    let path = dir.join(BRIEF_FILE);
    fs::create_dir_all(path.parent().expect("brief path has a parent"))?;
    fs::write(path, text)?;
    Ok(())
}
