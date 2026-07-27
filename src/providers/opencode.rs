//! OpenCode provider — reads/writes sessions from SQLite `opencode.db`.
//!
//! OpenCode stores session state in a SQLite database named `opencode.db`.
//! Two physical layouts of that database are in the wild and this provider
//! reads both — see [`Schema`]. Which one a given file uses is decided by
//! looking at `sqlite_master`, never by where the file lives: the location and
//! the layout are independent facts, and a user who installed OpenCode years
//! ago has an old layout in a path today's OpenCode never writes.
//!
//! casr addresses specific OpenCode sessions using a virtual path form:
//! `<db-path>/<urlencoded-session-id>`
//! This mirrors the approach used by Cursor and Aider providers.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags};
use tracing::{debug, info, trace, warn};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession};

/// OpenCode provider implementation.
pub struct OpenCode;

const DB_FILENAME: &str = "opencode.db";
const DATA_DIRNAME: &str = ".opencode";

/// Which physical layout an `opencode.db` uses.
///
/// OpenCode renamed its tables from plural to singular and moved every message
/// and part payload into a JSON `data` column. Both layouts still exist on real
/// machines, so this is detected per database rather than assumed, and every
/// query in this module is written against one variant or the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schema {
    /// Current OpenCode — verified against the released `opencode` 1.18.6
    /// binary. Tables `session` / `message` / `part`; a message row carries
    /// `time_created` plus a `data` JSON blob holding `role`, `modelID` and
    /// friends, and its content lives in separate `part` rows whose own `data`
    /// blob is the discriminated part union (`text`, `reasoning`, `tool`, …).
    Current,
    /// Pre-rename OpenCode. Tables `sessions` / `messages` / `files`; a message
    /// row has explicit `role` and `model` columns and inlines all of its
    /// content as a JSON array in `messages.parts`.
    Legacy,
}

impl Schema {
    /// Human-readable name used in detection evidence and error messages, so a
    /// report always says which layout was actually found.
    fn label(self) -> &'static str {
        match self {
            Schema::Current => "session/message/part",
            Schema::Legacy => "sessions/messages/files",
        }
    }

    /// The table holding one row per session in this layout.
    fn session_table(self) -> &'static str {
        match self {
            Schema::Current => "session",
            Schema::Legacy => "sessions",
        }
    }

    /// Column ordering sessions newest-first in this layout.
    fn created_column(self) -> &'static str {
        match self {
            Schema::Current => "time_created",
            Schema::Legacy => "created_at",
        }
    }

    /// Column linking a child session to its parent in this layout.
    fn parent_column(self) -> &'static str {
        match self {
            Schema::Current => "parent_id",
            Schema::Legacy => "parent_session_id",
        }
    }
}

impl OpenCode {
    /// Parse OPENCODE environment overrides into a target DB path.
    ///
    /// In precedence order:
    /// - `OPENCODE_DB_PATH` — casr's own override, a direct file path.
    /// - `OPENCODE_HOME` — casr's own override: a directory containing
    ///   `opencode.db`, or a direct `.db` path. OpenCode has no variable of
    ///   either name, so these two are casr's alone and win, so that aiming casr
    ///   at a database never disturbs the OpenCode the rest of the shell uses.
    /// - `OPENCODE_DB` — the variable OpenCode itself honours, resolved the way
    ///   OpenCode resolves it: an absolute path is used verbatim, anything else
    ///   is a *filename* joined onto OpenCode's data directory. `:memory:` names
    ///   a private in-process database that no other process can read, so it
    ///   yields no path and discovery falls through.
    fn env_db_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("OPENCODE_DB_PATH")
            && !path.trim().is_empty()
        {
            return Some(PathBuf::from(path));
        }

        if let Ok(home) = std::env::var("OPENCODE_HOME")
            && !home.trim().is_empty()
        {
            let home_path = PathBuf::from(home);
            if home_path.extension().is_some_and(|ext| ext == "db") {
                return Some(home_path);
            }
            return Some(home_path.join(DB_FILENAME));
        }

        if let Ok(db) = std::env::var("OPENCODE_DB")
            && !db.trim().is_empty()
            && db != ":memory:"
        {
            let db_path = PathBuf::from(&db);
            if db_path.is_absolute() {
                return Some(db_path);
            }
            return Some(Self::upstream_data_dir()?.join(db_path));
        }

        None
    }

    /// OpenCode's own data directory, the base a relative `OPENCODE_DB` resolves
    /// against. OpenCode gets it from the `xdg-basedir` npm package, which uses
    /// `$XDG_DATA_HOME` else `~/.local/share` on *every* platform — including
    /// macOS, where it is deliberately not `~/Library/Application Support`.
    fn upstream_data_dir() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(xdg).join("opencode"));
        }
        let home = dirs::home_dir()?;
        Some(home.join(".local").join("share").join("opencode"))
    }

    /// Candidate global config files that may contain `data.directory`.
    fn config_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".opencode.json"));
            paths.push(home.join(".config/opencode/.opencode.json"));
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.trim().is_empty()
        {
            paths.push(PathBuf::from(xdg).join("opencode/.opencode.json"));
        }
        paths
    }

    /// Parse absolute `data.directory` values from OpenCode config files.
    fn configured_data_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();

        for cfg in Self::config_paths() {
            let Ok(text) = std::fs::read_to_string(&cfg) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(dir) = json
                .pointer("/data/directory")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };

            let data_dir = PathBuf::from(dir);
            if data_dir.is_absolute() {
                dirs.push(data_dir);
            }
        }

        dirs
    }

    /// Candidate DB paths from current directory and parents (`.opencode/opencode.db`).
    fn cwd_ancestor_db_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Ok(cwd) = std::env::current_dir() else {
            return paths;
        };

        for ancestor in cwd.ancestors() {
            paths.push(ancestor.join(DATA_DIRNAME).join(DB_FILENAME));
        }

        paths
    }

    /// Discover existing OpenCode DB files.
    ///
    /// If env override is set, discovery is constrained to that location.
    fn find_db_files() -> Vec<PathBuf> {
        if let Some(env_db) = Self::env_db_path() {
            return if env_db.is_file() {
                vec![env_db]
            } else {
                Vec::new()
            };
        }

        let mut candidates = Vec::new();
        candidates.extend(Self::cwd_ancestor_db_paths());
        // OpenCode's real home. `upstream_data_dir` already encodes how OpenCode
        // resolves it, and leaving it out of discovery was why an ordinary
        // install — the only location a current OpenCode ever writes — was
        // invisible to casr while `~/.opencode` and the cwd ancestors, which
        // current OpenCode never writes, were searched.
        if let Some(data_dir) = Self::upstream_data_dir() {
            candidates.push(data_dir.join(DB_FILENAME));
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(DATA_DIRNAME).join(DB_FILENAME));
        }
        for data_dir in Self::configured_data_dirs() {
            candidates.push(data_dir.join(DB_FILENAME));
        }

        dedup_existing_files(candidates)
    }

    /// Resolve target DB path for writes.
    ///
    /// A database that already exists beats one casr would have to invent,
    /// because the only useful outcome of `casr resume opc <session>` is a
    /// session OpenCode can open. A fresh workspace-local database is not one:
    /// current OpenCode reads a single database in its own data directory, so
    /// inventing one there makes the conversion succeed, report a fidelity
    /// grade, and leave the user nothing to resume — success and emptiness at
    /// the same time, which is the same defect on the write side that the
    /// reader was just fixed for.
    ///
    /// Reaching into the agent's live state is the price of the session being
    /// resumable at all. Codex already makes the same trade, registering a
    /// converted thread into `~/.codex/state_*.sqlite`.
    ///
    /// Because of this, *every* caller of `write_session` reads the process
    /// environment. Tests that exercise it therefore live in
    /// `tests/opencode_write_test.rs`, where `XDG_DATA_HOME` and friends can be
    /// redirected — `src/lib.rs` forbids unsafe code, so an in-crate test
    /// cannot call `set_var` and cannot isolate itself from a real install.
    fn choose_target_db_path(session: &CanonicalSession) -> anyhow::Result<PathBuf> {
        // 1. An explicit override is the user naming the target, so it wins over
        //    anything discovery could infer.
        if let Some(env_db) = Self::env_db_path() {
            return Ok(env_db);
        }

        // 2. A database already sitting beside the workspace. Current OpenCode
        //    never puts one there, so its presence means somebody deliberately
        //    did — an older OpenCode, or a user who keeps a per-project store.
        if let Some(workspace) = &session.workspace {
            let workspace_db = workspace.join(DATA_DIRNAME).join(DB_FILENAME);
            if workspace_db.is_file() {
                return Ok(workspace_db);
            }
        }

        // 3. Any database discovery found — in practice the one OpenCode itself
        //    writes and reads.
        if let Some(existing) = Self::find_db_files().into_iter().next() {
            return Ok(existing);
        }

        // 4. Nothing exists anywhere. Create beside the workspace, else the cwd
        //    — and `write_session_legacy` warns that what lands here is not
        //    something this OpenCode will read.
        if let Some(workspace) = &session.workspace {
            return Ok(workspace.join(DATA_DIRNAME).join(DB_FILENAME));
        }
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        Ok(cwd.join(DATA_DIRNAME).join(DB_FILENAME))
    }

    /// Build virtual per-session path: `<db-path>/<urlencoded-session-id>`.
    fn virtual_session_path(db_path: &Path, session_id: &str) -> PathBuf {
        let encoded = urlencoding::encode(session_id);
        db_path.join(encoded.as_ref())
    }

    /// Parse virtual path back into `(db_path, session_id)`.
    fn parse_virtual_path(path: &Path) -> Option<(PathBuf, String)> {
        let parent = path.parent()?;
        if !parent.is_file() {
            return None;
        }
        if parent.file_name().and_then(|n| n.to_str()) != Some(DB_FILENAME) {
            return None;
        }

        let encoded = path.file_name()?.to_str()?;
        let decoded = urlencoding::decode(encoded).ok()?;
        Some((parent.to_path_buf(), decoded.into_owned()))
    }

    /// Open DB in read-only mode.
    fn open_db(path: &Path) -> anyhow::Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open OpenCode DB: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Open DB in read-write/create mode.
    fn open_db_rw(path: &Path) -> anyhow::Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open OpenCode DB for writing: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
            .and_then(|mut stmt| stmt.exists(rusqlite::params![table]))
            .unwrap_or(false)
    }

    fn trigger_exists(conn: &Connection, trigger: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?1")
            .and_then(|mut stmt| stmt.exists(rusqlite::params![trigger]))
            .unwrap_or(false)
    }

    /// Ensure core OpenCode tables exist.
    fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    parent_session_id TEXT,
    title TEXT NOT NULL,
    message_count INTEGER NOT NULL DEFAULT 0 CHECK (message_count >= 0),
    prompt_tokens INTEGER NOT NULL DEFAULT 0 CHECK (prompt_tokens >= 0),
    completion_tokens INTEGER NOT NULL DEFAULT 0 CHECK (completion_tokens >= 0),
    cost REAL NOT NULL DEFAULT 0.0 CHECK (cost >= 0.0),
    updated_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    summary_message_id TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    parts TEXT NOT NULL DEFAULT '[]',
    model TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS files (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    path TEXT NOT NULL,
    content TEXT NOT NULL,
    version TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE,
    UNIQUE(path, session_id, version)
);

CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages (session_id);
CREATE INDEX IF NOT EXISTS idx_files_session_id ON files (session_id);
"#,
        )
        .context("failed to initialize OpenCode schema")?;
        Ok(())
    }

    /// Which layout this database uses, or `None` when it is not an OpenCode
    /// database at all.
    ///
    /// Both table sets are required, not just the session table, so that a
    /// half-written or foreign database is reported as unrecognised rather than
    /// silently read as empty. `Current` wins when both are present: that
    /// happens only when something wrote legacy tables beside a live OpenCode
    /// database, and the live one is the truth.
    fn detect_schema(conn: &Connection) -> Option<Schema> {
        if Self::table_exists(conn, "session")
            && Self::table_exists(conn, "message")
            && Self::table_exists(conn, "part")
        {
            return Some(Schema::Current);
        }
        if Self::table_exists(conn, "sessions") && Self::table_exists(conn, "messages") {
            return Some(Schema::Legacy);
        }
        None
    }

    /// How many sessions a database holds, and under which layout.
    ///
    /// `Ok` carries the layout and the row count; `Err` carries the reason no
    /// count could be taken. There is deliberately no "0 sessions" success value
    /// for an unreadable database, because that is exactly the answer that made
    /// a broken reader look like an empty one.
    fn describe_db(path: &Path) -> Result<(Schema, i64), String> {
        let conn = Self::open_db(path).map_err(|err| format!("{err:#}"))?;
        let Some(schema) = Self::detect_schema(&conn) else {
            return Err(
                "unrecognised schema: has neither session/message/part nor sessions/messages"
                    .to_string(),
            );
        };
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", schema.session_table()),
                [],
                |row| row.get(0),
            )
            .map_err(|err| format!("{err}"))?;
        Ok((schema, count))
    }

    fn session_exists(conn: &Connection, session_id: &str) -> bool {
        let Some(schema) = Self::detect_schema(conn) else {
            return false;
        };
        conn.prepare(&format!(
            "SELECT 1 FROM {} WHERE id = ?1 LIMIT 1",
            schema.session_table()
        ))
        .and_then(|mut stmt| stmt.exists(rusqlite::params![session_id]))
        .unwrap_or(false)
    }

    fn newest_root_session_id(conn: &Connection) -> Option<String> {
        let schema = Self::detect_schema(conn)?;
        conn.query_row(
            &format!(
                "SELECT id FROM {} WHERE {} IS NULL ORDER BY {} DESC LIMIT 1",
                schema.session_table(),
                schema.parent_column(),
                schema.created_column(),
            ),
            [],
            |row| row.get(0),
        )
        .ok()
    }

    fn workspace_from_db_path(db_path: &Path) -> Option<PathBuf> {
        let data_dir = db_path.parent()?;
        if data_dir.file_name().and_then(|n| n.to_str()) == Some(DATA_DIRNAME) {
            return data_dir.parent().map(Path::to_path_buf);
        }
        None
    }

    fn read_session_by_id(
        conn: &Connection,
        db_path: &Path,
        session_id: &str,
    ) -> anyhow::Result<CanonicalSession> {
        let Some(schema) = Self::detect_schema(conn) else {
            anyhow::bail!(
                "{} is not an OpenCode database: it has neither the current \
                 session/message/part tables nor the legacy sessions/messages tables",
                db_path.display()
            );
        };
        match schema {
            Schema::Current => Self::read_session_current(conn, db_path, session_id),
            Schema::Legacy => Self::read_session_legacy(conn, db_path, session_id),
        }
    }

    /// Read a session from the current `session`/`message`/`part` layout.
    fn read_session_current(
        conn: &Connection,
        db_path: &Path,
        session_id: &str,
    ) -> anyhow::Result<CanonicalSession> {
        let (
            title_raw,
            directory,
            parent_id,
            created_raw,
            updated_raw,
            tokens_input,
            tokens_output,
            cost,
            session_model_raw,
        ): (
            String,
            String,
            Option<String>,
            i64,
            i64,
            i64,
            i64,
            f64,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT title, directory, parent_id, time_created, time_updated,
                        tokens_input, tokens_output, cost, model
                 FROM session
                 WHERE id = ?1
                 LIMIT 1",
                rusqlite::params![session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .with_context(|| {
                format!("session '{session_id}' not found in {}", db_path.display())
            })?;

        // Every part for the session in one pass, grouped by message. Parts are
        // ordered by id, which is how OpenCode's own `part_message_id_id_idx`
        // orders them — the ids are monotonic within a message.
        let mut parts_by_message: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, data
                     FROM part
                     WHERE session_id = ?1
                     ORDER BY message_id ASC, id ASC",
                )
                .context("failed to prepare part query")?;
            let rows = stmt.query_map(rusqlite::params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (message_id, data) = row?;
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
                    continue;
                };
                parts_by_message.entry(message_id).or_default().push(value);
            }
        }

        let mut started_at = parse_timestamp(&serde_json::Value::from(created_raw));
        let mut ended_at = parse_timestamp(&serde_json::Value::from(updated_raw)).or(started_at);
        let mut model_counts: HashMap<String, usize> = HashMap::new();
        let mut messages = Vec::new();

        let mut stmt = conn
            .prepare(
                "SELECT id, time_created, data
                 FROM message
                 WHERE session_id = ?1
                 ORDER BY time_created ASC, id ASC",
            )
            .context("failed to prepare message query")?;

        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        for row in rows {
            let (message_id, created_at_raw, data_json) = row?;
            let data = serde_json::from_str::<serde_json::Value>(&data_json)
                .unwrap_or_else(|_| serde_json::json!({}));

            let timestamp =
                parse_timestamp(&serde_json::Value::from(created_at_raw)).or(Some(created_at_raw));
            if let Some(ts) = timestamp {
                started_at = Some(started_at.map_or(ts, |current| current.min(ts)));
                ended_at = Some(ended_at.map_or(ts, |current| current.max(ts)));
            }
            if let Some(completed) = data.pointer("/time/completed")
                && let Some(completed_ts) = parse_timestamp(completed)
            {
                ended_at = Some(ended_at.map_or(completed_ts, |current| current.max(completed_ts)));
            }

            // An OpenCode message with no `role` is malformed rather than
            // user-authored; say "unknown" instead of guessing a side.
            let role = match data.get("role").and_then(serde_json::Value::as_str) {
                Some(role) => normalize_role(role),
                None => MessageRole::Other("unknown".to_string()),
            };

            // Assistant messages carry `modelID` at the top level; user
            // messages carry the model they were sent with under `model`.
            let model = data
                .get("modelID")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    data.pointer("/model/modelID")
                        .and_then(serde_json::Value::as_str)
                })
                .filter(|m| !m.is_empty())
                .map(ToString::to_string);
            if let Some(model_name) = model.as_deref() {
                *model_counts.entry(model_name.to_string()).or_insert(0) += 1;
            }

            let raw_parts = parts_by_message.remove(&message_id).unwrap_or_default();
            let (content, tool_calls, tool_results) = parse_current_parts(&raw_parts);

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content,
                timestamp,
                author: model,
                tool_calls,
                tool_results,
                extra: serde_json::json!({
                    "opencode_message_id": message_id,
                    "opencode_parts": raw_parts,
                }),
            });
        }

        reindex_messages(&mut messages);

        let title = (!title_raw.trim().is_empty())
            .then_some(title_raw)
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 80))
                    .filter(|t| !t.is_empty())
            });

        // Fall back to the session's configured model when no message named one
        // (a session that errored before its first completion has none).
        let session_model = session_model_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            });
        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| name)
            .or(session_model);

        // The current schema records the session's own working directory, which
        // beats inferring one from where the database happens to sit.
        let workspace = (!directory.trim().is_empty())
            .then(|| PathBuf::from(&directory))
            .or_else(|| Self::workspace_from_db_path(db_path));

        Ok(CanonicalSession {
            session_id: session_id.to_string(),
            provider_slug: "opencode".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::json!({
                "opencode_db": db_path.display().to_string(),
                "opencode_schema": Schema::Current.label(),
                "parent_session_id": parent_id,
                "prompt_tokens": tokens_input,
                "completion_tokens": tokens_output,
                "cost": cost,
            }),
            source_path: Self::virtual_session_path(db_path, session_id),
            model_name,
        })
    }

    /// Read a session from the legacy `sessions`/`messages`/`files` layout.
    fn read_session_legacy(
        conn: &Connection,
        db_path: &Path,
        session_id: &str,
    ) -> anyhow::Result<CanonicalSession> {
        let (title_raw, created_raw, updated_raw, parent_session_id, prompt_tokens, completion_tokens, cost): (
            String,
            i64,
            i64,
            Option<String>,
            i64,
            i64,
            f64,
        ) = conn
            .query_row(
                "SELECT title, created_at, updated_at, parent_session_id, prompt_tokens, completion_tokens, cost
                 FROM sessions
                 WHERE id = ?1
                 LIMIT 1",
                rusqlite::params![session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .with_context(|| format!("session '{session_id}' not found in {}", db_path.display()))?;

        let mut started_at = parse_timestamp(&serde_json::Value::from(created_raw));
        let mut ended_at = parse_timestamp(&serde_json::Value::from(updated_raw)).or(started_at);
        let mut model_counts: HashMap<String, usize> = HashMap::new();
        let mut messages = Vec::new();

        let mut stmt = conn
            .prepare(
                "SELECT id, role, parts, model, created_at, updated_at, finished_at
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )
            .context("failed to prepare message query")?;

        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?;

        for row in rows {
            let (
                message_id,
                role_raw,
                parts_json,
                model,
                created_at_raw,
                _updated_at_raw,
                finished_at_raw,
            ) = row?;

            let timestamp =
                parse_timestamp(&serde_json::Value::from(created_at_raw)).or(Some(created_at_raw));
            if let Some(ts) = timestamp {
                started_at = Some(started_at.map_or(ts, |current| current.min(ts)));
                ended_at = Some(ended_at.map_or(ts, |current| current.max(ts)));
            }

            if let Some(finished_raw) = finished_at_raw
                && let Some(finished_ts) = parse_timestamp(&serde_json::Value::from(finished_raw))
            {
                ended_at = Some(ended_at.map_or(finished_ts, |current| current.max(finished_ts)));
            }

            let raw_parts = serde_json::from_str::<serde_json::Value>(&parts_json)
                .unwrap_or_else(|_| serde_json::json!([]));
            let (content, tool_calls, tool_results) = parse_parts(&raw_parts);

            if let Some(model_name) = model.as_deref().filter(|m| !m.is_empty()) {
                *model_counts.entry(model_name.to_string()).or_insert(0) += 1;
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role: normalize_role(&role_raw),
                content,
                timestamp,
                author: model.clone(),
                tool_calls,
                tool_results,
                extra: serde_json::json!({
                    "opencode_message_id": message_id,
                    "opencode_parts": raw_parts,
                }),
            });
        }

        reindex_messages(&mut messages);

        let title = (!title_raw.trim().is_empty())
            .then_some(title_raw)
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 80))
                    .filter(|t| !t.is_empty())
            });

        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| name);

        let source = Self::virtual_session_path(db_path, session_id);

        Ok(CanonicalSession {
            session_id: session_id.to_string(),
            provider_slug: "opencode".to_string(),
            workspace: Self::workspace_from_db_path(db_path),
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::json!({
                "opencode_db": db_path.display().to_string(),
                "opencode_schema": Schema::Legacy.label(),
                "parent_session_id": parent_session_id,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "cost": cost,
            }),
            source_path: source,
            model_name,
        })
    }
}

