//! Write-path tests for the OpenCode provider.
//!
//! These live here rather than in an in-crate `#[cfg(test)]` module because
//! every path that reaches `OpenCode::choose_target_db_path` reads the process
//! environment, and `src/lib.rs` declares `#![forbid(unsafe_code)]` while
//! `std::env::set_var` is `unsafe` in edition 2024. An in-crate test therefore
//! *cannot* redirect `XDG_DATA_HOME`, and since `choose_target_db_path` now
//! prefers an existing database over inventing one, an unguarded test would
//! write into the developer's real `~/.local/share/opencode/opencode.db`.
//!
//! Every test below holds the shared `EnvLock` (see `tests/test_env.rs`) for as
//! long as it mutates the environment *and* for as long as it calls provider
//! code that reads it.

mod test_env;

use std::path::{Path, PathBuf};

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult};
use casr::providers::opencode::OpenCode;
use casr::providers::{Provider, WriteOptions};

static OPENCODE_ENV: test_env::EnvLock = test_env::EnvLock;

const DATA_DIRNAME: &str = ".opencode";
const DB_FILENAME: &str = "opencode.db";

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

/// RAII guard that overrides one env var and restores the original on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let guard = Self::capture(key);
        // SAFETY: callers hold the `OPENCODE_ENV` lock for the whole test, so no
        // other thread reads or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn unset(key: &'static str) -> Self {
        let guard = Self::capture(key);
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        guard
    }

    fn capture(key: &'static str) -> Self {
        Self {
            key,
            original: std::env::var_os(key),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: the same lock covers the restore.
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn change_to(path: &Path) -> Self {
        let original = std::env::current_dir().expect("read current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Redirects every location `OpenCode::find_db_files` consults into a throwaway
/// directory, so a write that falls through to discovery cannot reach a real
/// install.
///
/// The redirect is not a formality — `redirecting_the_environment_is_what_
/// protects_a_real_database` fails without it. Discovery reads, in order: the
/// three `OPENCODE_*` overrides, the cwd's ancestors, `$XDG_DATA_HOME/opencode`,
/// `$HOME/.opencode`, and `$XDG_CONFIG_HOME`-resolved config files. All of them
/// are covered here.
///
/// Field order is load-bearing. Rust drops fields in declaration order, so the
/// `EnvLock` guard must come **last**: it has to outlive every `EnvGuard` and
/// the `CwdGuard`, or the lock is released while the environment and the
/// process-global working directory are still dirty and the next test observes
/// them. Getting this backwards makes the suite intermittently fail.
struct Sandbox {
    _db_path: EnvGuard,
    _oc_home: EnvGuard,
    _oc_db: EnvGuard,
    _xdg_data: EnvGuard,
    _xdg_config: EnvGuard,
    _home: EnvGuard,
    _cwd: CwdGuard,
    tmp: tempfile::TempDir,
    workspace: PathBuf,
    _lock: test_env::EnvLockGuard<'static>,
}

impl Sandbox {
    /// A sandbox whose cwd is an empty workspace with no database in it.
    fn new() -> Self {
        let lock = OPENCODE_ENV.lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tmpdir");
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&home).expect("home dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");

        let guard = Self {
            _db_path: EnvGuard::unset("OPENCODE_DB_PATH"),
            _oc_home: EnvGuard::unset("OPENCODE_HOME"),
            _oc_db: EnvGuard::unset("OPENCODE_DB"),
            _xdg_data: EnvGuard::set("XDG_DATA_HOME", &home.join("share")),
            _xdg_config: EnvGuard::set("XDG_CONFIG_HOME", &home.join("config")),
            _home: EnvGuard::set("HOME", &home),
            _cwd: CwdGuard::change_to(&workspace),
            tmp,
            workspace,
            _lock: lock,
        };
        assert!(
            OpenCode.session_roots().is_empty(),
            "a fresh sandbox must start with no discoverable database; found {:?}",
            OpenCode.session_roots()
        );
        guard
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The path `<workspace>/.opencode/opencode.db` — precedence branch 2 and 4.
    fn workspace_db(&self) -> PathBuf {
        self.workspace.join(DATA_DIRNAME).join(DB_FILENAME)
    }

    /// The path an OpenCode install would use — precedence branch 3.
    fn data_dir_db(&self) -> PathBuf {
        self.tmp
            .path()
            .join("home/share/opencode")
            .join(DB_FILENAME)
    }

    fn session(&self) -> CanonicalSession {
        sample_session(self.workspace())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Copied verbatim from the database the released `opencode-linux-x64` 1.18.6
/// binary creates, trimmed to the tables this provider touches.
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
    std::fs::create_dir_all(path.parent().expect("db parent")).expect("db dir");
    let conn = rusqlite::Connection::open(path).expect("create db");
    conn.execute_batch(CURRENT_SCHEMA_DDL).expect("ddl");
}

fn open_ro(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open db read-only")
}

fn count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("count rows")
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
        .and_then(|mut stmt| stmt.exists(rusqlite::params![table]))
        .unwrap_or(false)
}

fn sample_session(workspace: &Path) -> CanonicalSession {
    CanonicalSession {
        session_id: "source-session".to_string(),
        provider_slug: "claude-code".to_string(),
        workspace: Some(workspace.to_path_buf()),
        title: Some("Fix OpenCode adapter".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages: vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Please inspect src/main.rs".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::json!({}),
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Inspecting now.".to_string(),
                timestamp: Some(1_700_000_005_000),
                author: Some("gpt-5".to_string()),
                tool_calls: vec![ToolCall {
                    id: Some("call-1".to_string()),
                    name: "Read".to_string(),
                    arguments: serde_json::json!({"path":"src/main.rs"}),
                }],
                tool_results: vec![ToolResult {
                    call_id: Some("call-1".to_string()),
                    content: "Read complete".to_string(),
                    is_error: false,
                }],
                extra: serde_json::json!({}),
            },
        ],
        metadata: serde_json::json!({}),
        source_path: workspace.join("source.jsonl"),
        model_name: Some("gpt-5".to_string()),
    }
}

fn written_db(written: &casr::providers::WrittenSession) -> PathBuf {
    written.paths[0]
        .parent()
        .expect("virtual path parent")
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// choose_target_db_path — the four branches
// ---------------------------------------------------------------------------

/// Branch 1: an explicit override is the user naming the target, and it beats
/// both an existing workspace database and anything discovery found.
#[test]
fn an_env_override_wins_over_every_existing_database() {
    let sb = Sandbox::new();
    make_current_db(&sb.workspace_db());
    make_current_db(&sb.data_dir_db());
    let chosen = sb.tmp.path().join("explicit/opencode.db");
    make_current_db(&chosen);
    let _override = EnvGuard::set("OPENCODE_DB_PATH", &chosen);

    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    assert_eq!(written_db(&written), chosen);
}

/// Branch 2: a database the user already keeps beside the workspace wins over
/// discovery. Current OpenCode never creates one there, so its presence means
/// somebody deliberately did.
#[test]
fn a_workspace_that_already_has_a_database_wins_over_discovery() {
    let sb = Sandbox::new();
    make_current_db(&sb.workspace_db());
    make_current_db(&sb.data_dir_db());

    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    assert_eq!(
        written_db(&written),
        sb.workspace_db(),
        "an existing workspace database must not be bypassed"
    );
    assert_eq!(
        count(&open_ro(&sb.data_dir_db()), "session"),
        0,
        "the data-dir database must be left alone"
    );
}

/// Branch 3: the workspace has no database, so the one discovery found wins —
/// the whole point of the change. Previously this invented a fresh
/// workspace-local database that OpenCode could never open.
#[test]
fn a_discovered_database_wins_over_a_workspace_without_one() {
    let sb = Sandbox::new();
    make_current_db(&sb.data_dir_db());

    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    assert_eq!(
        written_db(&written),
        sb.data_dir_db(),
        "a session must land in the database OpenCode actually reads"
    );
    assert!(
        !sb.workspace_db().exists(),
        "no fresh workspace database should have been invented"
    );
    assert_eq!(count(&open_ro(&sb.data_dir_db()), "session"), 1);
}

/// Branch 4: nothing exists anywhere, so one is created beside the workspace —
/// and the writer says plainly that OpenCode will not read it.
#[test]
fn with_no_database_anywhere_one_is_created_beside_the_workspace() {
    let sb = Sandbox::new();

    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    assert_eq!(written_db(&written), sb.workspace_db());
    assert_eq!(written.warnings.len(), 1, "a fresh database is a caveat");
}

/// Branch 4, with no workspace at all: the cwd is the last resort.
#[test]
fn with_no_workspace_the_cwd_is_the_last_resort() {
    let sb = Sandbox::new();
    let mut session = sb.session();
    session.workspace = None;

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("write");
    assert_eq!(
        written_db(&written),
        sb.workspace().join(DATA_DIRNAME).join(DB_FILENAME),
        "the cwd is the workspace here, reached as a fallback rather than from the session"
    );
}

/// The redirect is load-bearing, not decoration.
///
/// This writes a session whose workspace holds no database, so resolution falls
/// through to discovery — which, without the `XDG_DATA_HOME` redirect the
/// sandbox installs, is the developer's real OpenCode data directory. Remove
/// the redirect and this fails, because the write lands somewhere other than
/// the sandbox database.
///
/// That is the guarantee the whole file rests on: the environment guard is what
/// stands between this suite and a real install.
#[test]
fn redirecting_the_environment_is_what_protects_a_real_database() {
    let sb = Sandbox::new();
    let sandbox_db = sb.data_dir_db();
    make_current_db(&sandbox_db);

    // Nothing beside the workspace, so this must fall through to discovery.
    assert!(!sb.workspace_db().exists());
    assert_eq!(
        OpenCode.session_roots(),
        vec![sandbox_db.clone()],
        "discovery must see only the sandbox database; if this fails the \
         redirect is not covering every location find_db_files consults"
    );

    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");

    assert_eq!(
        written_db(&written),
        sandbox_db,
        "the write escaped the sandbox — the environment redirect is what keeps \
         this suite out of a real ~/.local/share/opencode/opencode.db"
    );
}

// ---------------------------------------------------------------------------
// Writer / reader round trips (moved from the crate module)
// ---------------------------------------------------------------------------

#[test]
fn writer_reader_roundtrip_preserves_core_content() {
    let sb = Sandbox::new();
    let source = sb.session();
    let written = OpenCode
        .write_session(&source, &WriteOptions { force: false })
        .expect("write should succeed");

    assert_eq!(written.resume_command, "opencode");
    assert_eq!(written.paths.len(), 1);
    assert!(written_db(&written).is_file(), "db file should exist");

    let readback = OpenCode
        .read_session(&written.paths[0])
        .expect("readback should succeed");

    assert_eq!(readback.provider_slug, "opencode");
    assert_eq!(readback.messages.len(), source.messages.len());
    assert_eq!(readback.messages[0].role, MessageRole::User);
    assert_eq!(readback.messages[0].content, source.messages[0].content);
    assert_eq!(readback.messages[1].role, MessageRole::Assistant);
    assert_eq!(readback.messages[1].content, source.messages[1].content);
    assert_eq!(readback.workspace.as_deref(), Some(sb.workspace()));
    // The target id is derived stably from the source session id so that
    // re-conversion is idempotent and `--force` can overwrite in place.
    assert_eq!(readback.session_id, source.session_id);
}

/// Regression for #14: writing the same OpenCode session twice must fail
/// without `--force` (clean SessionConflict, not a raw SQLite duplicate-key
/// error) and succeed with `--force`, overwriting the existing row in place
/// rather than orphaning a duplicate.
#[test]
fn write_twice_with_force_overwrites_in_place() {
    let sb = Sandbox::new();
    let source = sb.session();

    let first = OpenCode
        .write_session(&source, &WriteOptions { force: false })
        .expect("first write should succeed");
    let db_path = written_db(&first);

    let conflict = OpenCode
        .write_session(&source, &WriteOptions { force: false })
        .expect_err("second write without --force should conflict");
    match conflict.downcast_ref::<casr::error::CasrError>() {
        Some(casr::error::CasrError::SessionConflict { session_id, .. }) => {
            assert_eq!(session_id, &source.session_id);
        }
        other => panic!("expected SessionConflict, got {other:?}"),
    }

    let second = OpenCode
        .write_session(&source, &WriteOptions { force: true })
        .expect("force write should succeed");

    assert_eq!(first.session_id, second.session_id);
    assert_eq!(second.session_id, source.session_id);

    let conn = open_ro(&db_path);
    assert_eq!(
        count(&conn, "sessions"),
        1,
        "force must overwrite, not duplicate"
    );
    assert_eq!(
        count(&conn, "messages"),
        source.messages.len() as i64,
        "messages from the prior write must be replaced, not accumulated"
    );

    let readback = OpenCode
        .read_session(&second.paths[0])
        .expect("readback after force overwrite");
    assert_eq!(readback.messages.len(), source.messages.len());
}

#[test]
fn owns_session_returns_virtual_path() {
    let sb = Sandbox::new();
    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write should succeed");
    let found = OpenCode.owns_session(&written.session_id);

    assert_eq!(found.as_deref(), Some(written.paths[0].as_path()));
}

#[test]
fn read_session_from_db_path_returns_latest_root_session() {
    let sb = Sandbox::new();

    // Distinct source ids so both land as separate root sessions in one DB
    // (target ids are derived stably from the source session id).
    let mut first = sb.session();
    first.session_id = "older-source".to_string();
    first.title = Some("Older Session".to_string());
    first.started_at = Some(1_700_000_000_000);
    OpenCode
        .write_session(&first, &WriteOptions { force: false })
        .expect("first write");

    let mut second = sb.session();
    second.session_id = "newer-source".to_string();
    second.title = Some("Newer Session".to_string());
    second.started_at = Some(1_800_000_000_000);
    let second_written = OpenCode
        .write_session(&second, &WriteOptions { force: false })
        .expect("second write");

    let read_latest = OpenCode
        .read_session(&written_db(&second_written))
        .expect("read from db should pick latest");
    assert_eq!(read_latest.title.as_deref(), Some("Newer Session"));
}

#[test]
fn detect_reports_db_presence() {
    let sb = Sandbox::new();
    OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write should succeed");

    let detection = OpenCode.detect();
    assert!(
        detection.installed,
        "db presence should mark provider installed"
    );
    assert!(
        detection
            .evidence
            .iter()
            .any(|ev| ev.contains("opencode.db")),
        "evidence should include db detection"
    );
}

#[test]
fn writer_no_title_generates_from_first_user_message() {
    let sb = Sandbox::new();
    let mut session = sb.session();
    session.title = None;

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("write");
    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

    let title = readback.title.expect("title should be derived");
    assert!(title.contains("inspect"));
}

#[test]
fn writer_no_timestamps_uses_current_time() {
    let sb = Sandbox::new();
    let mut session = sb.session();
    session.started_at = None;
    session.ended_at = None;
    for msg in &mut session.messages {
        msg.timestamp = None;
    }

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("write");
    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

    assert!(readback.started_at.is_some());
    assert!(readback.ended_at.is_some());
}

#[test]
fn writer_model_name_propagated_to_messages() {
    let sb = Sandbox::new();
    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

    assert!(readback.model_name.is_some());
}

#[test]
fn reader_metadata_includes_token_counts() {
    let sb = Sandbox::new();
    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

    assert!(readback.metadata.get("opencode_db").is_some());
    assert!(readback.metadata.get("prompt_tokens").is_some());
    assert!(readback.metadata.get("completion_tokens").is_some());
    assert!(readback.metadata.get("cost").is_some());
}

#[test]
fn reader_message_extra_has_opencode_fields() {
    let sb = Sandbox::new();
    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write");
    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

    for msg in &readback.messages {
        assert!(
            msg.extra.get("opencode_message_id").is_some(),
            "each message should have opencode_message_id in extra"
        );
        assert!(
            msg.extra.get("opencode_parts").is_some(),
            "each message should have opencode_parts in extra"
        );
    }
}

#[test]
fn list_sessions_returns_all_sessions_from_db() {
    let sb = Sandbox::new();

    let mut first = sb.session();
    first.session_id = "first-source".to_string();
    first.title = Some("First Session".to_string());
    first.started_at = Some(1_700_000_000_000);
    let first_written = OpenCode
        .write_session(&first, &WriteOptions { force: false })
        .expect("first write");

    let mut second = sb.session();
    second.session_id = "second-source".to_string();
    second.title = Some("Second Session".to_string());
    second.started_at = Some(1_800_000_000_000);
    let second_written = OpenCode
        .write_session(&second, &WriteOptions { force: false })
        .expect("second write");

    let listed = OpenCode.list_sessions().expect("should return Some");
    let ids: Vec<&str> = listed.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&first_written.session_id.as_str()),
        "first session should be listed"
    );
    assert!(
        ids.contains(&second_written.session_id.as_str()),
        "second session should be listed"
    );
}

#[test]
fn list_sessions_empty_db_returns_empty_vec() {
    let sb = Sandbox::new();
    make_current_db(&sb.data_dir_db());

    let listed = OpenCode.list_sessions().expect("should return Some");
    assert!(listed.is_empty(), "empty DB should have no sessions");
}

// ---------------------------------------------------------------------------
// Schema matching
// ---------------------------------------------------------------------------

/// Writing into a live OpenCode database must produce rows that OpenCode reads
/// — which means its tables, not casr's.
#[test]
fn writer_matches_a_current_schema_target() {
    let sb = Sandbox::new();
    let db_path = sb.data_dir_db();
    make_current_db(&db_path);

    let source = sb.session();
    let written = OpenCode
        .write_session(&source, &WriteOptions { force: false })
        .expect("write should succeed");
    assert_eq!(written_db(&written), db_path);
    assert!(
        written.warnings.is_empty(),
        "writing into a real OpenCode database is not a degraded write"
    );

    let conn = open_ro(&db_path);
    assert_eq!(count(&conn, "session"), 1);
    assert_eq!(count(&conn, "message"), source.messages.len() as i64);
    assert!(
        count(&conn, "part") >= count(&conn, "message"),
        "every message contributes at least a part"
    );
    assert!(
        !table_exists(&conn, "sessions"),
        "casr must not graft its legacy tables onto a live OpenCode database"
    );

    // OpenCode brands session ids; an unbranded one is listed but cannot be
    // opened by `opencode export` and friends.
    assert!(
        written.session_id.starts_with("ses_"),
        "written id {} is not one OpenCode accepts",
        written.session_id
    );

    let readback = OpenCode
        .read_session(&written.paths[0])
        .expect("readback should succeed");
    assert_eq!(readback.metadata["opencode_schema"], "session/message/part");
    assert_eq!(readback.messages.len(), source.messages.len());
    assert_eq!(readback.messages[0].content, source.messages[0].content);
    assert_eq!(readback.messages[1].content, source.messages[1].content);
    assert_eq!(readback.messages[1].tool_calls.len(), 1);
    assert_eq!(readback.messages[1].tool_calls[0].name, "Read");
    assert_eq!(readback.messages[1].tool_results.len(), 1);
    assert_eq!(
        readback.messages[1].tool_results[0].content,
        "Read complete"
    );
    assert_eq!(readback.workspace.as_deref(), Some(sb.workspace()));
}

/// An OpenCode old enough to own a legacy database still gets the legacy
/// layout. Trading one layout for the other would move the bug rather than fix
/// it.
///
/// The legacy database here is made by casr's own branch-4 write, which is the
/// only way one comes into existence now, rather than by restating its DDL.
#[test]
fn writer_matches_a_legacy_schema_target() {
    let sb = Sandbox::new();

    let mut seed = sb.session();
    seed.session_id = "seed-source".to_string();
    let seeded = OpenCode
        .write_session(&seed, &WriteOptions { force: false })
        .expect("seed write creates the legacy database");
    let db_path = written_db(&seeded);
    assert_eq!(db_path, sb.workspace_db());

    // Now the workspace has a legacy database, so branch 2 selects it.
    let source = sb.session();
    let written = OpenCode
        .write_session(&source, &WriteOptions { force: false })
        .expect("write should succeed");
    assert_eq!(written_db(&written), db_path);
    assert!(
        written.warnings.is_empty(),
        "an existing legacy database is its owner's real store, not a degraded target"
    );

    let conn = open_ro(&db_path);
    assert!(!table_exists(&conn, "session"));
    assert_eq!(count(&conn, "sessions"), 2);

    let readback = OpenCode.read_session(&written.paths[0]).expect("readback");
    assert_eq!(
        readback.metadata["opencode_schema"],
        "sessions/messages/files"
    );
    assert_eq!(readback.messages.len(), source.messages.len());
}

/// casr cannot bootstrap a current-schema database from nothing — OpenCode's
/// migrator runs on open and aborts with "table `project` already exists". So
/// it writes the legacy layout and says so, instead of reporting a success that
/// no OpenCode will ever show.
#[test]
fn creating_a_database_from_nothing_warns_that_opencode_will_not_read_it() {
    let sb = Sandbox::new();
    let written = OpenCode
        .write_session(&sb.session(), &WriteOptions { force: false })
        .expect("write should succeed");

    assert_eq!(written.warnings.len(), 1, "a fresh database is a caveat");
    let warning = &written.warnings[0];
    assert!(
        warning.contains("will not show this session"),
        "warning must say the session is invisible to OpenCode: {warning}"
    );
    assert!(
        warning.contains("OPENCODE_DB_PATH"),
        "warning must say how to fix it: {warning}"
    );
}

/// Regression: a transcript whose message order disagrees with its own
/// timestamps used to read back reordered, because both layouts are read with
/// `ORDER BY <created>, id`. Measured on a real 478-message Claude Code
/// session, where a tool result is stamped a millisecond before the message it
/// follows.
#[test]
fn out_of_order_timestamps_still_round_trip_in_order() {
    for current in [true, false] {
        let sb = Sandbox::new();
        if current {
            make_current_db(&sb.data_dir_db());
        }

        let mut source = sb.session();
        source.messages = (0..6)
            .map(|i| CanonicalMessage {
                idx: i,
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("message number {i}"),
                // Deliberately non-monotonic: 3 lands before 2.
                timestamp: Some(1_700_000_000_000 + if i == 2 { 5 } else { i as i64 }),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::json!({}),
            })
            .collect();

        let written = OpenCode
            .write_session(&source, &WriteOptions { force: false })
            .expect("write");
        let readback = OpenCode.read_session(&written.paths[0]).expect("readback");

        let label = if current { "current" } else { "legacy" };
        assert_eq!(
            readback.metadata["opencode_schema"],
            if current {
                "session/message/part"
            } else {
                "sessions/messages/files"
            },
            "[{label}] the intended layout must be the one under test"
        );
        let got: Vec<&str> = readback
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        let want: Vec<&str> = source.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            got, want,
            "[{label}] message order must survive the round trip"
        );
    }
}
