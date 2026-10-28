//! SQLite persistence for directives, tasks, runs, questions, quota and the
//! append-only event log. The daemon is the only writer.

use aura_core::model::{
    Autonomy, Directive, DirectiveState, Question, QuotaState, Role, Run, RunState, Scope, Task,
    TaskState,
};
use aura_core::quota::ProviderQuota;
use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use std::str::FromStr;
use thiserror::Error;

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("stored data is invalid: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One entry of the event log.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Event {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub subject: Option<String>,
    pub payload: serde_json::Value,
}

pub struct Store {
    conn: Connection,
}

pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---- column helpers: a bad stored value becomes a conversion error ----------

fn enum_col<T>(row: &Row<'_>, name: &str) -> rusqlite::Result<T>
where
    T: FromStr<Err = String>,
{
    let text: String = row.get(name)?;
    text.parse::<T>().map_err(|e| conversion_error(name, e))
}

fn json_col<T: DeserializeOwned>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    let text: String = row.get(name)?;
    serde_json::from_str(&text).map_err(|e| conversion_error(name, e))
}

fn time_col(row: &Row<'_>, name: &str) -> rusqlite::Result<DateTime<Utc>> {
    let text: String = row.get(name)?;
    parse_time(&text).map_err(|e| conversion_error(name, e))
}

fn opt_time_col(row: &Row<'_>, name: &str) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let text: Option<String> = row.get(name)?;
    text.map(|t| parse_time(&t).map_err(|e| conversion_error(name, e)))
        .transpose()
}

fn parse_time(text: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(text).map(|t| t.with_timezone(&Utc))
}

fn conversion_error(name: &str, err: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, Type::Text, format!("column {name}: {err}").into())
}