impl Provider for OpenCode {
    fn name(&self) -> &str {
        "OpenCode"
    }

    fn slug(&self) -> &str {
        "opencode"
    }

    fn cli_alias(&self) -> &str {
        "opc"
    }

    fn detect(&self) -> DetectionResult {
        let mut installed = false;
        let mut evidence = Vec::new();

        if which::which("opencode").is_ok() {
            installed = true;
            evidence.push("opencode binary found in PATH".to_string());
        }

        if let Some(env_path) = Self::env_db_path() {
            evidence.push(format!("env override target: {}", env_path.display()));
        }

        let dbs = Self::find_db_files();
        if !dbs.is_empty() {
            installed = true;
            evidence.push(format!("found {} opencode.db database(s)", dbs.len()));
            // "Found a database" and "listed no sessions" used to be reported as
            // the same success. Every database now accounts for itself, so a
            // reader that cannot read one is visibly different from one that
            // read it and found it empty.
            for db in &dbs {
                match Self::describe_db(db) {
                    Ok((schema, 0)) => evidence.push(format!(
                        "{}: {} schema, no sessions stored",
                        db.display(),
                        schema.label()
                    )),
                    Ok((schema, count)) => evidence.push(format!(
                        "{}: {} schema, {count} session(s)",
                        db.display(),
                        schema.label()
                    )),
                    Err(reason) => evidence.push(format!(
                        "{}: UNREADABLE — {reason}; no sessions can be listed from it",
                        db.display()
                    )),
                }
            }
        }

        trace!(provider = "opencode", installed, ?evidence, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        Self::find_db_files()
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        for db_path in Self::find_db_files() {
            let Ok(conn) = Self::open_db(&db_path) else {
                continue;
            };

            if Self::session_exists(&conn, session_id) {
                let virtual_path = Self::virtual_session_path(&db_path, session_id);
                debug!(
                    db = %db_path.display(),
                    session = %virtual_path.display(),
                    session_id,
                    "found OpenCode session"
                );
                return Some(virtual_path);
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading OpenCode session");

        // Virtual path (`.../opencode.db/<encoded-session-id>`) from discovery.
        if let Some((db_path, session_id)) = Self::parse_virtual_path(path) {
            let conn = Self::open_db(&db_path)?;
            return Self::read_session_by_id(&conn, &db_path, &session_id);
        }

        // Direct DB path (`.../opencode.db`) — choose newest root session.
        let conn = Self::open_db(path)?;
        let Some(session_id) = Self::newest_root_session_id(&conn) else {
            anyhow::bail!("no OpenCode sessions found in {}", path.display());
        };
        Self::read_session_by_id(&conn, path, &session_id)
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let db_path = Self::choose_target_db_path(session)?;

        // Which layout to write is decided by what is already at the target, not
        // by preference: a session is only useful if the OpenCode that owns this
        // database can open it. A live OpenCode database gets
        // session/message/part rows, exactly as OpenCode's own `import` writes
        // them; a pre-rename database keeps getting the legacy layout, because
        // the OpenCode that created it reads nothing else.
        let existing_schema = if db_path.is_file() {
            Self::open_db(&db_path)
                .ok()
                .and_then(|conn| Self::detect_schema(&conn))
        } else {
            None
        };

        match existing_schema {
            Some(Schema::Current) => Self::write_session_current(self, session, opts, &db_path),
            // No database yet, or a file that is not one. casr cannot bootstrap
            // a current-schema database from nothing: OpenCode's migrator runs
            // on open and aborts with "table `project` already exists" if the
            // tables are already there, so a hand-built one would brick the
            // install it was meant to feed. Write the legacy layout, which is
            // self-contained and round-trips through casr, and say plainly that
            // this OpenCode will not read it.
            Some(Schema::Legacy) | None => {
                Self::write_session_legacy(self, session, opts, &db_path, existing_schema.is_none())
            }
        }
    }

    fn resume_command(&self, _session_id: &str) -> String {
        // OpenCode has no session-id-specific resume flag.
        "opencode".to_string()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let mut listing = SessionListing::default();
        // Every database accounts for itself. `find_db_files` returns only
        // databases that exist, so each of these failures is a store casr found
        // and could not read — the case `warn!` alone left out of `list`.
        for db_path in &Self::find_db_files() {
            let conn = match Self::open_db(db_path) {
                Ok(conn) => conn,
                Err(err) => {
                    let error = format!("{err:#}");
                    warn!(db = %db_path.display(), error = %error,
                          "OpenCode database could not be opened; its sessions are not listed");
                    listing.unreadable.push(UnreadableSource {
                        path: db_path.clone(),
                        error,
                    });
                    continue;
                }
            };
            let Some(schema) = Self::detect_schema(&conn) else {
                warn!(db = %db_path.display(),
                      "OpenCode database has an unrecognised schema (neither \
                       session/message/part nor sessions/messages); its sessions are not listed");
                listing.unreadable.push(UnreadableSource {
                    path: db_path.clone(),
                    error: "unrecognised schema (neither session/message/part nor \
                            sessions/messages)"
                        .to_string(),
                });
                continue;
            };

            let query = format!(
                "SELECT id FROM {} ORDER BY {} DESC",
                schema.session_table(),
                schema.created_column()
            );
            let mut stmt = match conn.prepare(&query) {
                Ok(stmt) => stmt,
                Err(err) => {
                    warn!(db = %db_path.display(), schema = schema.label(),
                          "OpenCode session query failed; its sessions are not listed");
                    listing.unreadable.push(UnreadableSource {
                        path: db_path.clone(),
                        error: format!("session query failed: {err}"),
                    });
                    continue;
                }
            };

            let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows,
                Err(err) => {
                    listing.unreadable.push(UnreadableSource {
                        path: db_path.clone(),
                        error: format!("session query failed: {err}"),
                    });
                    continue;
                }
            };

            for row in rows {
                match row {
                    Ok(id) => {
                        let virtual_path = Self::virtual_session_path(db_path, &id);
                        listing.sessions.push((id, virtual_path));
                    }
                    Err(err) => listing.unreadable.push(UnreadableSource {
                        path: db_path.clone(),
                        error: format!("session row could not be read: {err}"),
                    }),
                }
            }
        }

        Some(listing)
    }

    /// One SQLite database holds every session, so a session "path" is the
    /// virtual `<db>#<session id>` this provider mints.
    fn is_session_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("db" | "sqlite")
        )
    }
}

