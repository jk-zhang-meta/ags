//! OpenCode provider — reads sessions from SQLite `opencode.db`.
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
//!
//! Target writes never edit SQLite directly. When the official `opencode` CLI
//! is available, casr gives its `import` command an export-shaped JSON file,
//! verifies the imported session through this reader, and uses
//! `opencode session delete` for rollback.
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags};
use tracing::{debug, trace, warn};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession};

/// OpenCode provider implementation.
pub struct OpenCode;

const DB_FILENAME: &str = "opencode.db";
const DATA_DIRNAME: &str = ".opencode";
const OPENCODE_BIN_ENV: &str = "OPENCODE_BIN";
const OPENCODE_CLI_REQUIRED: &str = "OpenCode is read/resume-only on this machine: target writes \
require the official `opencode` CLI in PATH (or OPENCODE_BIN). casr uses the vendor's import and \
delete commands and will not modify opencode.db directly.";

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
    fn resume_spec(session_id: &str) -> LaunchSpec {
        LaunchSpec::new(
            "opencode",
            ["--session".to_string(), session_id.to_string()],
        )
    }

    fn binary_path() -> anyhow::Result<PathBuf> {
        if let Some(path) = std::env::var_os(OPENCODE_BIN_ENV).filter(|value| !value.is_empty()) {
            let path = Self::absolute_path(PathBuf::from(path))?;
            if path.is_file() {
                return Ok(path);
            }
            anyhow::bail!(
                "{OPENCODE_BIN_ENV} names {}, but that is not an OpenCode executable file",
                path.display()
            );
        }

        which::which("opencode").context(
            "OpenCode writes require the official `opencode` CLI in PATH (or OPENCODE_BIN)",
        )
    }

    /// The database the official CLI must import into.
    ///
    /// casr's `OPENCODE_HOME` and `OPENCODE_DB_PATH` overrides are intentionally
    /// understood here even though OpenCode itself does not know them. The child
    /// receives the resolved absolute path through OpenCode's own `OPENCODE_DB`.
    fn write_db_path() -> anyhow::Result<PathBuf> {
        let path = Self::env_db_path()
            .or_else(|| Self::upstream_data_dir().map(|dir| dir.join(DB_FILENAME)))
            .context("could not determine OpenCode's data directory")?;
        Self::absolute_path(path)
    }

    fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
        if path.is_absolute() {
            return Ok(path);
        }
        Ok(std::env::current_dir()
            .context("could not resolve an OpenCode path against the current directory")?
            .join(path))
    }

    fn command(binary: &Path, db_path: &Path, cwd: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(cwd)
            .env("OPENCODE_DB", db_path)
            // Import/delete do not need user plugins, model catalog downloads,
            // or an update check. Keeping them out makes this a local storage
            // operation instead of an accidental network/plugin lifecycle.
            .env("OPENCODE_PURE", "1")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1");
        command
    }

    fn command_detail(output: &Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match (stdout.trim(), stderr.trim()) {
            ("", "") => format!("exit status {}", output.status),
            (stdout, "") => format!("exit status {}; stdout: {stdout}", output.status),
            ("", stderr) => format!("exit status {}; stderr: {stderr}", output.status),
            (stdout, stderr) => format!(
                "exit status {}; stdout: {stdout}; stderr: {stderr}",
                output.status
            ),
        };
        const LIMIT: usize = 2_000;
        if detail.len() <= LIMIT {
            detail
        } else {
            format!("{}...", &detail[..detail.floor_char_boundary(LIMIT)])
        }
    }

    fn workspace_for_write(session: &CanonicalSession) -> anyhow::Result<(PathBuf, Vec<String>)> {
        if let Some(workspace) = session.workspace.as_ref()
            && workspace.is_dir()
        {
            return Ok((workspace.clone(), Vec::new()));
        }

        let cwd =
            std::env::current_dir().context("could not determine a workspace for OpenCode")?;
        let warnings = session.workspace.as_ref().map_or_else(Vec::new, |workspace| {
            vec![format!(
                "The source workspace {} does not exist; OpenCode imported the session into {}.",
                workspace.display(),
                cwd.display()
            )]
        });
        Ok((cwd, warnings))
    }

    fn inferred_model(session: &CanonicalSession) -> (String, String) {
        let model = session
            .model_name
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or("big-pickle")
            .to_string();
        let lower = model.to_ascii_lowercase();
        let provider = if lower.contains("claude") {
            "anthropic"
        } else if lower.contains("gemini") {
            "google"
        } else if lower.contains("grok") {
            "xai"
        } else if lower.starts_with("gpt") || lower.starts_with('o') || lower.contains("codex") {
            "openai"
        } else {
            "opencode"
        };
        (provider.to_string(), model)
    }

    fn message_text(message: &CanonicalMessage) -> String {
        let mut text = message.content.clone();
        let mut append = |block: String| {
            if text.contains(&block) {
                return;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block);
        };

        for call in &message.tool_calls {
            append(format!("[Tool: {}]", call.name));
        }
        for result in &message.tool_results {
            if !result.content.trim().is_empty() && message.content == result.content {
                continue;
            }
            append(if result.is_error {
                format!("[Tool Error] {}", result.content)
            } else {
                format!("[Tool Output] {}", result.content)
            });
        }
        text
    }

    fn import_payload(
        session: &CanonicalSession,
        session_id: &str,
        workspace: &Path,
    ) -> serde_json::Value {
        let id_seed = session_id.trim_start_matches("ses_");
        let (provider_id, model_id) = Self::inferred_model(session);
        let fallback_time = chrono::Utc::now().timestamp_millis().max(0);
        let mut previous_time = session.started_at.unwrap_or(fallback_time).max(0);
        let mut previous_message_id: Option<String> = None;
        let mut last_user_id: Option<String> = None;
        let mut messages = Vec::with_capacity(session.messages.len());

        for (index, message) in session.messages.iter().enumerate() {
            let message_id = format!("msg_{id_seed}_{index:08}");
            let part_id = format!("prt_{id_seed}_{index:08}");
            let created = message.timestamp.unwrap_or(previous_time).max(0);
            let created = created.max(previous_time);
            previous_time = created;
            let text = Self::message_text(message);
            let parts = if text.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({
                    "id": part_id,
                    "sessionID": session_id,
                    "messageID": message_id,
                    "type": "text",
                    "text": text,
                })]
            };

            let is_assistant = message.role == MessageRole::Assistant;
            let info = if is_assistant {
                let parent_id = last_user_id
                    .as_ref()
                    .or(previous_message_id.as_ref())
                    .cloned()
                    .unwrap_or_else(|| format!("msg_{id_seed}_parent"));
                serde_json::json!({
                    "id": message_id,
                    "sessionID": session_id,
                    "role": "assistant",
                    "time": {"created": created, "completed": created},
                    "parentID": parent_id,
                    "modelID": model_id,
                    "providerID": provider_id,
                    "mode": "build",
                    "agent": "build",
                    "path": {
                        "cwd": workspace.display().to_string(),
                        "root": workspace.display().to_string(),
                    },
                    "cost": 0,
                    "tokens": {
                        "input": 0,
                        "output": 0,
                        "reasoning": 0,
                        "cache": {"read": 0, "write": 0},
                    },
                    "finish": "stop",
                })
            } else {
                last_user_id = Some(message_id.clone());
                serde_json::json!({
                    "id": message_id,
                    "sessionID": session_id,
                    "role": "user",
                    "time": {"created": created},
                    "agent": "build",
                    "model": {
                        "providerID": provider_id,
                        "modelID": model_id,
                    },
                })
            };

            previous_message_id = Some(message_id);
            messages.push(serde_json::json!({"info": info, "parts": parts}));
        }

        let title = session
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                session
                    .messages
                    .iter()
                    .find(|message| message.role == MessageRole::User)
                    .map(|message| truncate_title(&message.content, 80))
                    .filter(|title| !title.is_empty())
            })
            .unwrap_or_else(|| "Imported session".to_string());
        let created = session.started_at.unwrap_or(fallback_time).max(0);
        let updated = session
            .ended_at
            .unwrap_or(previous_time)
            .max(created)
            .max(previous_time);

        serde_json::json!({
            "info": {
                "id": session_id,
                "slug": format!("ags-{}", &id_seed[..id_seed.len().min(12)]),
                "projectID": "global",
                "directory": workspace.display().to_string(),
                "title": title,
                "version": env!("CARGO_PKG_VERSION"),
                "metadata": {
                    "importedBy": "ags",
                    "sourceProvider": session.provider_slug,
                },
                "time": {"created": created, "updated": updated},
            },
            "messages": messages,
        })
    }

    fn session_exists_in_db(db_path: &Path, session_id: &str) -> anyhow::Result<bool> {
        if !db_path.is_file() {
            return Ok(false);
        }
        let conn = Self::open_db(db_path)?;
        Ok(Self::session_exists(&conn, session_id))
    }

    fn delete_imported_session(
        binary: &Path,
        db_path: &Path,
        session_id: &str,
        cwd: &Path,
    ) -> anyhow::Result<()> {
        if !Self::session_exists_in_db(db_path, session_id)? {
            return Ok(());
        }

        let output = Self::command(binary, db_path, cwd)
            .args(["session", "delete", session_id])
            .output()
            .with_context(|| format!("failed to start {}", binary.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "OpenCode could not delete imported session {session_id}: {}",
                Self::command_detail(&output)
            );
        }
        if Self::session_exists_in_db(db_path, session_id)? {
            anyhow::bail!(
                "OpenCode reported deletion success, but session {session_id} remains in {}",
                db_path.display()
            );
        }
        Ok(())
    }

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

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
            .and_then(|mut stmt| stmt.exists(rusqlite::params![table]))
            .unwrap_or(false)
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

        match Self::binary_path() {
            Ok(binary) => {
                installed = true;
                evidence.push(format!("opencode binary found at {}", binary.display()));
            }
            Err(error) if std::env::var_os(OPENCODE_BIN_ENV).is_some() => {
                evidence.push(format!("{OPENCODE_BIN_ENV} is unusable: {error:#}"));
            }
            Err(_) => {}
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
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let binary = Self::binary_path().map_err(|_| anyhow::anyhow!("{OPENCODE_CLI_REQUIRED}"))?;
        let db_path = Self::write_db_path()?;
        let (workspace, warnings) = Self::workspace_for_write(session)?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create OpenCode data directory {}",
                    parent.display()
                )
            })?;
        }

        let session_id = format!("ses_{}", uuid::Uuid::new_v4().simple());
        let payload = Self::import_payload(session, &session_id, &workspace);
        let mut import_file = tempfile::Builder::new()
            .prefix("casr-opencode-import-")
            .suffix(".json")
            .tempfile()
            .context("failed to create temporary OpenCode import file")?;
        serde_json::to_writer(&mut import_file, &payload)
            .context("failed to serialize OpenCode import data")?;
        import_file
            .flush()
            .context("failed to flush OpenCode import data")?;
        import_file
            .as_file()
            .sync_all()
            .context("failed to sync OpenCode import data")?;

        let output = Self::command(&binary, &db_path, &workspace)
            .arg("import")
            .arg(import_file.path())
            .output()
            .with_context(|| format!("failed to start {}", binary.display()))?;

        if !output.status.success() {
            let import_error = Self::command_detail(&output);
            let rollback =
                Self::delete_imported_session(&binary, &db_path, &session_id, &workspace);
            return Err(match rollback {
                Ok(()) => anyhow::anyhow!("OpenCode import failed: {import_error}"),
                Err(error) => anyhow::anyhow!(
                    "OpenCode import failed: {import_error}; partial-session cleanup failed: {error:#}"
                ),
            });
        }

        let readback = Self::open_db(&db_path)
            .and_then(|conn| Self::read_session_by_id(&conn, &db_path, &session_id));
        if let Err(read_error) = readback {
            let rollback =
                Self::delete_imported_session(&binary, &db_path, &session_id, &workspace);
            return Err(match rollback {
                Ok(()) => anyhow::anyhow!(
                    "OpenCode imported session {session_id}, but its native store could not be read back: {read_error:#}; rollback succeeded"
                ),
                Err(error) => anyhow::anyhow!(
                    "OpenCode imported session {session_id}, but its native store could not be read back: {read_error:#}; rollback failed: {error:#}"
                ),
            });
        }

        let virtual_path = Self::virtual_session_path(&db_path, &session_id);
        Ok(WrittenSession {
            paths: vec![virtual_path],
            session_id: session_id.clone(),
            resume_command: Self::resume_spec(&session_id).display(),
            backups: Vec::new(),
            warnings,
        })
    }

    fn rollback_write(&self, written: &WrittenSession) -> anyhow::Result<()> {
        let locator = written
            .paths
            .first()
            .context("OpenCode rollback has no virtual session locator")?;
        let (db_path, locator_session_id) = Self::parse_virtual_path(locator)
            .context("OpenCode rollback received an invalid virtual session locator")?;
        if locator_session_id != written.session_id {
            anyhow::bail!(
                "OpenCode rollback locator names {locator_session_id}, but the write names {}",
                written.session_id
            );
        }
        let binary = Self::binary_path()?;
        let cwd = std::env::current_dir().context("could not determine OpenCode rollback cwd")?;
        Self::delete_imported_session(&binary, &db_path, &written.session_id, &cwd)
    }

    fn write_refusal(&self) -> Option<&'static str> {
        Self::binary_path()
            .is_err()
            .then_some(OPENCODE_CLI_REQUIRED)
    }

    fn resume_command(&self, session_id: &str) -> String {
        Self::resume_spec(session_id).display()
    }

    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        Some(Self::resume_spec(session_id).targeting_session(session_id))
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

/// Reader tests use only isolated fixture databases. Target-side tests live in
/// `tests/opencode_write_test.rs`; the vendor-backed write probe is gated on an
/// explicit official binary so ordinary test runs never touch a real store.
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
            "opencode --session sid"
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
        let conn = Connection::open(path).expect("create db");
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
        let conn = Connection::open(&legacy).expect("create");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT);
             CREATE TABLE messages (id TEXT);
             CREATE TABLE files (id TEXT);",
        )
        .expect("legacy schema");
        drop(conn);
        let conn = OpenCode::open_db(&legacy).expect("open");
        assert_eq!(OpenCode::detect_schema(&conn), Some(Schema::Legacy));

        // A database that is neither is reported as neither, not as empty.
        let foreign = tmp.path().join("foreign.db");
        let conn = Connection::open(&foreign).expect("create");
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
        let conn = Connection::open(&foreign).expect("create");
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
}