/// Runs a query and collects every row. Keeping the statement inside this
/// function avoids returning a borrow of a local statement.
fn query_all<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, f)?
        .collect::<rusqlite::Result<Vec<T>>>()?;
    Ok(rows)
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("plain data always serializes")
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory database, for tests.
    pub fn in_memory() -> Result<Store> {
        let store = Store {
            conn: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            self.conn.execute_batch(SCHEMA_V1)?;
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            return Err(StoreError::Corrupt(format!(
                "database schema version {version} is not supported (expected {SCHEMA_VERSION})"
            )));
        }
        Ok(())
    }

    // ---- counters and meta -------------------------------------------------

    /// Next identifier for a kind, such as `T-0007` for kind `T`.
    pub fn next_id(&self, kind: &str) -> Result<String> {
        let n = self.next_counter(kind)?;
        Ok(format!("{kind}-{n:04}"))
    }

    pub fn next_counter(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO counters (kind, value) VALUES (?1, 1)
             ON CONFLICT(kind) DO UPDATE SET value = value + 1",
            params![name],
        )?;
        Ok(self.conn.query_row(
            "SELECT value FROM counters WHERE kind = ?1",
            params![name],
            |r| r.get(0),
        )?)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- directives --------------------------------------------------------

    pub fn save_directive(&self, d: &Directive) -> Result<()> {
        self.conn.execute(
            "INSERT INTO directives (id, text, autonomy, state, planner_attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET text = excluded.text, autonomy = excluded.autonomy,
                 state = excluded.state, planner_attempts = excluded.planner_attempts,
                 updated_at = excluded.updated_at",
            params![
                d.id,
                d.text,
                d.autonomy.as_str(),
                d.state.as_str(),
                d.planner_attempts,
                d.created_at,
                d.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_directive(&self, id: &str) -> Result<Option<Directive>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM directives WHERE id = ?1",
                params![id],
                directive_row,
            )
            .optional()?)
    }

    pub fn list_directives(&self) -> Result<Vec<Directive>> {
        query_all(
            &self.conn,
            "SELECT * FROM directives ORDER BY id",
            &[],
            directive_row,
        )
    }

    // ---- tasks -------------------------------------------------------------

    pub fn save_task(&self, t: &Task) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tasks (id, directive_id, title, brief, scope, depends_on, acceptance, state,
                 autonomy, branch, worktree, worker_provider, runs_used, max_runs, notes, queue_seq,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, brief = excluded.brief,
                 scope = excluded.scope, depends_on = excluded.depends_on, acceptance = excluded.acceptance,
                 state = excluded.state, autonomy = excluded.autonomy, branch = excluded.branch,
                 worktree = excluded.worktree, worker_provider = excluded.worker_provider,
                 runs_used = excluded.runs_used, max_runs = excluded.max_runs, notes = excluded.notes,
                 queue_seq = excluded.queue_seq, updated_at = excluded.updated_at",
            params![
                t.id,
                t.directive_id,
                t.title,
                t.brief,
                to_json(&t.scope),
                to_json(&t.depends_on),
                to_json(&t.acceptance),
                t.state.as_str(),
                t.autonomy.as_str(),
                t.branch,
                t.worktree,
                t.worker_provider,
                t.runs_used,
                t.max_runs,
                to_json(&t.notes),
                t.queue_seq,
                t.created_at,
                t.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM tasks WHERE id = ?1", params![id], task_row)
            .optional()?)
    }

    pub fn list_tasks(&self, directive: Option<&str>) -> Result<Vec<Task>> {
        match directive {
            Some(d) => query_all(
                &self.conn,
                "SELECT * FROM tasks WHERE directive_id = ?1 ORDER BY id",
                params![d],
                task_row,
            ),
            None => query_all(&self.conn, "SELECT * FROM tasks ORDER BY id", &[], task_row),
        }
    }

    /// Tasks in a given state, in creation order.
    pub fn tasks_in_state(&self, state: TaskState) -> Result<Vec<Task>> {
        query_all(
            &self.conn,
            "SELECT * FROM tasks WHERE state = ?1 ORDER BY id",
            params![state.as_str()],
            task_row,
        )
    }

    // ---- runs --------------------------------------------------------------

    pub fn save_run(&self, r: &Run) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (id, task_id, directive_id, role, provider, state, pid, started_at,
                 ended_at, exit_code, log_path, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, pid = excluded.pid,
                 ended_at = excluded.ended_at, exit_code = excluded.exit_code, reason = excluded.reason",
            params![
                r.id,
                r.task_id,
                r.directive_id,
                r.role.as_str(),
                r.provider,
                r.state.as_str(),
                r.pid,
                r.started_at,
                r.ended_at,
                r.exit_code,
                r.log_path,
                r.reason
            ],
        )?;
        Ok(())
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM runs WHERE id = ?1", params![id], run_row)
            .optional()?)
    }

    /// Most recent runs first, optionally for one task.
    pub fn list_runs(&self, task: Option<&str>, limit: usize) -> Result<Vec<Run>> {
        match task {
            Some(t) => query_all(
                &self.conn,
                "SELECT * FROM runs WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
                params![t, limit as i64],
                run_row,
            ),
            None => query_all(
                &self.conn,
                "SELECT * FROM runs ORDER BY id DESC LIMIT ?1",
                params![limit as i64],
                run_row,
            ),
        }
    }

    pub fn runs_in_state(&self, state: RunState) -> Result<Vec<Run>> {
        query_all(
            &self.conn,
            "SELECT * FROM runs WHERE state = ?1 ORDER BY id",
            params![state.as_str()],
            run_row,
        )
    }

    pub fn count_runs(&self, task: &str, role: Role, state: RunState) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND role = ?2 AND state = ?3",
            params![task, role.as_str(), state.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    // ---- questions ---------------------------------------------------------

    pub fn insert_question(&self, q: &Question) -> Result<()> {
        self.conn.execute(
            "INSERT INTO questions (id, task_id, text, answer, created_at, answered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                q.id,
                q.task_id,
                q.text,
                q.answer,
                q.created_at,
                q.answered_at
            ],
        )?;
        Ok(())
    }

    pub fn save_question(&self, q: &Question) -> Result<()> {
        self.conn.execute(
            "UPDATE questions SET answer = ?2, answered_at = ?3 WHERE id = ?1",
            params![q.id, q.answer, q.answered_at],
        )?;
        Ok(())
    }

    pub fn get_question(&self, id: &str) -> Result<Option<Question>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM questions WHERE id = ?1",
                params![id],
                question_row,
            )
            .optional()?)
    }

    pub fn questions_for(&self, task: &str) -> Result<Vec<Question>> {
        query_all(
            &self.conn,
            "SELECT * FROM questions WHERE task_id = ?1 ORDER BY id",
            params![task],
            question_row,
        )
    }

    pub fn open_questions(&self) -> Result<Vec<Question>> {
        query_all(
            &self.conn,
            "SELECT * FROM questions WHERE answer IS NULL ORDER BY id",
            &[],
            question_row,
        )
    }

    // ---- quota -------------------------------------------------------------

    pub fn save_quota(&self, q: &ProviderQuota) -> Result<()> {
        self.conn.execute(
            "INSERT INTO quotas (provider, state, retry_at, source, backoff_minutes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(provider) DO UPDATE SET state = excluded.state, retry_at = excluded.retry_at,
                 source = excluded.source, backoff_minutes = excluded.backoff_minutes,
                 updated_at = excluded.updated_at",
            params![
                q.provider,
                q.state.as_str(),
                q.retry_at.map(ts),
                q.source,
                q.backoff_minutes,
                ts(q.updated_at)
            ],
        )?;
        Ok(())
    }

    pub fn get_quota(&self, provider: &str) -> Result<Option<ProviderQuota>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM quotas WHERE provider = ?1",
                params![provider],
                quota_row,
            )
            .optional()?)
    }

    pub fn list_quotas(&self) -> Result<Vec<ProviderQuota>> {
        query_all(
            &self.conn,
            "SELECT * FROM quotas ORDER BY provider",
            &[],
            quota_row,
        )
    }

    // ---- events ------------------------------------------------------------

    pub fn append_event(
        &self,
        kind: &str,
        subject: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (ts, kind, subject, payload) VALUES (?1, ?2, ?3, ?4)",
            params![ts(Utc::now()), kind, subject, payload.to_string()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Events after `cursor` (exclusive), oldest first. A client keeps the last
    /// `seq` it saw and asks for the rest after a reconnect.
    pub fn events_since(&self, cursor: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, kind, subject, payload FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cursor, limit as i64], |r| {
            let payload: String = r.get("payload")?;
            Ok(Event {
                seq: r.get("seq")?,
                ts: r.get("ts")?,
                kind: r.get("kind")?,
                subject: r.get("subject")?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

// ---- row mappers ----------------------------------------------------------

fn directive_row(r: &Row<'_>) -> rusqlite::Result<Directive> {
    Ok(Directive {
        id: r.get("id")?,
        text: r.get("text")?,
        autonomy: enum_col::<Autonomy>(r, "autonomy")?,
        state: enum_col::<DirectiveState>(r, "state")?,
        planner_attempts: r.get("planner_attempts")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn task_row(r: &Row<'_>) -> rusqlite::Result<Task> {
    let scope: Scope = json_col(r, "scope")?;
    Ok(Task {
        id: r.get("id")?,
        directive_id: r.get("directive_id")?,
        title: r.get("title")?,
        brief: r.get("brief")?,
        scope,
        depends_on: json_col(r, "depends_on")?,
        acceptance: json_col(r, "acceptance")?,
        state: enum_col::<TaskState>(r, "state")?,
        autonomy: enum_col::<Autonomy>(r, "autonomy")?,
        branch: r.get("branch")?,
        worktree: r.get("worktree")?,
        worker_provider: r.get("worker_provider")?,
        runs_used: r.get("runs_used")?,
        max_runs: r.get("max_runs")?,
        notes: json_col(r, "notes")?,
        queue_seq: r.get("queue_seq")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get("id")?,
        task_id: r.get("task_id")?,
        directive_id: r.get("directive_id")?,
        role: enum_col::<Role>(r, "role")?,
        provider: r.get("provider")?,
        state: enum_col::<RunState>(r, "state")?,
        pid: r.get("pid")?,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        exit_code: r.get("exit_code")?,
        log_path: r.get("log_path")?,
        reason: r.get("reason")?,
    })
}

fn question_row(r: &Row<'_>) -> rusqlite::Result<Question> {
    Ok(Question {
        id: r.get("id")?,
        task_id: r.get("task_id")?,
        text: r.get("text")?,
        answer: r.get("answer")?,
        created_at: r.get("created_at")?,
        answered_at: r.get("answered_at")?,
    })
}

fn quota_row(r: &Row<'_>) -> rusqlite::Result<ProviderQuota> {
    Ok(ProviderQuota {
        provider: r.get("provider")?,
        state: enum_col::<QuotaState>(r, "state")?,
        retry_at: opt_time_col(r, "retry_at")?,
        source: r.get("source")?,
        backoff_minutes: r.get("backoff_minutes")?,
        updated_at: time_col(r, "updated_at")?,
    })
}

const SCHEMA_V1: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE counters (kind TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE directives (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    autonomy TEXT NOT NULL,
    state TEXT NOT NULL,
    planner_attempts INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    directive_id TEXT,
    title TEXT NOT NULL,
    brief TEXT NOT NULL,
    scope TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    acceptance TEXT NOT NULL,
    state TEXT NOT NULL,
    autonomy TEXT NOT NULL,
    branch TEXT NOT NULL,
    worktree TEXT,
    worker_provider TEXT,
    runs_used INTEGER NOT NULL,
    max_runs INTEGER NOT NULL,
    notes TEXT NOT NULL,
    queue_seq INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX tasks_by_directive ON tasks(directive_id);
CREATE INDEX tasks_by_state ON tasks(state);
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    task_id TEXT,
    directive_id TEXT,
    role TEXT NOT NULL,
    provider TEXT NOT NULL,
    state TEXT NOT NULL,
    pid INTEGER,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    exit_code INTEGER,
    log_path TEXT NOT NULL,
    reason TEXT
);
CREATE INDEX runs_by_task ON runs(task_id);
CREATE INDEX runs_by_state ON runs(state);
CREATE TABLE questions (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    text TEXT NOT NULL,
    answer TEXT,
    created_at TEXT NOT NULL,
    answered_at TEXT
);
CREATE TABLE quotas (
    provider TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    retry_at TEXT,
    source TEXT,
    backoff_minutes INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT,
    payload TEXT NOT NULL
);
";

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::model::Autonomy;
    use chrono::TimeZone;

    fn sample_task(id: &str) -> Task {
        Task {
            id: id.into(),
            directive_id: Some("D-0001".into()),
            title: "t".into(),
            brief: "b".into(),
            scope: Scope {
                write: vec!["src/**".into()],
                read: vec![],
            },
            depends_on: vec![],
            acceptance: vec!["cargo test".into()],
            state: TaskState::Queued,
            autonomy: Autonomy::Auto,
            branch: format!("aura/{id}"),
            worktree: None,
            worker_provider: None,
            runs_used: 0,
            max_runs: 3,
            notes: vec!["n1".into()],
            queue_seq: Some(7),
            created_at: "2026-10-19T10:00:00.000Z".into(),
            updated_at: "2026-10-19T10:00:00.000Z".into(),
        }
    }

    #[test]
    fn ids_increment_per_kind() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.next_id("T").unwrap(), "T-0001");
        assert_eq!(s.next_id("T").unwrap(), "T-0002");
        assert_eq!(s.next_id("D").unwrap(), "D-0001");
    }

    #[test]
    fn task_round_trips() {
        let s = Store::in_memory().unwrap();
        let t = sample_task("T-0001");
        s.save_task(&t).unwrap();
        assert_eq!(s.get_task("T-0001").unwrap(), Some(t));
        assert_eq!(s.tasks_in_state(TaskState::Queued).unwrap().len(), 1);
    }

    #[test]
    fn quota_round_trips_with_timestamps() {
        let s = Store::in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 10, 0, 0).unwrap();
        let q = ProviderQuota {
            provider: "codex".into(),
            state: QuotaState::Limited,
            retry_at: Some(now + chrono::Duration::hours(2)),
            source: Some("message".into()),
            backoff_minutes: 0,
            updated_at: now,
        };
        s.save_quota(&q).unwrap();
        assert_eq!(s.get_quota("codex").unwrap(), Some(q));
    }

    #[test]
    fn events_are_ordered_and_resumable() {
        let s = Store::in_memory().unwrap();
        let a = s.append_event("one", None, serde_json::json!({})).unwrap();
        s.append_event("two", Some("T-1"), serde_json::json!({"x": 1}))
            .unwrap();
        let all = s.events_since(0, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].subject.as_deref(), Some("T-1"));
        let rest = s.events_since(a, 10).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].kind, "two");
    }

    #[test]
    fn corrupt_enum_is_reported_not_panicking() {
        let s = Store::in_memory().unwrap();
        s.conn
            .execute(
                "INSERT INTO tasks (id, directive_id, title, brief, scope, depends_on, acceptance, state,
                     autonomy, branch, worktree, worker_provider, runs_used, max_runs, notes, queue_seq,
                     created_at, updated_at)
                 VALUES ('T-9', NULL, 't', 'b', '{}', '[]', '[]', 'bogus', 'auto', 'b', NULL, NULL, 0, 3,
                     '[]', NULL, 'x', 'x')",
                [],
            )
            .unwrap();
        assert!(s.get_task("T-9").is_err());
    }

    #[test]
    fn count_runs_filters_by_role_and_state() {
        let s = Store::in_memory().unwrap();
        let run = |id: &str, state: RunState| Run {
            id: id.into(),
            task_id: Some("T-1".into()),
            directive_id: None,
            role: Role::Worker,
            provider: "codex".into(),
            state,
            pid: None,
            started_at: "x".into(),
            ended_at: None,
            exit_code: None,
            log_path: "/dev/null".into(),
            reason: None,
        };
        s.save_run(&run("R-1", RunState::Failed)).unwrap();
        s.save_run(&run("R-2", RunState::Succeeded)).unwrap();
        assert_eq!(
            s.count_runs("T-1", Role::Worker, RunState::Failed).unwrap(),
            1
        );
        assert_eq!(
            s.count_runs("T-1", Role::Reviewer, RunState::Failed)
                .unwrap(),
            0
        );
    }
}