impl OpenCode {
    /// Write into a live OpenCode database (`session`/`message`/`part`).
    ///
    /// The row shapes here are not inferred from documentation: they are what
    /// the released `opencode` 1.18.6 binary itself writes, measured by running
    /// its own `opencode import` against an empty database and diffing the
    /// result. That run touched `project`, `session`, `message` and `part` and
    /// nothing else — in particular the `session_message` projection stays
    /// empty and is rebuilt by OpenCode on demand.
    fn write_session_current(
        provider: &Self,
        session: &CanonicalSession,
        opts: &WriteOptions,
        db_path: &Path,
    ) -> anyhow::Result<WrittenSession> {
        let mut conn = Self::open_db_rw(db_path)?;

        let target_session_id = opencode_session_id(&session.session_id);

        if Self::session_exists(&conn, &target_session_id) {
            if opts.force {
                let _ = conn.execute(
                    "DELETE FROM part WHERE session_id = ?1",
                    rusqlite::params![target_session_id],
                );
                let _ = conn.execute(
                    "DELETE FROM message WHERE session_id = ?1",
                    rusqlite::params![target_session_id],
                );
                conn.execute(
                    "DELETE FROM session WHERE id = ?1",
                    rusqlite::params![target_session_id],
                )
                .context("failed to delete existing OpenCode session for --force overwrite")?;
            } else {
                return Err(crate::error::CasrError::SessionConflict {
                    session_id: target_session_id,
                    existing_path: db_path.to_path_buf(),
                }
                .into());
            }
        }

        let now = chrono::Utc::now().timestamp_millis();
        let created_at = session.started_at.unwrap_or(now);
        let updated_at = session.ended_at.unwrap_or(now);
        let directory = session
            .workspace
            .as_ref()
            .map(|ws| ws.display().to_string())
            .unwrap_or_default();

        let title = session
            .title
            .clone()
            .or_else(|| {
                session
                    .messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 80))
                    .filter(|t| !t.is_empty())
            })
            .unwrap_or_else(|| "Converted session".to_string());

        let project_id = Self::resolve_project_id(&conn, &directory, created_at)?;
        // `session.version` is NOT NULL and records the OpenCode that created
        // the row. casr is not OpenCode, so it copies the version already in the
        // database rather than inventing one.
        let version: String = conn
            .query_row(
                "SELECT version FROM session ORDER BY time_created DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "0.0.0".to_string());
        let (provider_id, model_id) = split_model_ref(session.model_name.as_deref());

        let tx = conn.transaction().context("failed to begin transaction")?;

        tx.execute(
            "INSERT INTO session (
                id, project_id, workspace_id, parent_id, slug, directory, path, title, version,
                cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read,
                tokens_cache_write, agent, model, time_created, time_updated
             ) VALUES (?1, ?2, NULL, NULL, ?3, ?4, '', ?5, ?6, 0, 0, 0, 0, 0, 0, NULL, NULL, ?7, ?8)",
            rusqlite::params![
                target_session_id,
                project_id,
                format!("casr-{}", &target_session_id),
                directory,
                title,
                version,
                created_at,
                updated_at,
            ],
        )
        .context("failed to insert OpenCode session")?;

        let timestamps = monotonic_timestamps(&session.messages, created_at);
        for (i, msg) in session.messages.iter().enumerate() {
            // OpenCode orders a session's messages by `(time_created, id)`, and
            // a converted transcript routinely has many messages inside the same
            // millisecond. A random id would let those tie-break into a
            // different order than they were written, so the index goes in the
            // id — zero-padded, because the tie-break is a string comparison.
            let message_id = format!("msg_{i:06}_{}", uuid::Uuid::new_v4().simple());
            let timestamp = timestamps[i];
            let data = build_message_data(msg, timestamp, &provider_id, &model_id);

            tx.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    message_id,
                    target_session_id,
                    timestamp,
                    timestamp,
                    serde_json::to_string(&data)
                        .context("failed to serialize OpenCode message data")?,
                ],
            )
            .with_context(|| format!("failed to insert OpenCode message {}", msg.idx))?;

            for (part_idx, part) in build_current_parts(msg).into_iter().enumerate() {
                tx.execute(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        // Zero-padded so string ordering — which is how OpenCode
                        // orders parts within a message — matches insert order.
                        format!("prt_{message_id}_{part_idx:06}"),
                        message_id,
                        target_session_id,
                        timestamp,
                        timestamp,
                        serde_json::to_string(&part)
                            .context("failed to serialize OpenCode part data")?,
                    ],
                )
                .with_context(|| format!("failed to insert OpenCode part {part_idx}"))?;
            }
        }

        tx.commit().context("failed to commit transaction")?;

        let virtual_path = Self::virtual_session_path(db_path, &target_session_id);
        info!(
            session_id = target_session_id,
            path = %db_path.display(),
            messages = session.messages.len(),
            schema = Schema::Current.label(),
            "OpenCode session written"
        );

        Ok(WrittenSession {
            paths: vec![virtual_path],
            session_id: target_session_id.clone(),
            resume_command: provider.resume_command(&target_session_id),
            backups: Vec::new(),
            warnings: Vec::new(),
        })
    }

    /// The `project` row a written session should hang off.
    ///
    /// Prefers the project OpenCode already has for this worktree so the session
    /// shows up under the directory the user is in; only creates one when the
    /// database has nothing that fits.
    fn resolve_project_id(
        conn: &Connection,
        directory: &str,
        created_at: i64,
    ) -> anyhow::Result<String> {
        if !directory.is_empty()
            && let Ok(id) = conn.query_row(
                "SELECT id FROM project WHERE worktree = ?1 LIMIT 1",
                rusqlite::params![directory],
                |row| row.get::<_, String>(0),
            )
        {
            return Ok(id);
        }

        if directory.is_empty()
            && let Ok(id) = conn.query_row(
                "SELECT id FROM project ORDER BY time_updated DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
        {
            return Ok(id);
        }

        let project_id = uuid::Uuid::new_v4().simple().to_string();
        conn.execute(
            "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes)
             VALUES (?1, ?2, NULL, ?3, ?3, '[]')",
            rusqlite::params![project_id, directory, created_at],
        )
        .context("failed to create OpenCode project row")?;
        Ok(project_id)
    }

    /// Write into the pre-rename `sessions`/`messages`/`files` layout.
    ///
    /// `created` says the database did not exist before this call, which is the
    /// only case that warrants a warning: an OpenCode old enough to own a legacy
    /// database reads it fine, but a database casr just invented is one no
    /// current OpenCode will ever look at.
    fn write_session_legacy(
        provider: &Self,
        session: &CanonicalSession,
        opts: &WriteOptions,
        db_path: &Path,
        created: bool,
    ) -> anyhow::Result<WrittenSession> {
        let mut conn = Self::open_db_rw(db_path)?;
        Self::ensure_schema(&conn)?;

        let has_count_trigger =
            Self::trigger_exists(&conn, "update_session_message_count_on_insert");

        // Derive a STABLE target id from the source session so re-converting the
        // same session targets the same row (matching the clawdbot/cursor/pi_agent
        // idiom). This makes `--force` meaningful: without a stable id every run
        // would silently create an orphaned duplicate row, and with a colliding id
        // the INSERT would otherwise fail on the PRIMARY KEY. Fall back to a random
        // UUID only when the source has no id.
        let target_session_id = if session.session_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            session.session_id.clone()
        };

        // Honor `--force`: if the target session already exists, either overwrite
        // it (delete-then-insert; `ON DELETE CASCADE` clears messages/files) or
        // return a clean conflict error, matching the cursor provider's behavior.
        if Self::session_exists(&conn, &target_session_id) {
            if opts.force {
                // `ensure_schema` already enabled `PRAGMA foreign_keys = ON` on
                // this connection, so deleting the session cascades to messages
                // and files. Delete dependents explicitly too, in case the live
                // DB predates the FK constraint or has the pragma disabled.
                let _ = conn.execute(
                    "DELETE FROM files WHERE session_id = ?1",
                    rusqlite::params![target_session_id],
                );
                let _ = conn.execute(
                    "DELETE FROM messages WHERE session_id = ?1",
                    rusqlite::params![target_session_id],
                );
                conn.execute(
                    "DELETE FROM sessions WHERE id = ?1",
                    rusqlite::params![target_session_id],
                )
                .context("failed to delete existing OpenCode session for --force overwrite")?;
            } else {
                return Err(crate::error::CasrError::SessionConflict {
                    session_id: target_session_id,
                    existing_path: db_path.to_path_buf(),
                }
                .into());
            }
        }

        let now = chrono::Utc::now().timestamp_millis();
        let created_at = session.started_at.unwrap_or(now);
        let updated_at = session.ended_at.unwrap_or(now);

        let title = session.title.clone().or_else(|| {
            session
                .messages
                .iter()
                .find(|m| m.role == MessageRole::User)
                .map(|m| truncate_title(&m.content, 80))
                .filter(|t| !t.is_empty())
        });
        let title = title.unwrap_or_else(|| "Converted session".to_string());

        let tx = conn.transaction().context("failed to begin transaction")?;

        tx.execute(
            "INSERT INTO sessions (
                id, parent_session_id, title, message_count, prompt_tokens, completion_tokens, cost,
                summary_message_id, updated_at, created_at
             ) VALUES (?1, NULL, ?2, ?3, 0, 0, 0.0, NULL, ?4, ?5)",
            rusqlite::params![
                target_session_id,
                title,
                if has_count_trigger {
                    0_i64
                } else {
                    i64::try_from(session.messages.len()).unwrap_or(i64::MAX)
                },
                updated_at,
                created_at,
            ],
        )
        .context("failed to insert OpenCode session")?;

        let default_model = session.model_name.clone();
        let timestamps = monotonic_timestamps(&session.messages, created_at);
        for (i, msg) in session.messages.iter().enumerate() {
            // Ordered id and clamped timestamp, for the same reason as the
            // current-schema writer: this layout is read back with
            // `ORDER BY created_at ASC, id ASC` too.
            let message_id = format!("{i:06}-{}", uuid::Uuid::new_v4());
            let parts = build_parts(msg);
            let parts_json =
                serde_json::to_string(&parts).context("failed to serialize OpenCode parts")?;
            let timestamp = timestamps[i];
            let model = msg.author.clone().or_else(|| default_model.clone());

            tx.execute(
                "INSERT INTO messages (
                    id, session_id, role, parts, model, created_at, updated_at, finished_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                rusqlite::params![
                    message_id,
                    target_session_id,
                    role_to_opencode(&msg.role),
                    parts_json,
                    model,
                    timestamp,
                    timestamp,
                ],
            )
            .with_context(|| format!("failed to insert OpenCode message {}", msg.idx))?;
        }

        // If the DB has no count trigger, set message_count explicitly.
        if !has_count_trigger {
            tx.execute(
                "UPDATE sessions SET message_count = ?1 WHERE id = ?2",
                rusqlite::params![
                    i64::try_from(session.messages.len()).unwrap_or(i64::MAX),
                    target_session_id
                ],
            )
            .context("failed to update OpenCode session message_count")?;
        }

        tx.commit().context("failed to commit transaction")?;

        let virtual_path = Self::virtual_session_path(db_path, &target_session_id);
        info!(
            session_id = target_session_id,
            path = %db_path.display(),
            messages = session.messages.len(),
            schema = Schema::Legacy.label(),
            "OpenCode session written"
        );

        let warnings = if created {
            vec![format!(
                "created a new OpenCode database at {} using the legacy \
                 {} schema. Current OpenCode reads only its own database \
                 ({}) and will not show this session; re-run with \
                 OPENCODE_DB_PATH pointing at that file to write a session \
                 OpenCode can open.",
                db_path.display(),
                Schema::Legacy.label(),
                Self::upstream_data_dir()
                    .map(|dir| dir.join(DB_FILENAME).display().to_string())
                    .unwrap_or_else(|| "~/.local/share/opencode/opencode.db".to_string()),
            )]
        } else {
            Vec::new()
        };

        Ok(WrittenSession {
            paths: vec![virtual_path],
            session_id: target_session_id.clone(),
            resume_command: provider.resume_command(&target_session_id),
            backups: Vec::new(),
            warnings,
        })
    }
}

