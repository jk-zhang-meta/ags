//! Cursor AI provider — reads sessions from SQLite `state.vscdb` databases.
//!
//! Cursor stores conversations in SQLite databases under its config directory:
//! - Linux: `~/.config/Cursor/User/globalStorage/state.vscdb`
//! - macOS: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
//! - Windows: `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
//!
//! ## Storage format
//!
//! Two tables are used:
//! - `cursorDiskKV` (modern, v0.40+) — key-value store where:
//!   - `composerData:<uuid>` → session metadata + message ordering
//!   - `bubbleId:<composerId>:<bubbleId>` → individual message data
//! - `ItemTable` (legacy, v0.2x–v0.3x) — key-value store for older AI chat data
//!
//! ## Message types
//!
//! - Numeric type `1` = User, `2` = Assistant (v0.40+ format)
//! - String type `"user"/"human"` = User, `"assistant"/"ai"/"bot"` = Assistant
//!
//! ## Content extraction priority
//!
//! `text` > `rawText` > `content` > `message` (first non-empty wins)
//!
//! ## Resume mechanism
//!
//! Cursor's IDE composer store has no verified direct resume command, so the
//! resume command opens the workspace in Cursor (`cursor <workspace-path>`).
//! `cursor-agent --resume <id>` does exist, but it addresses the separate
//! `~/.cursor` agent store described below; it cannot open a composer ags
//! reads from the IDE's `state.vscdb`.
//!
//! ## Writing
//!
//! A composer visible in Cursor's UI requires the workbench-managed
//! `allComposers` index as well as the `composerData` and `bubbleId` records.
//! ags can read the latter, but has no vendor-authoritative way to update the
//! shared index without risking an invisible or damaged composer. Cursor is
//! therefore read/resume-only until that lifecycle is verified end to end.
//!
//! # The second store: `cursor-agent`
//!
//! `cursor-agent` (the `agent` CLI) is a separate product with a separate root
//! — `~/.cursor`, not `~/.config/Cursor` — and none of its sessions are in
//! `state.vscdb`. It keeps each conversation in two places:
//!
//! ```text
//! <configDir>/chats/<md5(cwd)>/<id>/store.db                       ← agent state
//! <dataDir>/projects/<slug>/agent-transcripts/<id>/<id>.jsonl      ← transcript
//! ```
//!
//! `<configDir>` is `$CURSOR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cursor`, else
//! `~/.cursor`; `<dataDir>` is `$CURSOR_DATA_DIR`, else `~/.cursor`. Both
//! resolvers are the CLI's own, so a user who moved either directory is still
//! read correctly. `<slug>` is the absolute workspace path with every
//! non-alphanumeric run collapsed to `-`, sometimes truncated with a sha256
//! suffix; the reader globs `projects/*` instead of recomputing it, so which of
//! the two namers ran does not matter.
//!
//! ## `store.db`
//!
//! ```sql
//! CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
//! CREATE TABLE meta  (key TEXT PRIMARY KEY, value TEXT);
//! ```
//!
//! `meta['0']` is the hex of UTF-8 JSON holding `{agentId, name, createdAt,
//! mode, lastUsedModel, latestRootBlobId, …}`. Only `name`, `createdAt` and
//! `lastUsedModel` are read, and only those three are republished: the object
//! also carries `blobEncryptionKey`, a live credential — see
//! [`Cursor::cli_chat_metadata`].
//!
//! The conversation itself is a content-addressed blob graph rooted at
//! `latestRootBlobId`. **This reader does not decode it**, which is not the
//! same as it being undecodable: the root blob is protobuf
//! (`agent.v1.ConversationStateStructure`), but its field 1
//! `root_prompt_messages_json` is `repeated bytes` holding *blob ids*, and the
//! blobs those ids name are plain JSON — `cursor-agent` reads them back with
//! `JSON.parse`. Nothing is encrypted at rest: `setBlob` writes
//! `Buffer.from(data)` straight into the `blobs` column, and
//! `blobEncryptionKey` is a request header for Cursor's backend, not a local
//! cipher. The CLI's own `generateTranscript` reconstructs a whole transcript
//! from one `store.db` and nothing else. Decoding it here is a feature nobody
//! has written, not a wall.
//!
//! ## `<id>.jsonl`
//!
//! One JSON object per line. Conversation lines are
//! `{"role": …, "message": {"content": [{"type":"text","text":…} |
//! {"type":"tool_use","name":…,"input":…}]}}`; `{"type":"metadata"}` and
//! `{"type":"turn_ended"}` lines carry no message. There are no timestamps and
//! no workspace path in the file — see [`Cursor::read_cli_transcript`].
//!
//! ## De-duplication
//!
//! The conversation id is the key, and it is the only key the two halves share:
//! `cursor-agent` uses the same string for the chat directory name and for the
//! transcript filename. A conversation with both halves is listed once, from
//! its transcript, enriched with its `store.db` metadata. A chat with no
//! transcript — created but never prompted, or written by a build that did not
//! write one — cannot be rendered, so it is listed as a candidate and reported
//! through `list`'s `skipped` channel rather than dropped.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags};
use tracing::{debug, trace, warn};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, flatten_content, normalize_role,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession, read_dir_reporting,
    store_evidence, walk_entry_reporting,
};

/// Cursor AI provider implementation.
pub struct Cursor;

const CURSOR_WRITE_REFUSAL: &str = "Cursor is read/resume-only: writing composerData and bubbleId records without Cursor's allComposers index creates a session the IDE cannot show. A safe import needs Cursor's own composer lifecycle; use Cursor as a conversion source, not a target.";

// ---------------------------------------------------------------------------
// Bubble type constants (v0.40+ numeric message types)
// ---------------------------------------------------------------------------

/// A bubble's `type` is `aiserver.v1.ConversationMessage.MessageType`, and the
/// whole vocabulary is three values. From the shipped Cursor 3.13.10 bundle,
/// `resources/app/out/vs/workbench/workbench.desktop.main.js`, verbatim:
///
/// ```js
/// yo = L.makeEnum("aiserver.v1.ConversationMessage.MessageType", [
///   { no: 0, name: "MESSAGE_TYPE_UNSPECIFIED", localName: "UNSPECIFIED" },
///   { no: 1, name: "MESSAGE_TYPE_HUMAN",       localName: "HUMAN"       },
///   { no: 2, name: "MESSAGE_TYPE_AI",          localName: "AI"          }])
/// ```
///
/// There is no system, developer, or tool member, and `UNSPECIFIED` is not a
/// third channel: `yo.UNSPECIFIED` appears zero times in that bundle, against
/// 132 uses of `yo.HUMAN` and 96 of `yo.AI`. A bubble that is neither is not
/// rendered neutrally, it is dropped — the function that groups a conversation
/// into turns pushes a bubble only on an explicit match:
///
/// ```js
/// l.type === yo.HUMAN ? s.messages.push(l)
///   : l.type === yo.AI && (o === void 0 && (o = {…}), o.messages.push(l))
/// ```
///
/// So every readable bubble has one of these two values. Cursor target imports
/// refuse before they would need to project another provider's roles onto them.
///
/// User message type in modern Cursor format.
const BUBBLE_TYPE_USER: i64 = 1;
/// Assistant message type in modern Cursor format.
const BUBBLE_TYPE_ASSISTANT: i64 = 2;

