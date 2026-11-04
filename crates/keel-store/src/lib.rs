//! Local SQLite store: projects, runs, attempts and the audit log.
//!
//! `event` is append-only, and an attempt's identifying facts can't change after it starts.
//! Triggers enforce both, so a bug in the app can't rewrite history. This isn't protection
//! against someone who owns the machine: the raw stream file and its SHA-256 back it up.

use std::path::Path;
use std::sync::Mutex;

use keel_core::{AttemptState, Reason};
pub use rusqlite::types as rusqlite_value;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Applied in order; `PRAGMA user_version` records how many ran. Only ever append.
const MIGRATIONS: &[&str] = &[r#"
    create table setting (key text primary key, value text not null);

    create table project (
        id text primary key,
        name text not null,
        path text not null unique,
        created_at text not null
    );

    create table run (
        id text primary key,
        project_id text not null,
        title text not null,
        base_ref text,
        created_at text not null
    );

    create table attempt (
        id text primary key,
        run_id text not null,
        step_id text not null,
        number integer not null,
        state text not null,
        reason text,
        model_requested text not null,
        model_actual text,
        effort text,
        tools text not null,
        max_turns integer,
        max_budget_usd real,
        language text not null,
        cli_version text,
        api_key_source text,
        account text,
        prompt text not null,
        system_prompt_sources text not null,
        settings text not null,
        session_id text not null,
        resumed_from text,
        pid integer,
        pid_started text,
        workspace text not null,
        raw_path text not null,
        raw_offset integer not null default 0,
        raw_sha256 text,
        diff_path text,
        sha_before text,
        sha_after text,
        files_changed text,
        result_text text,
        cost_usd real,
        input_tokens integer,
        output_tokens integer,
        turns integer,
        duration_ms integer,
        started_at text not null,
        finished_at text,
        last_activity_at text
    );
    create index attempt_by_run on attempt (run_id, number);
    create index attempt_by_state on attempt (state);

    create table event (
        seq integer primary key autoincrement,
        attempt_id text not null,
        at text not null,
        kind text not null,
        data text not null
    );
    create index event_by_attempt on event (attempt_id, seq);

    create trigger event_no_update before update on event
    begin select raise(abort, 'audit events are append-only'); end;
    create trigger event_no_delete before delete on event
    begin select raise(abort, 'audit events are append-only'); end;

    create trigger attempt_facts_fixed before update of
        run_id, step_id, number, model_requested, effort, tools, prompt, settings, session_id,
        resumed_from, workspace, raw_path, started_at on attempt
    begin select raise(abort, 'attempt facts are fixed once it starts'); end;
    create trigger attempt_no_delete before delete on attempt
    begin select raise(abort, 'attempts are never deleted'); end;
"#];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewAttempt {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub number: i64,
    pub state: AttemptState,
    pub model_requested: String,
    pub effort: Option<String>,
    pub tools: String,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub language: String,
    pub prompt: String,
    pub system_prompt_sources: String,
    pub settings: String,
    pub session_id: String,
    pub resumed_from: Option<String>,
    pub workspace: String,
    pub raw_path: String,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub number: i64,
    pub state: String,
    pub reason: Option<serde_json::Value>,
    pub model_requested: String,
    pub model_actual: Option<String>,
    pub effort: Option<String>,
    pub tools: String,
    pub max_turns: Option<i64>,
    pub max_budget_usd: Option<f64>,
    pub language: String,
    pub cli_version: Option<String>,
    pub api_key_source: Option<String>,
    pub account: Option<String>,
    pub prompt: String,
    pub system_prompt_sources: serde_json::Value,
    pub settings: serde_json::Value,
    pub session_id: String,
    pub resumed_from: Option<String>,
    pub pid: Option<i64>,
    pub pid_started: Option<String>,
    pub workspace: String,
    pub raw_path: String,
    pub raw_offset: i64,
    pub raw_sha256: Option<String>,
    pub diff_path: Option<String>,
    pub sha_before: Option<String>,
    pub sha_after: Option<String>,
    pub files_changed: Option<serde_json::Value>,
    pub result_text: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub turns: Option<i64>,
    pub duration_ms: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: i64,
    pub attempt_id: String,
    pub at: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// One row of the home table: a run and its latest attempt.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: String,
    pub title: String,
    pub project_id: String,
    pub project_name: String,
    pub step_id: String,
    pub attempt_id: String,
    pub attempt_number: i64,
    pub state: String,
    pub reason: Option<serde_json::Value>,
    pub model: String,
    pub turns: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub last_activity_at: Option<String>,
}