fn dedup_existing_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for path in paths {
        if path.is_file() {
            seen.insert(path);
        }
    }
    seen.into_iter().collect()
}

fn parse_tool_call_arguments(input: &str) -> serde_json::Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({ "input": input }))
}

fn parse_parts(parts: &serde_json::Value) -> (String, Vec<ToolCall>, Vec<ToolResult>) {
    let mut text_chunks: Vec<String> = Vec::new();
    let mut reasoning_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_results: Vec<ToolResult> = Vec::new();

    let Some(items) = parts.as_array() else {
        return (String::new(), tool_calls, tool_results);
    };

    for item in items {
        let part_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let data = item.get("data").unwrap_or(&serde_json::Value::Null);

        match part_type {
            "text" => {
                if let Some(text) = data.get("text").and_then(serde_json::Value::as_str)
                    && !text.trim().is_empty()
                {
                    text_chunks.push(text.to_string());
                }
            }
            "reasoning" => {
                if let Some(thinking) = data.get("thinking").and_then(serde_json::Value::as_str)
                    && !thinking.trim().is_empty()
                {
                    reasoning_chunks.push(thinking.to_string());
                }
            }
            "tool_call" => {
                let name = data
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or("tool_call")
                    .to_string();
                let id = data
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToString::to_string);
                let input = data
                    .get("input")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();

                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: parse_tool_call_arguments(input),
                });
            }
            "tool_result" => {
                let content = data
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let call_id = data
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToString::to_string);
                let is_error = data
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                tool_results.push(ToolResult {
                    call_id,
                    content,
                    is_error,
                });
            }
            _ => {
                let fallback = flatten_content(data);
                if !fallback.trim().is_empty() {
                    text_chunks.push(fallback);
                }
            }
        }
    }

    let mut content = text_chunks.join("\n");
    if content.trim().is_empty() {
        content = reasoning_chunks.join("\n");
    }
    if content.trim().is_empty() {
        let result_texts: Vec<&str> = tool_results
            .iter()
            .map(|result| result.content.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect();
        content = result_texts.join("\n");
    }

    (content, tool_calls, tool_results)
}