impl Cursor {
    /// Config directory for Cursor. Respects `CURSOR_HOME` env var override.
    fn config_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("CURSOR_HOME") {
            return Some(PathBuf::from(home));
        }
        #[cfg(target_os = "linux")]
        {
            dirs::config_dir().map(|c| c.join("Cursor"))
        }
        #[cfg(target_os = "macos")]
        {
            dirs::data_dir().map(|d| d.join("Cursor"))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            dirs::config_dir().map(|c| c.join("Cursor"))
        }
    }

    /// Find all `state.vscdb` files under the Cursor config directory,
    /// recording any directory it was refused.
    fn find_db_files_reporting(unreadable: &mut Vec<UnreadableSource>) -> Vec<PathBuf> {
        let Some(config_dir) = Self::config_dir() else {
            return vec![];
        };

        let mut dbs = Vec::new();

        // Global storage DB (most common location).
        let global_db = config_dir.join("User/globalStorage/state.vscdb");
        if global_db.is_file() {
            dbs.push(global_db);
        }

        // Workspace-specific DBs.
        let ws_storage = config_dir.join("User/workspaceStorage");
        for entry in read_dir_reporting(&ws_storage, unreadable) {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let candidate = entry.path().join("state.vscdb");
                if candidate.is_file() {
                    dbs.push(candidate);
                }
            }
        }

        dbs
    }

    /// `find_db_files_reporting` for the callers with nowhere to put a read
    /// failure — `detect`, `session_roots`, `owns_session`. None of those
    /// answers a question an unreadable `workspaceStorage` changes.
    fn find_db_files() -> Vec<PathBuf> {
        Self::find_db_files_reporting(&mut Vec::new())
    }

    /// `cursor-agent`'s config root, resolved exactly as the CLI resolves it.
    fn cli_config_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("CURSOR_CONFIG_DIR")
            && !dir.trim().is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
            && !dir.trim().is_empty()
        {
            return Some(PathBuf::from(dir).join("cursor"));
        }
        dirs::home_dir().map(|h| h.join(".cursor"))
    }

    /// `cursor-agent`'s data root. Note it does *not* consult `XDG_DATA_HOME`.
    fn cli_data_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("CURSOR_DATA_DIR")
            && !dir.trim().is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|h| h.join(".cursor"))
    }

    /// Every `cursor-agent` transcript, as `(conversation id, path)`.
    ///
    /// Three filename layouts exist — `<id>/<id>.jsonl`, the legacy flat
    /// `<id>.jsonl`, and `<parent>/subagents/<id>.jsonl`. In all three the file
    /// stem is the conversation id, so a recursive walk covers them without
    /// having to know which one produced a given file.
    fn cli_transcripts(unreadable: &mut Vec<UnreadableSource>) -> Vec<(String, PathBuf)> {
        let Some(projects) = Self::cli_data_dir().map(|d| d.join("projects")) else {
            return vec![];
        };
        let mut out = Vec::new();
        for project in read_dir_reporting(&projects, unreadable) {
            let transcripts = project.path().join("agent-transcripts");
            if !transcripts.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&transcripts).max_depth(3) {
                let Some(entry) = walk_entry_reporting(entry, unreadable) else {
                    continue;
                };
                let path = entry.path();
                if !entry.file_type().is_file()
                    || path.extension().and_then(|e| e.to_str()) != Some("jsonl")
                {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push((stem.to_string(), path.to_path_buf()));
                }
            }
        }
        out
    }

    /// Every `cursor-agent` chat store, as `(conversation id, store.db path)`.
    fn cli_chat_stores(unreadable: &mut Vec<UnreadableSource>) -> Vec<(String, PathBuf)> {
        let Some(chats) = Self::cli_config_dir().map(|d| d.join("chats")) else {
            return vec![];
        };
        let mut out = Vec::new();
        for bucket in read_dir_reporting(&chats, unreadable) {
            for chat in read_dir_reporting(&bucket.path(), unreadable) {
                let db = chat.path().join("store.db");
                if !db.is_file() {
                    continue;
                }
                if let Some(id) = chat.file_name().to_str() {
                    out.push((id.to_string(), db));
                }
            }
        }
        out
    }

    /// The fields of a chat's `meta['0']` this reader uses, if a store exists
    /// for `id`.
    ///
    /// Returns `None` rather than an error: this is enrichment, and a chat
    /// store that will not open must not cost the transcript its listing.
    ///
    /// The decoded object is filtered here, at the one place it is decoded,
    /// rather than at each use. `meta['0']` is a vendor blob whose contents are
    /// Cursor's to change, and it carries a live credential: every conversation
    /// is built from `cursor-agent`'s default metadata literal, which ends
    /// `subagentInfo: void 0, blobEncryptionKey: Q()` with `Q()` returning 32
    /// bytes from `crypto.getRandomValues`, hex-encoded. The CLI sends that
    /// value to Cursor's backend as the `x-blob-encryption-key` header. Copying
    /// the object wholesale republished it through `info --json`, a command
    /// users pipe to a file and paste into bug reports.
    ///
    /// So this is an allow-list, not a deny-list of the names that are secret
    /// today: a deny-list would have to be re-audited every time Cursor adds a
    /// field, and would leak until someone noticed.
    fn cli_chat_metadata(id: &str) -> Option<serde_json::Value> {
        /// Everything the reader asks of a chat store: the title, the creation
        /// time and the model. Adding a name here republishes it.
        const USED_FIELDS: [&str; 3] = ["name", "createdAt", "lastUsedModel"];

        let (_, db) = Self::cli_chat_stores(&mut Vec::new())
            .into_iter()
            .find(|(k, _)| k == id)?;
        let conn = Self::open_db(&db).ok()?;
        let hex: String = conn
            .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
                row.get(0)
            })
            .ok()?;
        let full: serde_json::Value = serde_json::from_slice(&decode_hex(&hex)?).ok()?;

        let mut kept = serde_json::Map::new();
        for field in USED_FIELDS {
            if let Some(value) = full.get(field) {
                kept.insert(field.to_string(), value.clone());
            }
        }
        Some(serde_json::Value::Object(kept))
    }

    /// Build a virtual per-session path backed by a `state.vscdb` file.
    ///
    /// Format: `<db_path>/<urlencoded-composer-id>`
    fn virtual_session_path(db_path: &Path, composer_id: &str) -> PathBuf {
        let encoded = urlencoding::encode(composer_id);
        db_path.join(encoded.as_ref())
    }

    /// Open a SQLite database read-only with a busy timeout.
    fn open_db(path: &Path) -> anyhow::Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open Cursor DB: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    #[cfg(test)]
    fn open_db_rw(path: &Path) -> anyhow::Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open Cursor DB for test: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Check if a table exists in the database.
    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
            .and_then(|mut stmt| stmt.exists(rusqlite::params![table]))
            .unwrap_or(false)
    }

    /// List all composer IDs from the cursorDiskKV table.
    /// Every composer id in `conn`, recording a query that failed rather than
    /// answering it with an empty list.
    ///
    /// A database with no `cursorDiskKV` table is not a failure — that is an
    /// ordinary workspace database that holds no composer data — so only real
    /// query errors are reported. Without the distinction the listing would
    /// carry one line per workspace database on every run.
    fn list_composer_ids_reporting(
        conn: &Connection,
        db_path: &Path,
        unreadable: &mut Vec<UnreadableSource>,
    ) -> Vec<String> {
        let mut report = |error: String| {
            unreadable.push(UnreadableSource {
                path: db_path.to_path_buf(),
                error,
            });
        };

        // Three outcomes, not two. The table being absent is ordinary — a
        // workspace database that holds no composer data — and stays quiet.
        // The *lookup* failing is not: it is what a `state.vscdb` that is
        // corrupt, truncated, or not a SQLite file at all does, and
        // `unwrap_or(false)` made that indistinguishable from an empty store.
        match conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
            .and_then(|mut stmt| stmt.exists(rusqlite::params!["cursorDiskKV"]))
        {
            Ok(true) => {}
            Ok(false) => return vec![],
            Err(e) => {
                report(format!("could not read the database schema: {e}"));
                return vec![];
            }
        }

        let mut stmt =
            match conn.prepare("SELECT key FROM cursorDiskKV WHERE key LIKE 'composerData:%'") {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "failed to query composerData keys");
                    report(format!("composer query failed: {e}"));
                    return vec![];
                }
            };

        let rows = match stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            Ok(key
                .strip_prefix("composerData:")
                .unwrap_or(&key)
                .to_string())
        }) {
            Ok(rows) => rows,
            Err(e) => {
                report(format!("composer query failed: {e}"));
                return vec![];
            }
        };

        let mut ids = Vec::new();
        for row in rows {
            match row {
                Ok(id) => ids.push(id),
                Err(e) => report(format!("composer row could not be read: {e}")),
            }
        }
        ids
    }

    /// `list_composer_ids_reporting` for the callers that answer a question a
    /// failed query does not change — `owns_session` and the readers, for
    /// which "no such composer here" is already the outcome.
    fn list_composer_ids(conn: &Connection) -> Vec<String> {
        Self::list_composer_ids_reporting(conn, Path::new("<unknown>"), &mut Vec::new())
    }

    /// Fetch bubble data for a composer using range query optimization.
    ///
    /// Key format: `bubbleId:{composerId}:{bubbleId}`
    /// Uses range query `key >= prefix AND key < prefix_upper` for index leverage.
    fn fetch_bubbles(
        conn: &Connection,
        composer_id: &str,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let prefix = format!("bubbleId:{composer_id}:");
        // Increment last char for upper bound.
        let prefix_upper = format!("bubbleId:{composer_id};");

        let mut bubbles = std::collections::HashMap::new();

        let mut stmt = match conn
            .prepare("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to query bubble data");
                return bubbles;
            }
        };

        let rows = match stmt.query_map(rusqlite::params![prefix, prefix_upper], |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to fetch bubble rows");
                return bubbles;
            }
        };

        for row in rows.flatten() {
            let (key, value_str) = row;
            // Extract bubble ID from key.
            let bubble_id = key.strip_prefix(&prefix).unwrap_or(&key);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&value_str) {
                bubbles.insert(bubble_id.to_string(), val);
            }
        }

        bubbles
    }

    /// Read one `cursor-agent` transcript.
    ///
    /// The file carries roles, text, thinking and tool calls, and nothing else:
    /// no per-message timestamps, no model, and no workspace path. The chat's
    /// `store.db` supplies the title, creation time and model when it is still
    /// there; the workspace is supplied by nothing. Neither directory name that
    /// could name it is invertible, so the workspace stays `None` rather than
    /// being guessed.
    ///
    /// They are uninvertible for two different reasons, and only one of them is
    /// a hash. `chats/<md5>` is `md5(resolve(cwd))`, genuinely one-way. The
    /// `projects/<slug>` slug is not a hash at all — it is
    /// `path.replace(/[^a-zA-Z0-9]/g, "-")` with runs collapsed — but it is
    /// still ambiguous, because `/home/u/demo project`, `/home/u/demo_project`
    /// and `/home/u/demo.project` all slugify to `home-u-demo-project`, and
    /// nothing records which one it was. The CLI also has a second namer that
    /// truncates at 84 characters and appends a 7-hex sha256 prefix, so a long
    /// enough path is partly hashed as well.
    fn read_cli_transcript(path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading cursor-agent transcript");

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut overview: Option<String> = None;
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    // The writer appends, so an interrupted run leaves a
                    // partial last line; the turns before it are intact.
                    trace!(line = lineno, error = %e, "skipping unparseable transcript line");
                    continue;
                }
            };
            // `{"type":"metadata"}` / `{"type":"turn_ended"}` lines are not
            // messages. The overview is the CLI's own summary of the
            // conversation, so it is kept as the title candidate.
            if let Some(kind) = record.get("type").and_then(|v| v.as_str()) {
                if kind == "metadata" {
                    overview = record
                        .pointer("/metadata/overview")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                }
                continue;
            }
            if let Some(msg) = parse_transcript_line(&record) {
                messages.push(msg);
            }
        }

        reindex_messages(&mut messages);

        let chat_meta = Self::cli_chat_metadata(&session_id);
        let meta_str = |key: &str| {
            chat_meta
                .as_ref()
                .and_then(|m| m.get(key))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(ToString::to_string)
        };

        let title = meta_str("name")
            .or(overview)
            .map(|t| truncate_title(&t, 100))
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });
        let started_at = chat_meta
            .as_ref()
            .and_then(|m| m.get("createdAt"))
            .and_then(parse_timestamp);

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("cursor".to_string()),
        );
        metadata.insert(
            "cursor_store".into(),
            serde_json::Value::String("cursor-agent".to_string()),
        );
        if let Some(chat) = chat_meta.clone() {
            metadata.insert("cursor_agent_chat".into(), chat);
        }
        // A subagent transcript lives at `<parent>/subagents/<id>.jsonl`, so
        // the path is the only record of whose subagent it was. It is a real
        // session and stays listed as one, but the lineage is right there and
        // dropping it would lose something the store actually said.
        if let Some(parent) = subagent_parent_id(path) {
            metadata.insert(
                "cursor_agent_parent_id".into(),
                serde_json::Value::String(parent),
            );
        }

        debug!(
            session_id,
            messages = messages.len(),
            "cursor-agent transcript parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "cursor".to_string(),
            // Not determinable from either store; see the doc comment.
            workspace: None,
            title,
            started_at,
            // The transcript has no timestamps at all, and inventing the file's
            // mtime here would make it indistinguishable from a recorded one.
            ended_at: None,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name: meta_str("lastUsedModel"),
        })
    }

    /// Read a single session from a composerData entry.
    fn read_composer_session(
        conn: &Connection,
        composer_id: &str,
        db_path: &Path,
    ) -> anyhow::Result<CanonicalSession> {
        // Fetch the composerData entry.
        let composer_json: String = conn
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                rusqlite::params![format!("composerData:{composer_id}")],
                |row| row.get(0),
            )
            .with_context(|| format!("composerData not found for {composer_id}"))?;

        let composer: serde_json::Value =
            serde_json::from_str(&composer_json).context("invalid composerData JSON")?;

        // Fetch all bubbles for this composer.
        let bubbles = Self::fetch_bubbles(conn, composer_id);

        Self::parse_composer(composer_id, &composer, &bubbles, db_path)
    }

    /// The `modelConfig` sub-object of a `composerData:<uuid>` entry, reduced
    /// to the fields Cursor is known to put there.
    ///
    /// Cursor 3.13.10 builds it in one place, and only one — `Gyr(t)` in
    /// `resources/app/out/vs/workbench/workbench.desktop.main.js`:
    ///
    /// ```js
    /// function Gyr(t){return{modelName:t.modelName,maxMode:t.maxMode,
    ///   ...t.useExperimentalModelOptOut===!0?{useExperimentalModelOptOut:!0}:{},
    ///   selectedModels:t.selectedModels?.map(e=>({modelId:e.modelId,
    ///     parameters:e.parameters.map(n=>({...n}))}))}}
    /// ```
    ///
    /// `modelConfig` is absent from the persist function's own field list and
    /// arrives only through the default-object spread, so `Gyr` re-runs on
    /// every write: the four names below are the whole set, and none of them
    /// is a credential. Cursor's BYOK material lives elsewhere entirely — OS
    /// secret storage (`cursorAuth/openAIKey`, …) and the `ItemTable` reactive
    /// storage blob (`openAIBaseUrl`, `azureState.apiKey`, `bedrockState.*`) —
    /// neither of which this reader touches.
    ///
    /// So this filter removes nothing that ships today. It is here for the
    /// same reason [`Cursor::cli_chat_metadata`] is: `ags convert info --json` prints
    /// the metadata bag verbatim, and the entry this object came from *does*
    /// carry live secrets one level up — `blobEncryptionKey` and
    /// `speculativeSummarizationEncryptionKey`, both 32 bytes of
    /// `crypto.getRandomValues`, sitting at the top level of the same
    /// `composerData` value. Copying any part of that value wholesale is the
    /// habit that published the first one. Adding a name here republishes it.
    fn composer_model_config(composer: &serde_json::Value) -> Option<serde_json::Value> {
        const USED_FIELDS: [&str; 4] = [
            "modelName",
            "maxMode",
            "useExperimentalModelOptOut",
            "selectedModels",
        ];

        let full = composer.get("modelConfig")?;
        // A non-object `modelConfig` is not something this reader can vouch
        // for, and passing it through is exactly the wholesale copy.
        let full = full.as_object()?;

        let mut kept = serde_json::Map::new();
        for field in USED_FIELDS {
            if let Some(value) = full.get(field) {
                kept.insert(field.to_string(), value.clone());
            }
        }
        Some(serde_json::Value::Object(kept))
    }

    /// Parse a composerData entry + bubbles into a CanonicalSession.
    fn parse_composer(
        composer_id: &str,
        composer: &serde_json::Value,
        bubbles: &std::collections::HashMap<String, serde_json::Value>,
        source_path: &Path,
    ) -> anyhow::Result<CanonicalSession> {
        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut model_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        // Session-level timestamps.
        if let Some(ts) = composer.get("createdAt").and_then(parse_timestamp) {
            started_at = Some(ts);
        }
        if let Some(ts) = composer.get("lastUpdatedAt").and_then(parse_timestamp) {
            ended_at = Some(ts);
        }

        // Extract workspace from bubbles.
        let workspace = extract_workspace_from_bubbles(bubbles)
            .or_else(|| extract_workspace_from_composer(composer));

        // Try modern v0.40+ format: fullConversationHeadersOnly.
        if let Some(headers) = composer
            .get("fullConversationHeadersOnly")
            .and_then(|v| v.as_array())
        {
            for header in headers {
                let bubble_id = header
                    .get("bubbleId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if bubble_id.is_empty() {
                    continue;
                }

                let bubble = match bubbles.get(bubble_id) {
                    Some(b) => b,
                    None => {
                        trace!(bubble_id, "bubble not found in fetched data");
                        continue;
                    }
                };

                if let Some(msg) =
                    parse_bubble(bubble, &mut model_counts, &mut started_at, &mut ended_at)
                {
                    messages.push(msg);
                }
            }
        }
        // Fallback: v0.3x tabs format.
        else if let Some(tabs) = composer.get("tabs").and_then(|v| v.as_array()) {
            for tab in tabs {
                if let Some(tab_bubbles) = tab.get("bubbles").and_then(|v| v.as_array()) {
                    for bubble in tab_bubbles {
                        if let Some(msg) =
                            parse_bubble(bubble, &mut model_counts, &mut started_at, &mut ended_at)
                        {
                            messages.push(msg);
                        }
                    }
                }
            }
        }
        // Fallback: v0.2x conversationMap format.
        else if let Some(conv_map) = composer.get("conversationMap").and_then(|v| v.as_object()) {
            for (_conv_id, conv) in conv_map {
                if let Some(conv_bubbles) = conv.get("bubbles").and_then(|v| v.as_array()) {
                    for bubble in conv_bubbles {
                        if let Some(msg) =
                            parse_bubble(bubble, &mut model_counts, &mut started_at, &mut ended_at)
                        {
                            messages.push(msg);
                        }
                    }
                }
            }
        }
        // Fallback: simple single-entry format.
        else if let Some(content) = extract_bubble_content(composer)
            && !content.trim().is_empty()
        {
            messages.push(CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content,
                timestamp: started_at,
                author: None,
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                // `composerData` now carries live encryption keys at its top
                // level. This fallback already lifts every field it uses, so
                // copying the vendor object would only republish unknown data.
                extra: serde_json::Value::Null,
            });
        }

        reindex_messages(&mut messages);

        // Derive title.
        let session_title = composer
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });

        // Most common model name.
        let model_name = model_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| name);

        // Build metadata.
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("cursor".to_string()),
        );
        if let Some(model_config) = Self::composer_model_config(composer) {
            metadata.insert("modelConfig".into(), model_config);
        }
        if let Some(mode) = composer.get("unifiedMode").and_then(|v| v.as_str()) {
            metadata.insert(
                "unifiedMode".into(),
                serde_json::Value::String(mode.to_string()),
            );
        }

        // Unique source path: db_path/composer_id for dedup.
        let source = Self::virtual_session_path(source_path, composer_id);

        debug!(
            composer_id,
            messages = messages.len(),
            "Cursor session parsed"
        );

        Ok(CanonicalSession {
            session_id: composer_id.to_string(),
            provider_slug: "cursor".to_string(),
            workspace,
            title: session_title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: source,
            model_name,
        })
    }
}