/// The numbers on the home cards.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub running: i64,
    pub retrying: i64,
    pub blocked: i64,
    pub tokens: i64,
    pub duration_ms: i64,
    pub cost_usd: f64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn json_col(row: &Row, name: &str) -> rusqlite::Result<Option<serde_json::Value>> {
    let text: Option<String> = row.get(name)?;
    Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
}

fn attempt_from(row: &Row) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        step_id: row.get("step_id")?,
        number: row.get("number")?,
        state: row.get("state")?,
        reason: json_col(row, "reason")?,
        model_requested: row.get("model_requested")?,
        model_actual: row.get("model_actual")?,
        effort: row.get("effort")?,
        tools: row.get("tools")?,
        max_turns: row.get("max_turns")?,
        max_budget_usd: row.get("max_budget_usd")?,
        language: row.get("language")?,
        cli_version: row.get("cli_version")?,
        api_key_source: row.get("api_key_source")?,
        account: row.get("account")?,
        prompt: row.get("prompt")?,
        system_prompt_sources: json_col(row, "system_prompt_sources")?.unwrap_or_default(),
        settings: json_col(row, "settings")?.unwrap_or_default(),
        session_id: row.get("session_id")?,
        resumed_from: row.get("resumed_from")?,
        pid: row.get("pid")?,
        pid_started: row.get("pid_started")?,
        workspace: row.get("workspace")?,
        raw_path: row.get("raw_path")?,
        raw_offset: row.get("raw_offset")?,
        raw_sha256: row.get("raw_sha256")?,
        diff_path: row.get("diff_path")?,
        sha_before: row.get("sha_before")?,
        sha_after: row.get("sha_after")?,
        files_changed: json_col(row, "files_changed")?,
        result_text: row.get("result_text")?,
        cost_usd: row.get("cost_usd")?,
        input_tokens: row.get("input_tokens")?,
        output_tokens: row.get("output_tokens")?,
        turns: row.get("turns")?,
        duration_ms: row.get("duration_ms")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        last_activity_at: row.get("last_activity_at")?,
    })
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("pragma user_version", [], |r| r.get(0))?;
        let tx = conn.transaction()?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("select value from setting where key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "insert into setting (key, value) values (?1, ?2) on conflict (key) do update set value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn add_project(&self, project: &Project) -> Result<()> {
        self.lock().execute(
            "insert into project (id, name, path, created_at) values (?1, ?2, ?3, ?4)",
            params![project.id, project.name, project.path, project.created_at],
        )?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("select id, name, path, created_at from project order by name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: &str) -> Result<Project> {
        self.lock()
            .query_row(
                "select id, name, path, created_at from project where id = ?1",
                [id],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        path: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("project {id}")))
    }

    pub fn project_by_path(&self, path: &str) -> Result<Option<Project>> {
        Ok(self
            .lock()
            .query_row(
                "select id, name, path, created_at from project where path = ?1",
                [path],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        path: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn add_run(
        &self,
        id: &str,
        project_id: &str,
        title: &str,
        base_ref: Option<&str>,
    ) -> Result<()> {
        self.lock().execute(
            "insert into run (id, project_id, title, base_ref, created_at) values (?1, ?2, ?3, ?4, ?5)",
            params![id, project_id, title, base_ref, now()],
        )?;
        Ok(())
    }

    pub fn project_of_run(&self, run_id: &str) -> Result<String> {
        self.lock()
            .query_row("select project_id from run where id = ?1", [run_id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("run {run_id}")))
    }

    pub fn add_attempt(&self, a: &NewAttempt) -> Result<()> {
        self.lock().execute(
            "insert into attempt (id, run_id, step_id, number, state, model_requested, effort, tools, max_turns,
                max_budget_usd, language, prompt, system_prompt_sources, settings, session_id, resumed_from,
                workspace, raw_path, started_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                a.id,
                a.run_id,
                a.step_id,
                a.number,
                a.state.as_str(),
                a.model_requested,
                a.effort,
                a.tools,
                a.max_turns,
                a.max_budget_usd,
                a.language,
                a.prompt,
                a.system_prompt_sources,
                a.settings,
                a.session_id,
                a.resumed_from,
                a.workspace,
                a.raw_path,
                a.started_at
            ],
        )?;
        Ok(())
    }

    pub fn set_state(&self, id: &str, state: AttemptState, reason: Option<&Reason>) -> Result<()> {
        let reason = reason.map(|r| serde_json::to_string(r).unwrap_or_default());
        self.lock().execute(
            "update attempt set state = ?2, reason = ?3 where id = ?1",
            params![id, state.as_str(), reason],
        )?;
        Ok(())
    }

    /// Updates progress columns. Column names come from this crate's callers, never from data.
    pub fn patch_attempt(&self, id: &str, fields: &[(&'static str, Value)]) -> Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let assignments: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (col, _))| format!("{col} = ?{}", i + 2))
            .collect();
        let sql = format!(
            "update attempt set {} where id = ?1",
            assignments.join(", ")
        );
        let mut values = vec![Value::Text(id.to_owned())];
        values.extend(fields.iter().map(|(_, v)| v.clone()));
        self.lock().execute(&sql, params_from_iter(values))?;
        Ok(())
    }

    pub fn attempt(&self, id: &str) -> Result<Attempt> {
        self.lock()
            .query_row("select * from attempt where id = ?1", [id], attempt_from)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("attempt {id}")))
    }

    pub fn attempts_of_run(&self, run_id: &str) -> Result<Vec<Attempt>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("select * from attempt where run_id = ?1 order by number")?;
        let rows = stmt.query_map([run_id], attempt_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn active_attempts(&self) -> Result<Vec<Attempt>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select * from attempt where state in ('queued', 'preparing', 'running', 'stalled') order by started_at",
        )?;
        let rows = stmt.query_map([], attempt_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn append_event(
        &self,
        attempt_id: &str,
        kind: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "insert into event (attempt_id, at, kind, data) values (?1, ?2, ?3, ?4)",
            params![attempt_id, now(), kind, data.to_string()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn events(&self, attempt_id: &str, after_seq: i64) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select seq, attempt_id, at, kind, data from event where attempt_id = ?1 and seq > ?2 order by seq",
        )?;
        let rows = stmt.query_map(params![attempt_id, after_seq], |r| {
            let data: String = r.get(4)?;
            Ok(Event {
                seq: r.get(0)?,
                attempt_id: r.get(1)?,
                at: r.get(2)?,
                kind: r.get(3)?,
                data: serde_json::from_str(&data).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Runs, newest first, each with its latest attempt.
    pub fn runs(&self, limit: i64) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "select r.id, r.title, r.project_id, p.name as project_name, a.step_id, a.id as attempt_id, a.number,
                    a.state, a.reason, coalesce(a.model_actual, a.model_requested) as model, a.turns,
                    a.started_at, a.finished_at, a.last_activity_at
             from run r
             join project p on p.id = r.project_id
             join attempt a on a.run_id = r.id
              and a.number = (select max(number) from attempt where run_id = r.id)
             order by r.created_at desc limit ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                title: r.get(1)?,
                project_id: r.get(2)?,
                project_name: r.get(3)?,
                step_id: r.get(4)?,
                attempt_id: r.get(5)?,
                attempt_number: r.get(6)?,
                state: r.get(7)?,
                reason: json_col(r, "reason")?,
                model: r.get(9)?,
                turns: r.get(10)?,
                started_at: r.get(11)?,
                finished_at: r.get(12)?,
                last_activity_at: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn totals(&self) -> Result<Totals> {
        Ok(self.lock().query_row(
            "select
                coalesce(sum(state in ('preparing', 'running') and number = 1), 0),
                coalesce(sum(state in ('preparing', 'running') and number > 1), 0),
                coalesce(sum(state in ('stalled', 'blocked', 'interrupted')), 0),
                coalesce(sum(coalesce(input_tokens, 0) + coalesce(output_tokens, 0)), 0),
                coalesce(sum(coalesce(duration_ms, 0)), 0),
                coalesce(sum(coalesce(cost_usd, 0)), 0)
             from attempt",
            [],
            |r| {
                Ok(Totals {
                    running: r.get(0)?,
                    retrying: r.get(1)?,
                    blocked: r.get(2)?,
                    tokens: r.get(3)?,
                    duration_ms: r.get(4)?,
                    cost_usd: r.get(5)?,
                })
            },
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("keel.db")).unwrap();
        (dir, store)
    }

    fn seeded(store: &Store) -> String {
        store
            .add_project(&Project {
                id: "p".into(),
                name: "demo".into(),
                path: "/demo".into(),
                created_at: now(),
            })
            .unwrap();
        store.add_run("r", "p", "fix it", Some("main")).unwrap();
        store
            .add_attempt(&NewAttempt {
                id: "a".into(),
                run_id: "r".into(),
                step_id: "builder".into(),
                number: 1,
                state: AttemptState::Running,
                model_requested: "haiku".into(),
                effort: None,
                tools: "build".into(),
                max_turns: None,
                max_budget_usd: None,
                language: "en".into(),
                prompt: "do it".into(),
                system_prompt_sources: "[]".into(),
                settings: "{}".into(),
                session_id: "s".into(),
                resumed_from: None,
                workspace: "/w".into(),
                raw_path: "/w.jsonl".into(),
                started_at: now(),
            })
            .unwrap();
        "a".into()
    }

    #[test]
    fn events_cannot_be_edited_or_deleted() {
        let (_dir, store) = store();
        let id = seeded(&store);
        let seq = store
            .append_event(&id, "note", &serde_json::json!({"a": 1}))
            .unwrap();
        let conn = store.lock();
        let updated = conn.execute("update event set kind = 'x' where seq = ?1", [seq]);
        assert!(updated.unwrap_err().to_string().contains("append-only"));
        let deleted = conn.execute("delete from event where seq = ?1", [seq]);
        assert!(deleted.unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn attempt_facts_are_fixed_but_progress_is_not() {
        let (_dir, store) = store();
        let id = seeded(&store);
        let conn = store.lock();
        for sql in [
            "update attempt set prompt = 'other' where id = 'a'",
            "update attempt set model_requested = 'opus' where id = 'a'",
            "update attempt set settings = '{}' where id = 'a'",
            "delete from attempt where id = 'a'",
        ] {
            assert!(conn.execute(sql, []).is_err(), "{sql} should be refused");
        }
        drop(conn);
        store
            .patch_attempt(&id, &[("turns", Value::Integer(3))])
            .unwrap();
        store.set_state(&id, AttemptState::Succeeded, None).unwrap();
        let attempt = store.attempt(&id).unwrap();
        assert_eq!(attempt.turns, Some(3));
        assert_eq!(attempt.state, "succeeded");
    }

    #[test]
    fn home_table_and_cards_follow_the_latest_attempt() {
        let (_dir, store) = store();
        let id = seeded(&store);
        store
            .patch_attempt(
                &id,
                &[
                    ("input_tokens", Value::Integer(10)),
                    ("output_tokens", Value::Integer(5)),
                ],
            )
            .unwrap();
        let rows = store.runs(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "running");
        assert_eq!(rows[0].model, "haiku");
        let totals = store.totals().unwrap();
        assert_eq!((totals.running, totals.tokens), (1, 15));
    }

    #[test]
    fn reopening_keeps_data_and_does_not_rerun_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        {
            let store = Store::open(&path).unwrap();
            store.set_setting("language", "es").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.setting("language").unwrap().as_deref(), Some("es"));
    }
}