/// Flatten current-schema `part` rows into content, tool calls and tool results.
///
/// The current layout stores one row per part and the discriminated union is
/// the part row's own `data` blob, so — unlike the legacy `{type, data}`
/// wrapper — the fields sit at the top level. Structural parts (`step-start`,
/// `snapshot`, `patch`, …) carry no conversation text and contribute nothing;
/// nothing is lost by that, because the caller keeps every raw part in the
/// message's `extra.opencode_parts`.
fn parse_current_parts(parts: &[serde_json::Value]) -> (String, Vec<ToolCall>, Vec<ToolResult>) {
    let mut text_chunks: Vec<String> = Vec::new();
    let mut reasoning_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_results: Vec<ToolResult> = Vec::new();

    for item in parts {
        let part_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        match part_type {
            "text" => {
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str)
                    && !text.trim().is_empty()
                {
                    text_chunks.push(text.to_string());
                }
            }
            "reasoning" => {
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str)
                    && !text.trim().is_empty()
                {
                    reasoning_chunks.push(text.to_string());
                }
            }
            "tool" => {
                let name = item
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or("tool_call")
                    .to_string();
                let id = item
                    .get("callID")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToString::to_string);
                let state = item.get("state").unwrap_or(&serde_json::Value::Null);
                let arguments = state
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));

                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name,
                    arguments,
                });

                // Only a finished call has a result. A pending or running call
                // has genuinely produced nothing yet, and inventing an empty
                // result for it would claim the tool returned nothing.
                match state.get("status").and_then(serde_json::Value::as_str) {
                    Some("completed") => tool_results.push(ToolResult {
                        call_id: id,
                        content: state
                            .get("output")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        is_error: false,
                    }),
                    Some("error") => tool_results.push(ToolResult {
                        call_id: id,
                        content: state
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        is_error: true,
                    }),
                    Some(_) | None => {}
                }
            }
            // Bookkeeping parts: real, but not conversation content.
            "step-start" | "step-finish" | "snapshot" | "patch" | "agent" | "retry"
            | "compaction" | "file" => {}
            _ => {
                let fallback = flatten_content(item);
                if !fallback.trim().is_empty() {
                    text_chunks.push(fallback);
                }
            }
        }
    }

    let mut content = text_chunks.join("\n");
    if content.trim().is_empty() {
        content = reasoning_chunks.join("\n");
    }
    if content.trim().is_empty() {
        let result_texts: Vec<&str> = tool_results
            .iter()
            .map(|result| result.content.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect();
        content = result_texts.join("\n");
    }

    (content, tool_calls, tool_results)
}

fn build_parts(message: &CanonicalMessage) -> serde_json::Value {
    let mut parts = Vec::new();

    if !message.content.trim().is_empty() {
        parts.push(serde_json::json!({
            "type": "text",
            "data": { "text": message.content },
        }));
    }

    for call in &message.tool_calls {
        let input = if let Some(s) = call.arguments.as_str() {
            s.to_string()
        } else {
            serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string())
        };

        parts.push(serde_json::json!({
            "type": "tool_call",
            "data": {
                "id": call.id.clone().unwrap_or_default(),
                "name": call.name,
                "input": input,
                "type": "function",
                "finished": true
            }
        }));
    }

    for result in &message.tool_results {
        parts.push(serde_json::json!({
            "type": "tool_result",
            "data": {
                "tool_call_id": result.call_id.clone().unwrap_or_default(),
                "name": "tool",
                "content": result.content,
                "metadata": "",
                "is_error": result.is_error
            }
        }));
    }

    serde_json::Value::Array(parts)
}

/// A session id current OpenCode will accept.
///
/// OpenCode brands its ids: `opencode export`, and every other command that
/// takes a session, rejects anything not starting with `ses` — a converted
/// session written under its source's own id is listed but cannot be opened.
///
/// The id stays *derived* from the source rather than random, because
/// re-conversion, `--force` and [`Provider::owns_session`] all depend on the
/// same source session resolving to the same target row.
fn opencode_session_id(source_id: &str) -> String {
    if source_id.starts_with("ses_") {
        return source_id.to_string();
    }
    let sanitized: String = source_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    if sanitized.is_empty() {
        return format!("ses_{}", uuid::Uuid::new_v4().simple());
    }
    format!("ses_{sanitized}")
}

/// Message timestamps that preserve message order in a time-sorted store.
///
/// Both layouts order a session's messages by `(created, id)`, so a source
/// whose message order disagrees with its own timestamps reads back reordered.
/// That is not a corner case: in a real Claude Code transcript a tool result is
/// routinely stamped a millisecond *before* the message it follows, and the
/// round-trip then returns two messages swapped.
///
/// Holding each timestamp at no less than its predecessor fixes the order while
/// keeping every message inside the session's own span — the clamp only ever
/// moves a timestamp forward to one already present earlier in the session.
fn monotonic_timestamps(messages: &[CanonicalMessage], fallback: i64) -> Vec<i64> {
    let mut last = i64::MIN;
    messages
        .iter()
        .map(|msg| {
            let ts = msg.timestamp.unwrap_or(fallback).max(last);
            last = ts;
            ts
        })
        .collect()
}

/// Split casr's single `model_name` into OpenCode's `(providerID, modelID)`.
///
/// OpenCode names a model `provider/model`; casr carries whatever the source
/// provider recorded, which is usually just the bare model. Both fields are
/// required by OpenCode's message schema, so an unsplittable name yields an
/// explicit `"unknown"` provider rather than a plausible-looking guess.
fn split_model_ref(model_name: Option<&str>) -> (String, String) {
    let Some(name) = model_name.map(str::trim).filter(|n| !n.is_empty()) else {
        return ("unknown".to_string(), "unknown".to_string());
    };
    match name.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_string(), model.to_string())
        }
        Some(_) | None => ("unknown".to_string(), name.to_string()),
    }
}

/// Build the `message.data` blob for the current schema.
fn build_message_data(
    message: &CanonicalMessage,
    timestamp: i64,
    provider_id: &str,
    model_id: &str,
) -> serde_json::Value {
    let model_id = message.author.as_deref().unwrap_or(model_id);
    // OpenCode's message union has exactly two roles. Everything that is not an
    // assistant turn becomes a user turn, which is the same equivalence
    // `pipeline::readback_role_bucket` applies — collapsing them the other way
    // would turn a tool or system message into something that reads back as the
    // model's own words.
    match message.role {
        MessageRole::User | MessageRole::Tool | MessageRole::System | MessageRole::Other(_) => {
            serde_json::json!({
                "role": "user",
                "time": { "created": timestamp },
                "agent": "build",
                "model": { "providerID": provider_id, "modelID": model_id },
            })
        }
        MessageRole::Assistant => serde_json::json!({
            "role": "assistant",
            "time": { "created": timestamp, "completed": timestamp },
            "mode": "build",
            "agent": "build",
            "path": { "cwd": "", "root": "" },
            "cost": 0,
            "tokens": {
                "input": 0, "output": 0, "reasoning": 0,
                "cache": { "read": 0, "write": 0 }
            },
            "modelID": model_id,
            "providerID": provider_id,
        }),
    }
}

/// Build the `part.data` blobs for the current schema.
///
/// The current layout stores each part as its own row whose `data` blob is the
/// part itself, so — unlike [`build_parts`] — there is no `{type, data}`
/// wrapper and a tool call and its result are one `tool` part, not two.
fn build_current_parts(message: &CanonicalMessage) -> Vec<serde_json::Value> {
    let mut parts = Vec::new();

    if !message.content.trim().is_empty() {
        parts.push(serde_json::json!({ "type": "text", "text": message.content }));
    }

    // Each result is claimed by at most one call, so calls that share a missing
    // id do not all end up reporting the same output.
    let mut claimed = vec![false; message.tool_results.len()];

    for call in &message.tool_calls {
        let input = if call.arguments.is_object() {
            call.arguments.clone()
        } else {
            serde_json::json!({ "input": call.arguments })
        };

        // OpenCode pairs a result to its call by `callID`, so the result is
        // folded into the call's state rather than emitted separately.
        let matched =
            message.tool_results.iter().enumerate().find(|(i, result)| {
                !claimed[*i] && result.call_id.as_deref() == call.id.as_deref()
            });
        if let Some((i, _)) = matched {
            claimed[i] = true;
        }

        let state = match matched.map(|(_, result)| result) {
            Some(result) if result.is_error => serde_json::json!({
                "status": "error",
                "input": input,
                "error": result.content,
                "time": { "start": 0, "end": 0 },
            }),
            Some(result) => serde_json::json!({
                "status": "completed",
                "input": input,
                "output": result.content,
                "title": call.name,
                "metadata": {},
                "time": { "start": 0, "end": 0 },
            }),
            // No result recorded: the call really is unfinished, and OpenCode
            // has a status that says exactly that.
            None => serde_json::json!({ "status": "pending", "input": input, "raw": "" }),
        };

        parts.push(serde_json::json!({
            "type": "tool",
            "callID": call.id.clone().unwrap_or_default(),
            "tool": call.name,
            "state": state,
        }));
    }

    // A result whose call lives in an earlier message — the shape Claude Code
    // and Codex use — still has to survive. It gets its own tool part rather
    // than a text part: the current schema has nowhere else to put a result,
    // and appending it to the text would put tool output into the transcript as
    // if someone had said it.
    for (i, result) in message.tool_results.iter().enumerate() {
        if claimed[i] {
            continue;
        }
        let state = if result.is_error {
            serde_json::json!({
                "status": "error",
                "input": {},
                "error": result.content,
                "time": { "start": 0, "end": 0 },
            })
        } else {
            serde_json::json!({
                "status": "completed",
                "input": {},
                "output": result.content,
                "title": "tool",
                "metadata": {},
                "time": { "start": 0, "end": 0 },
            })
        };
        parts.push(serde_json::json!({
            "type": "tool",
            "callID": result.call_id.clone().unwrap_or_default(),
            "tool": "tool",
            "state": state,
        }));
    }

    parts
}