impl Provider for Cursor {
    fn name(&self) -> &str {
        "Cursor"
    }

    fn slug(&self) -> &str {
        "cursor"
    }

    fn cli_alias(&self) -> &str {
        "cur"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        // Check for binary in PATH.
        for bin in ["cursor", "cursor-agent"] {
            if which::which(bin).is_ok() {
                evidence.push(format!("{bin} binary found in PATH"));
                installed = true;
            }
        }

        // Check for config directory.
        if let Some(config) = Self::config_dir()
            && config.is_dir()
        {
            evidence.push(format!("{} exists", config.display()));
            installed = true;
        }

        // `cursor-agent` has its own root, and installing it is the one thing
        // that creates it. Reporting it keeps detection matched to the two
        // stores this provider reads.
        for dir in [Self::cli_config_dir(), Self::cli_data_dir()] {
            if let Some(dir) = dir
                && dir.is_dir()
                && !evidence
                    .iter()
                    .any(|e| e.starts_with(&dir.display().to_string()))
            {
                evidence.push(format!("{} exists", dir.display()));
                installed = true;
            }
        }

        // Always, including zero. "Cursor is installed" and "ags found a
        // database to read" are different facts, and reporting the count only
        // when it is non-zero made the interesting case the silent one.
        if installed {
            let dbs = Self::find_db_files();
            evidence.push(format!("found {} state.vscdb database(s)", dbs.len()));
            for dir in [
                Self::cli_data_dir().map(|d| d.join("projects")),
                Self::cli_config_dir().map(|d| d.join("chats")),
            ]
            .into_iter()
            .flatten()
            {
                evidence.push(store_evidence(&dir));
            }
        }

        trace!(provider = "cursor", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let mut roots = Self::find_db_files();
        for dir in [
            Self::cli_data_dir().map(|d| d.join("projects")),
            Self::cli_config_dir().map(|d| d.join("chats")),
        ]
        .into_iter()
        .flatten()
        {
            if dir.is_dir() && !roots.contains(&dir) {
                roots.push(dir);
            }
        }
        roots
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        if let Some((_, path)) = Self::cli_transcripts(&mut Vec::new())
            .into_iter()
            .find(|(id, _)| id == session_id)
        {
            return Some(path);
        }
        if let Some((_, path)) = Self::cli_chat_stores(&mut Vec::new())
            .into_iter()
            .find(|(id, _)| id == session_id)
        {
            return Some(path);
        }
        for db_path in Self::find_db_files() {
            if let Ok(conn) = Self::open_db(&db_path) {
                let ids = Self::list_composer_ids(&conn);
                if ids.iter().any(|id| id == session_id) {
                    let virtual_path = Self::virtual_session_path(&db_path, session_id);
                    debug!(
                        db = %db_path.display(),
                        session_path = %virtual_path.display(),
                        session_id,
                        "found Cursor session"
                    );
                    return Some(virtual_path);
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Cursor session");

        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            return Self::read_cli_transcript(path);
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("store.db") {
            // Deliberately an error, not an empty session: this chat has real
            // turns and this reader cannot see them. `list` carries it in
            // `skipped` so the count the user reads is honest, where a
            // zero-message row would silently claim the conversation is empty.
            let id = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            anyhow::bail!(
                "cursor-agent chat {id}: its turns live in this store's `blobs` table, \
                 rooted at a protobuf record this reader does not decode, and no \
                 transcript for it exists under `projects/*/agent-transcripts/`"
            );
        }

        // The path may be a DB file directly, or a DB file with an appended composer ID.
        // Check if the path itself is a real file (SQLite DB).
        if path.is_file() && path.extension().is_some_and(|ext| ext == "vscdb") {
            // Read the first (or only) session from this DB.
            let conn = Self::open_db(path)?;
            let ids = Self::list_composer_ids(&conn);
            if let Some(first_id) = ids.first() {
                return Self::read_composer_session(&conn, first_id, path);
            }

            // Fallback: try ItemTable (legacy).
            return read_legacy_session(&conn, path);
        }

        // Path might be "db_path/encoded_composer_id" — virtual path from discovery.
        let parent = path.parent();
        let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

        if let Some(parent_path) = parent
            && parent_path.is_file()
        {
            let composer_id = urlencoding::decode(filename)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| filename.to_string());
            let conn = Self::open_db(parent_path)?;
            return Self::read_composer_session(&conn, &composer_id, parent_path);
        }

        // Last resort: try opening as a DB directly.
        let conn = Self::open_db(path)?;
        let ids = Self::list_composer_ids(&conn);
        if let Some(first_id) = ids.first() {
            return Self::read_composer_session(&conn, first_id, path);
        }

        anyhow::bail!("no Cursor sessions found in {}", path.display())
    }

    fn write_session(
        &self,
        _session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        Err(anyhow::anyhow!(CURSOR_WRITE_REFUSAL))
    }

    fn write_refusal(&self) -> Option<&'static str> {
        Some(CURSOR_WRITE_REFUSAL)
    }

    fn resume_command(&self, _session_id: &str) -> String {
        // `cursor-agent --resume` targets the separate agent CLI store, not
        // this writer's IDE composer in state.vscdb. Best we can do for the
        // composer is open Cursor.
        "cursor .".to_string()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let mut listing = SessionListing::default();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        for db_path in &Self::find_db_files_reporting(&mut listing.unreadable) {
            let conn = match Self::open_db(db_path) {
                Ok(conn) => conn,
                Err(err) => {
                    // `find_db_files_reporting` only returns databases that
                    // exist, so this is a store ags found and could not open.
                    listing.unreadable.push(UnreadableSource {
                        path: db_path.clone(),
                        error: format!("{err:#}"),
                    });
                    continue;
                }
            };

            for id in Self::list_composer_ids_reporting(&conn, db_path, &mut listing.unreadable) {
                let virtual_path = Self::virtual_session_path(db_path, &id);
                if seen.insert(id.clone()) {
                    listing.sessions.push((id, virtual_path));
                }
            }
        }

        // `cursor-agent`. Transcripts first so a conversation that has both
        // halves is listed from the half that can actually be read; the chat
        // store then contributes only what the transcript is missing.
        for (id, path) in Self::cli_transcripts(&mut listing.unreadable) {
            if seen.insert(id.clone()) {
                listing.sessions.push((id, path));
            }
        }
        for (id, path) in Self::cli_chat_stores(&mut listing.unreadable) {
            if seen.insert(id.clone()) {
                listing.sessions.push((id, path));
            }
        }

        Some(listing)
    }

    /// Three stores, three shapes: the IDE's `state.vscdb`, `cursor-agent`'s
    /// `agent-transcripts/**/<id>.jsonl`, and its `chats/<bucket>/<id>/store.db`.
    fn is_session_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("vscdb" | "jsonl" | "db")
        )
    }
}