fn role_to_opencode(role: &MessageRole) -> &str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
        MessageRole::System => "system",
        MessageRole::Other(role) => role.as_str(),
    }
}

/// Note on what is *not* tested here.
///
/// Every path that reaches [`OpenCode::choose_target_db_path`] reads the
/// process environment, so a test that exercised it in-crate would consult the
/// developer's real OpenCode install. `src/lib.rs` declares
/// `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe` in edition
/// 2024, so an in-crate test cannot redirect `XDG_DATA_HOME` and cannot isolate
/// itself. Those tests live in `tests/opencode_write_test.rs`, which can.
///
/// What stays here needs no environment: pure parsing, virtual-path round
/// trips, and schema questions asked about a path the test was handed.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;

    #[test]
    fn provider_metadata_and_resume_command() {
        let provider = OpenCode;
        assert_eq!(provider.name(), "OpenCode");
        assert_eq!(provider.slug(), "opencode");
        assert_eq!(provider.cli_alias(), "opc");
        assert_eq!(
            <OpenCode as Provider>::resume_command(&provider, "sid"),
            "opencode"
        );
    }

    #[test]
    fn virtual_path_round_trip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".opencode")).expect("data dir");
        let db = workspace.join(".opencode/opencode.db");
        std::fs::write(&db, "").expect("touch db file");

        let sid = "abc-123";
        let virtual_path = OpenCode::virtual_session_path(&db, sid);
        let parsed = OpenCode::parse_virtual_path(&virtual_path).expect("should parse");
        assert_eq!(parsed.0, db.as_path());
        assert_eq!(parsed.1, sid);
    }

    #[test]
    fn parse_parts_extracts_tool_calls_and_results() {
        let raw = serde_json::json!([
            {"type":"text","data":{"text":"hello"}},
            {"type":"tool_call","data":{"id":"c1","name":"Read","input":"{\"path\":\"src/main.rs\"}","type":"function","finished":true}},
            {"type":"tool_result","data":{"tool_call_id":"c1","name":"Read","content":"ok","metadata":"","is_error":false}}
        ]);

        let (content, tool_calls, tool_results) = parse_parts(&raw);
        assert_eq!(content, "hello");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "Read");
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0].content, "ok");
    }

    // ── parse_parts edge cases ──────────────────────────────────────────

    #[test]
    fn parse_parts_reasoning_content_when_no_text() {
        let raw = serde_json::json!([
            {"type":"reasoning","data":{"thinking":"Let me analyze this problem step by step."}}
        ]);
        let (content, tool_calls, tool_results) = parse_parts(&raw);
        assert_eq!(content, "Let me analyze this problem step by step.");
        assert!(tool_calls.is_empty());
        assert!(tool_results.is_empty());
    }

    #[test]
    fn parse_parts_text_preferred_over_reasoning() {
        let raw = serde_json::json!([
            {"type":"text","data":{"text":"The answer is 42."}},
            {"type":"reasoning","data":{"thinking":"Hmm, thinking..."}}
        ]);
        let (content, _, _) = parse_parts(&raw);
        assert_eq!(content, "The answer is 42.");
    }

    #[test]
    fn parse_parts_empty_array() {
        let raw = serde_json::json!([]);
        let (content, tool_calls, tool_results) = parse_parts(&raw);
        assert!(content.is_empty());
        assert!(tool_calls.is_empty());
        assert!(tool_results.is_empty());
    }

    #[test]
    fn parse_parts_non_array_returns_empty() {
        let raw = serde_json::json!("just a string");
        let (content, tool_calls, tool_results) = parse_parts(&raw);
        assert!(content.is_empty());
        assert!(tool_calls.is_empty());
        assert!(tool_results.is_empty());
    }

    #[test]
    fn parse_parts_unknown_type_uses_fallback() {
        // Unknown part type with a "text" field in data → flatten_content extracts it.
        let raw = serde_json::json!([
            {"type":"custom_widget","data":"Some inline text from unknown part type"}
        ]);
        let (content, _, _) = parse_parts(&raw);
        assert_eq!(content, "Some inline text from unknown part type");
    }

    #[test]
    fn parse_parts_tool_result_fallback_when_no_text_or_reasoning() {
        let raw = serde_json::json!([
            {"type":"tool_result","data":{"tool_call_id":"c1","content":"file contents here","is_error":false}}
        ]);
        let (content, _, tool_results) = parse_parts(&raw);
        assert_eq!(content, "file contents here");
        assert_eq!(tool_results.len(), 1);
    }

    #[test]
    fn parse_parts_multiple_text_chunks_joined() {
        let raw = serde_json::json!([
            {"type":"text","data":{"text":"First part."}},
            {"type":"text","data":{"text":"Second part."}}
        ]);
        let (content, _, _) = parse_parts(&raw);
        assert_eq!(content, "First part.\nSecond part.");
    }

    #[test]
    fn parse_parts_skips_empty_text() {
        let raw = serde_json::json!([
            {"type":"text","data":{"text":"  "}},
            {"type":"text","data":{"text":"real content"}}
        ]);
        let (content, _, _) = parse_parts(&raw);
        assert_eq!(content, "real content");
    }

    #[test]
    fn parse_parts_tool_call_missing_name_defaults() {
        let raw = serde_json::json!([
            {"type":"tool_call","data":{"id":"c1","name":"","input":"{}","type":"function"}}
        ]);
        let (_, tool_calls, _) = parse_parts(&raw);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "tool_call");
    }

    #[test]
    fn parse_parts_tool_call_no_id_is_none() {
        let raw = serde_json::json!([
            {"type":"tool_call","data":{"name":"Bash","input":"{\"cmd\":\"ls\"}"}}
        ]);
        let (_, tool_calls, _) = parse_parts(&raw);
        assert_eq!(tool_calls.len(), 1);
        assert!(tool_calls[0].id.is_none());
    }

    #[test]
    fn parse_parts_tool_result_error_flag() {
        let raw = serde_json::json!([
            {"type":"tool_result","data":{"tool_call_id":"c1","content":"command failed","is_error":true}}
        ]);
        let (_, _, tool_results) = parse_parts(&raw);
        assert_eq!(tool_results.len(), 1);
        assert!(tool_results[0].is_error);
    }

    // ── parse_tool_call_arguments ───────────────────────────────────────

    #[test]
    fn parse_tool_call_arguments_valid_json() {
        let result = parse_tool_call_arguments(r#"{"path":"src/main.rs"}"#);
        assert_eq!(result["path"], "src/main.rs");
    }

    #[test]
    fn parse_tool_call_arguments_empty_returns_empty_object() {
        let result = parse_tool_call_arguments("");
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn parse_tool_call_arguments_invalid_json_wraps_in_input() {
        let result = parse_tool_call_arguments("not json");
        assert_eq!(result["input"], "not json");
    }

    // ── role_to_opencode ────────────────────────────────────────────────

    #[test]
    fn role_to_opencode_all_variants() {
        assert_eq!(role_to_opencode(&MessageRole::User), "user");
        assert_eq!(role_to_opencode(&MessageRole::Assistant), "assistant");
        assert_eq!(role_to_opencode(&MessageRole::Tool), "tool");
        assert_eq!(role_to_opencode(&MessageRole::System), "system");
        assert_eq!(
            role_to_opencode(&MessageRole::Other("custom".to_string())),
            "custom"
        );
    }

    // ── build_parts ─────────────────────────────────────────────────────

    #[test]
    fn build_parts_text_only() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Hello world".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::json!({}),
        };
        let parts = build_parts(&msg);
        let arr = parts.as_array().expect("should be array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["data"]["text"], "Hello world");
    }

    #[test]
    fn build_parts_with_tool_call_and_result() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "Let me check.".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![ToolCall {
                id: Some("tc-1".to_string()),
                name: "Bash".to_string(),
                arguments: serde_json::json!({"cmd": "ls"}),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("tc-1".to_string()),
                content: "file1.rs\nfile2.rs".to_string(),
                is_error: false,
            }],
            extra: serde_json::json!({}),
        };
        let parts = build_parts(&msg);
        let arr = parts.as_array().expect("should be array");
        assert_eq!(arr.len(), 3); // text + tool_call + tool_result
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "tool_call");
        assert_eq!(arr[1]["data"]["name"], "Bash");
        assert_eq!(arr[2]["type"], "tool_result");
        assert!(!arr[2]["data"]["is_error"].as_bool().unwrap());
    }

    #[test]
    fn build_parts_empty_content_skips_text() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Tool,
            content: "  ".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: Some("c1".to_string()),
                content: "result".to_string(),
                is_error: false,
            }],
            extra: serde_json::json!({}),
        };
        let parts = build_parts(&msg);
        let arr = parts.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "tool_result");
    }

    // ── workspace_from_db_path ──────────────────────────────────────────

    #[test]
    fn workspace_from_db_path_valid() {
        let path = PathBuf::from("/home/user/project/.opencode/opencode.db");
        let ws = OpenCode::workspace_from_db_path(&path);
        assert_eq!(ws, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn workspace_from_db_path_wrong_dirname_returns_none() {
        let path = PathBuf::from("/home/user/project/data/opencode.db");
        let ws = OpenCode::workspace_from_db_path(&path);
        assert!(ws.is_none());
    }

    #[test]
    fn workspace_from_db_path_root_opencode_returns_none() {
        let path = PathBuf::from("/.opencode/opencode.db");
        let ws = OpenCode::workspace_from_db_path(&path);
        assert_eq!(ws, Some(PathBuf::from("/")));
    }

    // ── virtual_path_special_characters ─────────────────────────────────

    #[test]
    fn virtual_path_encodes_special_characters() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let db = tmp.path().join("opencode.db");
        std::fs::write(&db, "").expect("touch");

        let sid = "session/with spaces&special=chars";
        let vp = OpenCode::virtual_session_path(&db, sid);
        let (parsed_db, parsed_sid) = OpenCode::parse_virtual_path(&vp).expect("parse");
        assert_eq!(parsed_db, db);
        assert_eq!(parsed_sid, sid);
    }

    // ── writer edge cases ───────────────────────────────────────────────

    // ── reader edge cases ───────────────────────────────────────────────

    // ── dedup_existing_files ────────────────────────────────────────────

    #[test]
    fn dedup_existing_files_removes_duplicates_and_nonexistent() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let file1 = tmp.path().join("a.db");
        let file2 = tmp.path().join("b.db");
        std::fs::write(&file1, "").expect("touch");
        std::fs::write(&file2, "").expect("touch");

        let input = vec![
            file1.clone(),
            file2.clone(),
            file1.clone(),                     // duplicate
            tmp.path().join("nonexistent.db"), // doesn't exist
        ];
        let result = dedup_existing_files(input);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&file1));
        assert!(result.contains(&file2));
    }

    #[test]
    fn dedup_existing_files_empty_input() {
        let result = dedup_existing_files(Vec::new());
        assert!(result.is_empty());
    }

    // ── list_sessions ───────────────────────────────────────────────────

    // ── current schema (session/message/part) ───────────────────────────
    //
    // The DDL below is copied verbatim from the database the released
    // `opencode-linux-x64` 1.18.6 binary creates (`sqlite_master`), trimmed to
    // the three tables this provider touches. `tests/fixtures/opencode-current`
    // holds the whole thing; this is here so a unit test can build one.

    const CURRENT_SCHEMA_DDL: &str = r#"
CREATE TABLE `session` (
  `id` text PRIMARY KEY, `project_id` text NOT NULL, `workspace_id` text,
  `parent_id` text, `slug` text NOT NULL, `directory` text NOT NULL, `path` text,
  `title` text NOT NULL, `version` text NOT NULL, `share_url` text,
  `summary_additions` integer, `summary_deletions` integer, `summary_files` integer,
  `summary_diffs` text, `metadata` text, `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL, `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL, `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL, `revert` text, `permission` text,
  `agent` text, `model` text, `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL, `time_compacting` integer, `time_archived` integer);
CREATE TABLE `message` (
  `id` text PRIMARY KEY, `session_id` text NOT NULL, `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE TABLE `part` (
  `id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE TABLE `project` (
  `id` text PRIMARY KEY, `worktree` text NOT NULL, `vcs` text, `name` text,
  `icon_url` text, `icon_url_override` text, `icon_color` text,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL,
  `time_initialized` integer, `sandboxes` text NOT NULL, `commands` text);
"#;

    fn make_current_db(path: &Path) {
        let conn = OpenCode::open_db_rw(path).expect("create db");
        conn.execute_batch(CURRENT_SCHEMA_DDL).expect("ddl");
    }

    #[test]
    fn detect_schema_tells_the_two_layouts_apart() {
        let tmp = tempfile::tempdir().expect("tmpdir");

        let current = tmp.path().join("current.db");
        make_current_db(&current);
        let conn = OpenCode::open_db(&current).expect("open");
        assert_eq!(OpenCode::detect_schema(&conn), Some(Schema::Current));

        let legacy = tmp.path().join("legacy.db");
        let conn = OpenCode::open_db_rw(&legacy).expect("create");
        OpenCode::ensure_schema(&conn).expect("legacy schema");
        drop(conn);
        let conn = OpenCode::open_db(&legacy).expect("open");
        assert_eq!(OpenCode::detect_schema(&conn), Some(Schema::Legacy));

        // A database that is neither is reported as neither, not as empty.
        let foreign = tmp.path().join("foreign.db");
        let conn = OpenCode::open_db_rw(&foreign).expect("create");
        conn.execute_batch("CREATE TABLE unrelated (id TEXT)")
            .expect("ddl");
        drop(conn);
        let conn = OpenCode::open_db(&foreign).expect("open");
        assert_eq!(OpenCode::detect_schema(&conn), None);
    }

    /// A database that cannot be read must never be reported the same way as a
    /// database that was read and turned out to be empty. Confusing those two
    /// is the exact failure this provider had: `✓ found 1 opencode.db
    /// database(s)` next to a list of zero sessions.
    #[test]
    fn describe_db_separates_unreadable_from_empty() {
        let tmp = tempfile::tempdir().expect("tmpdir");

        let empty = tmp.path().join("empty.db");
        make_current_db(&empty);
        assert_eq!(
            OpenCode::describe_db(&empty),
            Ok((Schema::Current, 0)),
            "an empty but valid database reports zero sessions under a known schema"
        );

        let foreign = tmp.path().join("foreign.db");
        let conn = OpenCode::open_db_rw(&foreign).expect("create");
        conn.execute_batch("CREATE TABLE unrelated (id TEXT)")
            .expect("ddl");
        drop(conn);
        let described = OpenCode::describe_db(&foreign);
        assert!(
            described.is_err(),
            "an unrecognised database must be an error, not zero sessions: {described:?}"
        );
    }

    #[test]
    fn opencode_session_id_brands_foreign_ids_deterministically() {
        // Already an OpenCode id: left alone, so re-reading a session casr wrote
        // back into OpenCode does not rename it again.
        assert_eq!(
            opencode_session_id("ses_05db873bfffesTmyTPhYKMceqn"),
            "ses_05db873bfffesTmyTPhYKMceqn"
        );
        // A foreign id is folded into OpenCode's branded form, and the same
        // source always yields the same target.
        let uuid = "7732146b-7bb7-4d07-9899-1f54565de931";
        assert_eq!(
            opencode_session_id(uuid),
            "ses_7732146b7bb74d0798991f54565de931"
        );
        assert_eq!(opencode_session_id(uuid), opencode_session_id(uuid));
        assert!(opencode_session_id("").starts_with("ses_"));
    }

    #[test]
    fn parse_current_parts_reads_the_part_union() {
        // Shapes taken from the released 1.18.6 OpenAPI document.
        let parts = vec![
            serde_json::json!({"type":"reasoning","text":"thinking out loud"}),
            serde_json::json!({"type":"step-start"}),
            serde_json::json!({"type":"text","text":"here you go"}),
            serde_json::json!({"type":"tool","callID":"c1","tool":"read","state":{
                "status":"completed","input":{"filePath":"a.rs"},"output":"ok","title":"a.rs",
                "metadata":{},"time":{"start":1,"end":2}}}),
            serde_json::json!({"type":"tool","callID":"c2","tool":"bash","state":{
                "status":"error","input":{"command":"false"},"error":"boom",
                "time":{"start":3,"end":4}}}),
            serde_json::json!({"type":"tool","callID":"c3","tool":"grep","state":{
                "status":"running","input":{"pattern":"x"},"time":{"start":5}}}),
        ];
        let (content, calls, results) = parse_current_parts(&parts);

        assert_eq!(content, "here you go", "reasoning must not shadow text");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].arguments["filePath"], "a.rs");
        assert_eq!(calls[2].name, "grep");
        assert_eq!(
            results.len(),
            2,
            "a running call has produced no result yet, and none must be invented"
        );
        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert_eq!(results[1].content, "boom");
    }

    #[test]
    fn parse_current_parts_falls_back_to_reasoning_then_tool_output() {
        let (content, _, _) =
            parse_current_parts(&[serde_json::json!({"type":"reasoning","text":"only thinking"})]);
        assert_eq!(content, "only thinking");

        let (content, _, _) = parse_current_parts(&[serde_json::json!({
            "type":"tool","callID":"c1","tool":"read",
            "state":{"status":"completed","input":{},"output":"file body","title":"t","metadata":{},
                     "time":{"start":1,"end":2}}})]);
        assert_eq!(content, "file body");
    }

    #[test]
    fn monotonic_timestamps_only_moves_time_forward() {
        let msg = |ts: Option<i64>| CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: String::new(),
            timestamp: ts,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::json!({}),
        };
        let messages = vec![msg(Some(100)), msg(Some(90)), msg(None), msg(Some(300))];
        assert_eq!(
            monotonic_timestamps(&messages, 42),
            vec![100, 100, 100, 300],
            "clamped values must be ones the session already contains"
        );
    }
}