// ---------------------------------------------------------------------------
// Bubble parsing helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// cursor-agent transcript helpers
// ---------------------------------------------------------------------------

/// Decode a lowercase-or-upper hex string. `None` on any non-hex input.
///
/// Six lines beats a dependency for the one place a hex string is read.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// The parent conversation id of a subagent transcript, from its path.
///
/// `agent-transcripts/<parentId>/subagents/<id>.jsonl` is the only layout that
/// records one; the two primary layouts return `None`.
fn subagent_parent_id(path: &Path) -> Option<String> {
    let subagents = path.parent()?;
    if subagents.file_name()? != "subagents" {
        return None;
    }
    subagents
        .parent()?
        .file_name()?
        .to_str()
        .map(ToString::to_string)
}

/// Parse one conversation line of a `cursor-agent` transcript.
///
/// `{"role": …, "message": {"content": [ … ]}}`. Returns `None` when the line
/// has no content blocks the canonical message can carry.
fn parse_transcript_line(record: &serde_json::Value) -> Option<CanonicalMessage> {
    let role_str = record.get("role").and_then(|v| v.as_str())?;
    let blocks = record.pointer("/message/content")?.as_array()?;

    let mut text_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str())
                    && !t.trim().is_empty()
                {
                    text_chunks.push(t.to_string());
                }
            }
            Some("tool_use") => tool_calls.push(ToolCall {
                // The transcript records no tool-call id — the writer drops it
                // when it flattens the blob graph — so there is none to report.
                id: None,
                name: block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            }),
            _ => {}
        }
    }

    if text_chunks.is_empty() && tool_calls.is_empty() {
        return None;
    }

    Some(CanonicalMessage {
        idx: 0,
        role: normalize_role(role_str),
        content: text_chunks.join("\n\n"),
        // Not recorded anywhere in the file.
        timestamp: None,
        author: None,
        tool_calls,
        tool_results: Vec::new(),
        extra: record.clone(),
    })
}

/// Extract text content from a bubble, trying multiple fields.
///
/// Priority: `text` > `rawText` > `content` > `message`
fn extract_bubble_content(bubble: &serde_json::Value) -> Option<String> {
    for field in ["text", "rawText", "richText", "content", "message"] {
        if let Some(val) = bubble.get(field) {
            let text = flatten_content(val);
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Parse a single bubble into a CanonicalMessage.
fn parse_bubble(
    bubble: &serde_json::Value,
    model_counts: &mut std::collections::HashMap<String, usize>,
    started_at: &mut Option<i64>,
    ended_at: &mut Option<i64>,
) -> Option<CanonicalMessage> {
    let content = extract_bubble_content(bubble)?;

    // Determine role.
    let role = determine_bubble_role(bubble);

    // Extract author (model name).
    let author = bubble
        .get("modelType")
        .and_then(|v| v.as_str())
        .or_else(|| bubble.get("model").and_then(|v| v.as_str()))
        .or_else(|| {
            bubble
                .pointer("/modelInfo/modelName")
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .map(String::from);

    if let Some(ref m) = author {
        *model_counts.entry(m.clone()).or_insert(0) += 1;
    }

    // Extract timestamp.
    let timestamp = bubble
        .get("timestamp")
        .or_else(|| bubble.get("createdAt"))
        .and_then(parse_timestamp);

    if let Some(ts) = timestamp {
        *started_at = Some(started_at.map_or(ts, |s: i64| s.min(ts)));
        *ended_at = Some(ended_at.map_or(ts, |e: i64| e.max(ts)));
    }

    Some(CanonicalMessage {
        idx: 0, // Re-indexed by caller.
        role,
        content,
        timestamp,
        author,
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        extra: bubble.clone(),
    })
}

/// Determine message role from bubble data.
///
/// Checks numeric `type` field first (v0.40+), then string `type`/`role` fields.
fn determine_bubble_role(bubble: &serde_json::Value) -> MessageRole {
    // Modern numeric type.
    if let Some(num_type) = bubble.get("type").and_then(|v| v.as_i64()) {
        return match num_type {
            BUBBLE_TYPE_USER => MessageRole::User,
            BUBBLE_TYPE_ASSISTANT => MessageRole::Assistant,
            _ => MessageRole::Assistant, // Unknown types default to assistant.
        };
    }

    // String type field.
    if let Some(type_str) = bubble.get("type").and_then(|v| v.as_str()) {
        return normalize_cursor_role(type_str);
    }

    // Fallback: role field.
    if let Some(role_str) = bubble.get("role").and_then(|v| v.as_str()) {
        return normalize_cursor_role(role_str);
    }

    // Default to assistant for unknown content.
    MessageRole::Assistant
}

/// Normalize Cursor-specific role strings.
///
/// Cursor uses some role names that differ from the standard normalize_role:
/// - `"human"` → User (Cursor-specific)
/// - `"ai"` / `"bot"` → Assistant (Cursor-specific)
fn normalize_cursor_role(role_str: &str) -> MessageRole {
    match role_str.to_ascii_lowercase().as_str() {
        "user" | "human" => MessageRole::User,
        "assistant" | "ai" | "bot" | "model" | "agent" => MessageRole::Assistant,
        other => normalize_role(other),
    }
}

// ---------------------------------------------------------------------------
// Workspace extraction
// ---------------------------------------------------------------------------

/// Extract workspace path from bubble data.
///
/// Searches all bubbles for `workspaceProjectDir` or `workspaceUris`.
fn extract_workspace_from_bubbles(
    bubbles: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<PathBuf> {
    for bubble in bubbles.values() {
        // Direct workspace path.
        if let Some(dir) = bubble.get("workspaceProjectDir").and_then(|v| v.as_str())
            && !dir.is_empty()
        {
            return Some(PathBuf::from(dir));
        }

        // Workspace URIs array.
        if let Some(uris) = bubble.get("workspaceUris").and_then(|v| v.as_array()) {
            for uri in uris {
                if let Some(uri_str) = uri.as_str()
                    && let Some(path) = parse_workspace_uri(uri_str)
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Extract workspace from composerData itself (fallback).
fn extract_workspace_from_composer(composer: &serde_json::Value) -> Option<PathBuf> {
    workspace_from_identifier(composer.get("workspaceIdentifier")).or_else(|| {
        composer
            .get("workspacePath")
            .or_else(|| composer.get("projectPath"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    })
}

/// Read a workspace path out of `composerData.workspaceIdentifier`.
///
/// Cursor stamps every composer it creates with VS Code's `IWorkspaceIdentifier`
/// — `{id, uri}` for a single folder, `{id, configPath}` for a multi-root
/// `.code-workspace`, and a bare `{id}` for an empty window. The `id` is the
/// `workspaceStorage` hash and names no path, so only the URI forms answer.
///
/// The URI is a serialized VS Code `URI`: `{"$mid":1, "fsPath":…, "path":…,
/// "scheme":"file", "external":"file:///…"}`. `fsPath` is preferred because it
/// is already native and already decoded; `external` is the fallback for a
/// remote workspace, where `fsPath` is a path on the *remote*. An empty-window
/// composer returns `None` — there is no folder to name.
fn workspace_from_identifier(identifier: Option<&serde_json::Value>) -> Option<PathBuf> {
    let uri = identifier?
        .get("uri")
        .or_else(|| identifier?.get("configPath"))?;

    // A plain string URI, in case a future writer stops using `URI.toJSON`.
    if let Some(s) = uri.as_str() {
        return parse_workspace_uri(s).or_else(|| Some(PathBuf::from(s)));
    }

    if uri.get("scheme").and_then(|v| v.as_str()) == Some("file")
        && let Some(fs_path) = uri.get("fsPath").and_then(|v| v.as_str())
        && !fs_path.is_empty()
    {
        return Some(PathBuf::from(fs_path));
    }
    if let Some(external) = uri.get("external").and_then(|v| v.as_str())
        && let Some(path) = parse_workspace_uri(external)
    {
        return Some(path);
    }
    uri.get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Parse a workspace URI into a filesystem path.
///
/// Handles:
/// - `file:///path/to/project` → `/path/to/project`
/// - `vscode-remote://ssh-remote+{host}/path` → `/path`
fn parse_workspace_uri(uri: &str) -> Option<PathBuf> {
    if let Some(file_path) = uri.strip_prefix("file://") {
        let decoded = urlencoding::decode(file_path).ok()?;
        let path_str = decoded.as_ref();
        // On Unix, path is absolute. On Windows, strip leading / for drive letters.
        #[cfg(target_os = "windows")]
        {
            if path_str.len() > 2
                && path_str.as_bytes()[0] == b'/'
                && path_str.as_bytes()[2] == b':'
            {
                return Some(PathBuf::from(&path_str[1..]));
            }
        }
        return Some(PathBuf::from(path_str));
    }

    if let Some(rest) = uri.strip_prefix("vscode-remote://") {
        // Format: ssh-remote+{host_json}/actual/path
        // Find the first / after the host part.
        if let Some(slash_idx) = rest.find('/') {
            let path_part = &rest[slash_idx..];
            let decoded = urlencoding::decode(path_part).ok()?;
            return Some(PathBuf::from(decoded.as_ref()));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Legacy ItemTable support
// ---------------------------------------------------------------------------

/// Read a session from the legacy ItemTable format.
fn read_legacy_session(conn: &Connection, db_path: &Path) -> anyhow::Result<CanonicalSession> {
    if !Cursor::table_exists(conn, "ItemTable") {
        anyhow::bail!(
            "no cursorDiskKV or ItemTable found in {}",
            db_path.display()
        );
    }

    let mut stmt = conn.prepare(
        "SELECT key, value FROM ItemTable WHERE key LIKE '%aichat%chatdata%' OR key LIKE '%composer%' ORDER BY key LIMIT 1",
    )?;

    let result: Option<(String, String)> = stmt
        .query_row([], |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        })
        .ok();

    let (entry_key, entry_value) = result
        .ok_or_else(|| anyhow::anyhow!("no legacy chat data found in {}", db_path.display()))?;

    let data: serde_json::Value = serde_json::from_str(&entry_value)
        .with_context(|| format!("invalid JSON in legacy entry {entry_key}"))?;

    // Legacy format may have tabs/bubbles or direct messages.
    let empty_map = std::collections::HashMap::new();
    Cursor::parse_composer(&entry_key, &data, &empty_map, db_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Bubble content extraction
    // -----------------------------------------------------------------------

    #[test]
    fn extract_bubble_content_text_field() {
        let bubble = json!({"text": "Hello world", "type": 1});
        assert_eq!(extract_bubble_content(&bubble), Some("Hello world".into()));
    }

    #[test]
    fn extract_bubble_content_raw_text_field() {
        let bubble = json!({"rawText": "Raw content", "type": 1});
        assert_eq!(extract_bubble_content(&bubble), Some("Raw content".into()));
    }

    #[test]
    fn extract_bubble_content_rich_text_field() {
        let bubble = json!({"richText": "Rich content"});
        assert_eq!(extract_bubble_content(&bubble), Some("Rich content".into()));
    }

    #[test]
    fn extract_bubble_content_content_field() {
        let bubble = json!({"content": "Content field"});
        assert_eq!(
            extract_bubble_content(&bubble),
            Some("Content field".into())
        );
    }

    #[test]
    fn extract_bubble_content_message_field() {
        let bubble = json!({"message": "Message content"});
        assert_eq!(
            extract_bubble_content(&bubble),
            Some("Message content".into())
        );
    }

    #[test]
    fn extract_bubble_content_priority_text_over_raw() {
        let bubble = json!({"text": "Primary", "rawText": "Secondary"});
        assert_eq!(extract_bubble_content(&bubble), Some("Primary".into()));
    }

    #[test]
    fn extract_bubble_content_empty_text_falls_through() {
        let bubble = json!({"text": "", "rawText": "Fallback"});
        assert_eq!(extract_bubble_content(&bubble), Some("Fallback".into()));
    }

    #[test]
    fn extract_bubble_content_whitespace_only_falls_through() {
        let bubble = json!({"text": "   ", "content": "Real content"});
        assert_eq!(extract_bubble_content(&bubble), Some("Real content".into()));
    }

    #[test]
    fn extract_bubble_content_none_when_empty() {
        let bubble = json!({"type": 1});
        assert_eq!(extract_bubble_content(&bubble), None);
    }

    // -----------------------------------------------------------------------
    // Role determination
    // -----------------------------------------------------------------------

    #[test]
    fn determine_role_numeric_user() {
        let bubble = json!({"type": 1, "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::User);
    }

    #[test]
    fn determine_role_numeric_assistant() {
        let bubble = json!({"type": 2, "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    #[test]
    fn determine_role_numeric_unknown_defaults_assistant() {
        let bubble = json!({"type": 0, "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    #[test]
    fn determine_role_string_user() {
        let bubble = json!({"type": "user", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::User);
    }

    #[test]
    fn determine_role_string_human() {
        let bubble = json!({"type": "human", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::User);
    }

    #[test]
    fn determine_role_string_assistant() {
        let bubble = json!({"type": "assistant", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    #[test]
    fn determine_role_string_ai() {
        let bubble = json!({"type": "ai", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    #[test]
    fn determine_role_string_bot() {
        let bubble = json!({"type": "bot", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    #[test]
    fn determine_role_fallback_to_role_field() {
        let bubble = json!({"role": "user", "text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::User);
    }

    #[test]
    fn determine_role_no_type_no_role_defaults_assistant() {
        let bubble = json!({"text": "hi"});
        assert_eq!(determine_bubble_role(&bubble), MessageRole::Assistant);
    }

    // -----------------------------------------------------------------------
    // Workspace URI parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_file_uri() {
        let path = parse_workspace_uri("file:///home/user/project");
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn parse_file_uri_with_encoded_spaces() {
        let path = parse_workspace_uri("file:///home/user/my%20project");
        assert_eq!(path, Some(PathBuf::from("/home/user/my project")));
    }

    #[test]
    fn parse_vscode_remote_uri() {
        let path = parse_workspace_uri("vscode-remote://ssh-remote+myhost/home/user/project");
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn parse_unknown_uri_returns_none() {
        assert_eq!(parse_workspace_uri("https://example.com"), None);
    }

    // -----------------------------------------------------------------------
    // Cursor role normalization
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_cursor_role_standard() {
        assert_eq!(normalize_cursor_role("user"), MessageRole::User);
        assert_eq!(normalize_cursor_role("assistant"), MessageRole::Assistant);
    }

    #[test]
    fn normalize_cursor_role_cursor_specific() {
        assert_eq!(normalize_cursor_role("human"), MessageRole::User);
        assert_eq!(normalize_cursor_role("ai"), MessageRole::Assistant);
        assert_eq!(normalize_cursor_role("bot"), MessageRole::Assistant);
    }

    #[test]
    fn normalize_cursor_role_case_insensitive() {
        assert_eq!(normalize_cursor_role("USER"), MessageRole::User);
        assert_eq!(normalize_cursor_role("Human"), MessageRole::User);
        assert_eq!(normalize_cursor_role("AI"), MessageRole::Assistant);
        assert_eq!(normalize_cursor_role("Bot"), MessageRole::Assistant);
    }

    // -----------------------------------------------------------------------
    // parse_bubble
    // -----------------------------------------------------------------------

    #[test]
    fn parse_bubble_user_message() {
        let bubble = json!({
            "text": "Hello assistant",
            "type": 1,
            "timestamp": 1700000000000_i64,
        });
        let mut model_counts = std::collections::HashMap::new();
        let mut started = None;
        let mut ended = None;

        let msg = parse_bubble(&bubble, &mut model_counts, &mut started, &mut ended)
            .expect("should parse");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "Hello assistant");
        assert_eq!(msg.timestamp, Some(1_700_000_000_000));
    }

    #[test]
    fn parse_bubble_assistant_with_model() {
        let bubble = json!({
            "text": "Here's the answer.",
            "type": 2,
            "modelType": "gpt-4",
            "timestamp": 1700000001000_i64,
        });
        let mut model_counts = std::collections::HashMap::new();
        let mut started = None;
        let mut ended = None;

        let msg = parse_bubble(&bubble, &mut model_counts, &mut started, &mut ended)
            .expect("should parse");
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.author.as_deref(), Some("gpt-4"));
        assert_eq!(*model_counts.get("gpt-4").unwrap(), 1);
    }

    #[test]
    fn parse_bubble_empty_content_returns_none() {
        let bubble = json!({"type": 1});
        let mut model_counts = std::collections::HashMap::new();
        let mut started = None;
        let mut ended = None;

        assert!(parse_bubble(&bubble, &mut model_counts, &mut started, &mut ended).is_none());
    }

    #[test]
    fn parse_bubble_tracks_timestamps() {
        let b1 = json!({"text": "first", "type": 1, "timestamp": 1700000010000_i64});
        let b2 = json!({"text": "second", "type": 2, "timestamp": 1700000005000_i64});
        let mut model_counts = std::collections::HashMap::new();
        let mut started = None;
        let mut ended = None;

        parse_bubble(&b1, &mut model_counts, &mut started, &mut ended);
        parse_bubble(&b2, &mut model_counts, &mut started, &mut ended);

        assert_eq!(started, Some(1_700_000_005_000));
        assert_eq!(ended, Some(1_700_000_010_000));
    }

    // -----------------------------------------------------------------------
    // SQLite integration tests
    // -----------------------------------------------------------------------

    fn create_test_db(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("create test DB");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);",
        )
        .expect("create table");
        conn
    }

    fn insert_kv(conn: &Connection, key: &str, value: &serde_json::Value) {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, serde_json::to_string(value).unwrap()],
        )
        .unwrap();
    }

    #[test]
    fn read_modern_session_from_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = create_test_db(&db_path);

        let composer_id = "test-composer-123";

        // Insert bubbles.
        insert_kv(
            &conn,
            &format!("bubbleId:{composer_id}:bubble-1"),
            &json!({
                "text": "What is Rust?",
                "type": 1,
                "timestamp": 1700000000000_i64,
            }),
        );
        insert_kv(
            &conn,
            &format!("bubbleId:{composer_id}:bubble-2"),
            &json!({
                "text": "Rust is a systems programming language.",
                "type": 2,
                "modelType": "gpt-4",
                "timestamp": 1700000001000_i64,
            }),
        );

        // Insert composerData.
        insert_kv(
            &conn,
            &format!("composerData:{composer_id}"),
            &json!({
                "fullConversationHeadersOnly": [
                    {"bubbleId": "bubble-1"},
                    {"bubbleId": "bubble-2"},
                ],
                "createdAt": 1700000000000_i64,
                "lastUpdatedAt": 1700000001000_i64,
                "name": "Rust question",
            }),
        );

        drop(conn);

        let session = Cursor.read_session(&db_path).expect("should read session");

        assert_eq!(session.session_id, composer_id);
        assert_eq!(session.provider_slug, "cursor");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "What is Rust?");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(
            session.messages[1].content,
            "Rust is a systems programming language."
        );
        assert_eq!(session.messages[1].author.as_deref(), Some("gpt-4"));
        assert_eq!(session.title.as_deref(), Some("Rust question"));
        assert_eq!(session.model_name.as_deref(), Some("gpt-4"));
        assert_eq!(session.started_at, Some(1_700_000_000_000));
        assert_eq!(session.ended_at, Some(1_700_000_001_000));
    }

    #[test]
    fn read_tabs_format_from_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = create_test_db(&db_path);

        let composer_id = "tabs-composer";

        // No separate bubbles — inline in tabs.
        insert_kv(
            &conn,
            &format!("composerData:{composer_id}"),
            &json!({
                "tabs": [
                    {
                        "bubbles": [
                            {"text": "Tab question", "type": "user", "timestamp": 1700000000000_i64},
                            {"text": "Tab answer", "type": "assistant", "model": "claude-3", "timestamp": 1700000001000_i64},
                        ]
                    }
                ]
            }),
        );

        drop(conn);

        let session = Cursor
            .read_session(&db_path)
            .expect("should read tabs session");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Tab question");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Tab answer");
        assert_eq!(session.messages[1].author.as_deref(), Some("claude-3"));
    }

    #[test]
    fn read_conversation_map_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = create_test_db(&db_path);

        let composer_id = "convmap-composer";

        insert_kv(
            &conn,
            &format!("composerData:{composer_id}"),
            &json!({
                "conversationMap": {
                    "conv1": {
                        "bubbles": [
                            {"text": "Old format question", "role": "human"},
                            {"text": "Old format answer", "role": "ai"},
                        ]
                    }
                }
            }),
        );

        drop(conn);

        let session = Cursor
            .read_session(&db_path)
            .expect("should read conversationMap session");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn list_composer_ids_returns_all_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = create_test_db(&db_path);

        insert_kv(
            &conn,
            "composerData:session-a",
            &json!({"fullConversationHeadersOnly": []}),
        );
        insert_kv(
            &conn,
            "composerData:session-b",
            &json!({"fullConversationHeadersOnly": []}),
        );

        let ids = Cursor::list_composer_ids(&conn);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"session-a".to_string()));
        assert!(ids.contains(&"session-b".to_string()));
    }

    #[test]
    fn write_and_read_back_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Point CURSOR_HOME to temp dir.
        let cursor_home = tmp.path().join("cursor_config");
        std::fs::create_dir_all(cursor_home.join("User/globalStorage")).unwrap();

        // We can't set env vars safely in parallel tests, so test the write
        // path directly using the internal methods.
        let db_path = cursor_home.join("User/globalStorage/state.vscdb");
        let conn = Cursor::open_db_rw(&db_path).expect("create DB");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();

        // Create a sample session.
        let session = CanonicalSession {
            session_id: "original-123".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(PathBuf::from("/tmp/project")),
            title: Some("Test session".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Hello".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: Vec::new(),
                    tool_results: Vec::new(),
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "Hi there!".to_string(),
                    timestamp: Some(1_700_000_005_000),
                    author: Some("gpt-4".to_string()),
                    tool_calls: Vec::new(),
                    tool_results: Vec::new(),
                    extra: json!({}),
                },
            ],
            metadata: json!({}),
            source_path: PathBuf::from("/tmp/original.jsonl"),
            model_name: Some("gpt-4".to_string()),
        };

        // Write using internal method.
        let composer_id = "roundtrip-test-id";
        let now_millis = chrono::Utc::now().timestamp_millis();

        let mut headers: Vec<serde_json::Value> = Vec::new();
        for msg in &session.messages {
            let bubble_id = uuid::Uuid::new_v4().to_string();
            // Kept identical to `write_session`'s mapping above: this test
            // re-implements the write because it cannot set `CURSOR_HOME`, and a
            // second copy that drifts is a second copy that can be wrong.
            let bubble_type = match msg.role {
                MessageRole::User | MessageRole::System | MessageRole::Other(_) => BUBBLE_TYPE_USER,
                MessageRole::Assistant | MessageRole::Tool => BUBBLE_TYPE_ASSISTANT,
            };
            let bubble = json!({
                "text": msg.content,
                "type": bubble_type,
                "timestamp": msg.timestamp.unwrap_or(now_millis),
                "modelType": msg.author.as_deref(),
            });
            insert_kv(
                &conn,
                &format!("bubbleId:{composer_id}:{bubble_id}"),
                &bubble,
            );
            headers.push(json!({"bubbleId": bubble_id}));
        }

        let composer_data = json!({
            "fullConversationHeadersOnly": headers,
            "createdAt": session.started_at,
            "lastUpdatedAt": session.ended_at,
            "name": session.title,
        });
        insert_kv(
            &conn,
            &format!("composerData:{composer_id}"),
            &composer_data,
        );
        drop(conn);

        // Read back.
        let readback = {
            let conn = Cursor::open_db(&db_path).unwrap();
            Cursor::read_composer_session(&conn, composer_id, &db_path).expect("should read back")
        };

        assert_eq!(readback.session_id, composer_id);
        assert_eq!(readback.messages.len(), 2);
        assert_eq!(readback.messages[0].role, MessageRole::User);
        assert_eq!(readback.messages[0].content, "Hello");
        assert_eq!(readback.messages[1].role, MessageRole::Assistant);
        assert_eq!(readback.messages[1].content, "Hi there!");
        assert_eq!(readback.messages[1].author.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn workspace_extraction_from_bubbles() {
        let mut bubbles = std::collections::HashMap::new();
        bubbles.insert(
            "b1".to_string(),
            json!({"workspaceProjectDir": "/home/user/project"}),
        );
        bubbles.insert("b2".to_string(), json!({"text": "no workspace"}));

        let ws = extract_workspace_from_bubbles(&bubbles);
        assert_eq!(ws, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn workspace_extraction_from_uris() {
        let mut bubbles = std::collections::HashMap::new();
        bubbles.insert(
            "b1".to_string(),
            json!({"workspaceUris": ["file:///data/projects/test"]}),
        );

        let ws = extract_workspace_from_bubbles(&bubbles);
        assert_eq!(ws, Some(PathBuf::from("/data/projects/test")));
    }

    #[test]
    fn workspace_extraction_from_composer_fallback() {
        let composer = json!({"workspacePath": "/data/projects/test"});
        let ws = extract_workspace_from_composer(&composer);
        assert_eq!(ws, Some(PathBuf::from("/data/projects/test")));
    }

    #[test]
    fn simple_composer_fallback_does_not_copy_unknown_vendor_fields() {
        let composer = json!({
            "text": "legacy prompt",
            "type": 1,
            "timestamp": 1_700_000_000_000_i64,
            "blobEncryptionKey": "planted-not-a-real-key",
        });

        let session = Cursor::parse_composer(
            "legacy-composer",
            &composer,
            &std::collections::HashMap::new(),
            Path::new("/tmp/state.vscdb"),
        )
        .expect("legacy composer should parse");

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "legacy prompt");
        assert!(
            session.messages[0].extra.is_null(),
            "fallback copied the whole composerData vendor object"
        );
    }

    #[test]
    fn empty_db_returns_no_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let _conn = create_test_db(&db_path);

        let result = Cursor.read_session(&db_path);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // list_sessions
    // -----------------------------------------------------------------------

    #[test]
    fn list_sessions_enumerates_all_composers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = create_test_db(&db_path);

        // Insert two composer sessions
        let composer_a = json!({
            "composerId": "comp-aaa",
            "conversation": [
                {"bubbleId": "b1", "type": 1, "text": "User msg A"},
                {"bubbleId": "b2", "type": 2, "text": "Assist msg A"}
            ]
        });
        let composer_b = json!({
            "composerId": "comp-bbb",
            "conversation": [
                {"bubbleId": "b3", "type": 1, "text": "User msg B"},
                {"bubbleId": "b4", "type": 2, "text": "Assist msg B"}
            ]
        });
        insert_kv(&conn, "composerData:comp-aaa", &composer_a);
        insert_kv(&conn, "composerData:comp-bbb", &composer_b);
        drop(conn);

        // list_composer_ids should find both
        let conn = Cursor::open_db(&db_path).expect("open");
        let ids = Cursor::list_composer_ids(&conn);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"comp-aaa".to_string()));
        assert!(ids.contains(&"comp-bbb".to_string()));
    }
}
