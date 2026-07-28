//! Writer integration tests for multiple providers.
//!
//! Tests `write_session()` → `read_session()` round-trip compatibility and
//! provider-specific output shape conformance.
//!
//! These tests serialize process environment access because `write_session()`
//! reads provider home environment variables (`CLAUDE_HOME`, `CODEX_HOME`,
//! `GEMINI_HOME`, `CLINE_HOME`, `XDG_DATA_HOME` for Amp, etc.) to determine the target
//! directory and Rust 2024 makes env mutation `unsafe` under concurrency.

mod test_env;

use std::path::PathBuf;

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolCall};
use casr::providers::amp::Amp;
use casr::providers::chatgpt::ChatGpt;
use casr::providers::claude_code::ClaudeCode;
use casr::providers::clawdbot::ClawdBot;
use casr::providers::cline::Cline;
use casr::providers::codex::Codex;
use casr::providers::factory::Factory;
use casr::providers::gemini::Gemini;
use casr::providers::openclaw::OpenClaw;
use casr::providers::pi_agent::PiAgent;
use casr::providers::vibe::Vibe;
use casr::providers::{Provider, WriteOptions};

// ---------------------------------------------------------------------------
// Env var isolation
//
// Rust 2024 makes `std::env::set_var`/`remove_var` `unsafe` due to unsoundness
// when the process environment is accessed concurrently. The test harness runs
// tests in parallel, so all provider env mutations (and code that reads env)
// must be serialized within this test binary.
//
// Provider-named statics are kept for readability; they all share the same
// global re-entrant lock via `test_env`.
// ---------------------------------------------------------------------------

static CC_ENV: test_env::EnvLock = test_env::EnvLock;
static CODEX_ENV: test_env::EnvLock = test_env::EnvLock;
static GEMINI_ENV: test_env::EnvLock = test_env::EnvLock;
static CLINE_ENV: test_env::EnvLock = test_env::EnvLock;
static AMP_ENV: test_env::EnvLock = test_env::EnvLock;
static CHATGPT_ENV: test_env::EnvLock = test_env::EnvLock;
static CLAWDBOT_ENV: test_env::EnvLock = test_env::EnvLock;
static VIBE_ENV: test_env::EnvLock = test_env::EnvLock;
static FACTORY_ENV: test_env::EnvLock = test_env::EnvLock;
static OPENCLAW_ENV: test_env::EnvLock = test_env::EnvLock;
static PI_AGENT_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that sets an env var and restores the original value on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: Tests must hold an `_ENV` lock (see `test_env`) while mutating
        // the process environment and while invoking code that reads it.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: Same Mutex protects the restore.
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// Test session builders
// ---------------------------------------------------------------------------

fn simple_msg(idx: usize, role: MessageRole, content: &str, ts: i64) -> CanonicalMessage {
    CanonicalMessage {
        idx,
        role,
        content: content.to_string(),
        timestamp: Some(ts),
        author: None,
        tool_calls: vec![],
        tool_results: vec![],
        extra: serde_json::Value::Null,
    }
}

fn assistant_msg(idx: usize, content: &str, ts: i64, model: &str) -> CanonicalMessage {
    let mut m = simple_msg(idx, MessageRole::Assistant, content, ts);
    m.author = Some(model.to_string());
    m
}

/// Session with 4 text-only messages (clean roundtrip expected for all providers).
fn simple_session() -> CanonicalSession {
    CanonicalSession {
        session_id: "src-simple".to_string(),
        provider_slug: "test-source".to_string(),
        workspace: Some(PathBuf::from("/data/projects/myapp")),
        title: Some("Fix the login bug".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages: vec![
            simple_msg(0, MessageRole::User, "Fix the login bug", 1_700_000_000_000),
            assistant_msg(1, "I'll fix that now.", 1_700_000_005_000, "claude-3-opus"),
            simple_msg(
                2,
                MessageRole::User,
                "Also check the tests",
                1_700_000_007_000,
            ),
            assistant_msg(3, "Tests are passing.", 1_700_000_010_000, "claude-3-opus"),
        ],
        metadata: serde_json::json!({"source": "test"}),
        source_path: PathBuf::from("/tmp/source.jsonl"),
        model_name: Some("claude-3-opus".to_string()),
    }
}

/// Session with a tool call in the assistant message.
fn tool_call_session() -> CanonicalSession {
    let mut session = simple_session();
    session.messages[1].tool_calls = vec![ToolCall {
        id: Some("tc-1".to_string()),
        name: "Read".to_string(),
        arguments: serde_json::json!({"file_path": "src/auth.rs"}),
    }];
    session
}

// ===========================================================================
// Claude Code writer tests
// ===========================================================================

#[test]
fn writer_cc_roundtrip() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let session = simple_session();
    let written = ClaudeCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("CC write_session should succeed");

    assert_eq!(written.paths.len(), 1, "CC should produce exactly one file");
    assert!(written.paths[0].exists(), "CC output file should exist");
    assert!(
        written.resume_command.starts_with("claude --resume"),
        "CC resume command format"
    );

    let readback = ClaudeCode
        .read_session(&written.paths[0])
        .expect("CC read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "CC roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(orig.role, rb.role, "CC roundtrip msg {i}: role mismatch");
        assert_eq!(
            orig.content, rb.content,
            "CC roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "CC roundtrip: workspace"
    );
    assert!(
        readback.model_name.is_some(),
        "CC roundtrip: model_name should survive"
    );
}

#[test]
fn writer_cc_output_valid_jsonl() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 4, "CC should write one line per message");
    for (i, line) in lines.iter().enumerate() {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!("CC line {i} not valid JSON: {e}\nContent: {line}");
        }
    }
}

#[test]
fn writer_cc_entries_have_required_fields() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    for (i, line) in content.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        for field in [
            "sessionId",
            "type",
            "message",
            "uuid",
            "timestamp",
            "parentUuid",
            "cwd",
        ] {
            assert!(
                entry.get(field).is_some(),
                "CC line {i}: missing required field '{field}'"
            );
        }
        let entry_type = entry["type"].as_str().unwrap();
        assert!(
            entry_type == "user" || entry_type == "assistant",
            "CC line {i}: unexpected type '{entry_type}'"
        );
    }
}

#[test]
fn writer_cc_parent_uuid_chain() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let entries: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First entry: parentUuid is null.
    assert!(
        entries[0]["parentUuid"].is_null(),
        "CC first entry parentUuid should be null"
    );

    // Subsequent entries: parentUuid == previous entry's uuid.
    for i in 1..entries.len() {
        let expected = entries[i - 1]["uuid"].as_str().unwrap();
        let actual = entries[i]["parentUuid"].as_str().unwrap();
        assert_eq!(
            actual, expected,
            "CC entry {i}: parentUuid should chain to previous uuid"
        );
    }
}

#[test]
fn writer_cc_workspace_directory_placement() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let session = simple_session(); // workspace: /data/projects/myapp
    let written = ClaudeCode
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let path = &written.paths[0];
    // File should be under <CLAUDE_HOME>/projects/-data-projects-myapp/<uuid>.jsonl
    let expected_dir_key = "-data-projects-myapp";
    let parent = path.parent().unwrap();
    assert!(
        parent.ends_with(expected_dir_key),
        "CC file should be under project dir key '{expected_dir_key}', got: {}",
        parent.display()
    );
    assert!(
        path.extension().is_some_and(|e| e == "jsonl"),
        "CC file should have .jsonl extension"
    );
}

#[test]
fn writer_cc_timestamps_are_rfc3339() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    for (i, line) in content.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts_str = match entry["timestamp"].as_str() {
            Some(ts_str) => ts_str,
            None => {
                panic!("CC line {i}: timestamp should be a string");
            }
        };
        if let Err(e) = chrono::DateTime::parse_from_rfc3339(ts_str) {
            panic!("CC line {i}: timestamp '{ts_str}' not valid RFC3339: {e}");
        }
    }
}

#[test]
fn writer_cc_tool_calls_in_assistant_content() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&tool_call_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let entries: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Entry 1 is the assistant with a tool call.
    let assistant_entry = &entries[1];
    assert_eq!(assistant_entry["type"], "assistant");
    let msg_content = &assistant_entry["message"]["content"];
    let blocks = msg_content
        .as_array()
        .expect("CC assistant content should be array of blocks");

    let has_text = blocks.iter().any(|b| b["type"] == "text");
    let has_tool_use = blocks.iter().any(|b| b["type"] == "tool_use");
    assert!(has_text, "CC assistant content should contain text block");
    assert!(
        has_tool_use,
        "CC assistant content should contain tool_use block"
    );

    let tool_block = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
    assert_eq!(tool_block["name"], "Read");
    assert_eq!(tool_block["id"], "tc-1");
}

#[test]
fn writer_cc_model_name_on_assistant_entries() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let written = ClaudeCode
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    for (i, line) in content.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        if entry["type"] == "assistant" {
            assert!(
                entry["message"]["model"].is_string(),
                "CC assistant entry {i} should have message.model"
            );
        }
    }
}

// ===========================================================================
// Codex writer tests
// ===========================================================================

#[test]
fn writer_codex_roundtrip() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let session = simple_session();
    let written = Codex
        .write_session(&session, &WriteOptions { force: false })
        .expect("Codex write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "Codex should produce exactly one file"
    );
    assert!(written.paths[0].exists(), "Codex output file should exist");
    assert!(
        written.resume_command.starts_with("codex resume"),
        "Codex resume command format"
    );

    let readback = Codex
        .read_session(&written.paths[0])
        .expect("Codex read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Codex roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(orig.role, rb.role, "Codex roundtrip msg {i}: role mismatch");
        assert_eq!(
            orig.content, rb.content,
            "Codex roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "Codex roundtrip: workspace"
    );
}

#[test]
fn writer_codex_output_valid_jsonl() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // session_meta + 4 messages (2 user event_msg + 2 assistant response_item)
    assert_eq!(
        lines.len(),
        5,
        "Codex should write session_meta + 4 message lines"
    );
    for (i, line) in lines.iter().enumerate() {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!("Codex line {i} not valid JSON: {e}\nContent: {line}");
        }
    }
}

/// The real Codex 0.142.5 `threads` schema. Used as a fixture so the
/// registration test exercises the exact NOT NULL / default constraints and
/// column set casr must satisfy on a live database. Keep in sync with
/// `sqlite3 ~/.codex/state_5.sqlite '.schema threads'`.
const CODEX_THREADS_SCHEMA: &str = r#"
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    cwd TEXT NOT NULL,
    title TEXT NOT NULL,
    sandbox_policy TEXT NOT NULL,
    approval_mode TEXT NOT NULL,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    has_user_event INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    git_sha TEXT,
    git_branch TEXT,
    git_origin_url TEXT,
    cli_version TEXT NOT NULL DEFAULT '',
    first_user_message TEXT NOT NULL DEFAULT '',
    agent_nickname TEXT,
    agent_role TEXT,
    memory_mode TEXT NOT NULL DEFAULT 'enabled',
    model TEXT,
    reasoning_effort TEXT,
    agent_path TEXT,
    created_at_ms INTEGER,
    updated_at_ms INTEGER,
    thread_source TEXT,
    preview TEXT NOT NULL DEFAULT '',
    recency_at INTEGER NOT NULL DEFAULT 0,
    recency_at_ms INTEGER NOT NULL DEFAULT 0
);
"#;

/// Regression for issue #16: `codex resume <id>` looks the session up in
/// `~/.codex/state_*.sqlite` (`threads` table), not by scanning JSONL. After a
/// CC→Codex conversion, casr must register a `threads` row for the converted
/// session pointing at the rollout file.
#[test]
fn writer_codex_registers_thread_in_state_db() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    // Seed a state_5.sqlite with the real threads schema.
    let db_path = tmp.path().join("state_5.sqlite");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(CODEX_THREADS_SCHEMA).unwrap();
    }

    let session = simple_session();
    let written = Codex
        .write_session(&session, &WriteOptions { force: false })
        .expect("Codex write_session should succeed");

    assert!(
        written.warnings.is_empty(),
        "registration should succeed without warnings, got: {:?}",
        written.warnings
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE id = ?1",
            [&written.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "exactly one threads row for the converted session"
    );

    let (rollout_path, cwd, thread_source): (String, String, Option<String>) = conn
        .query_row(
            "SELECT rollout_path, cwd, thread_source FROM threads WHERE id = ?1",
            [&written.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    assert!(
        std::path::Path::new(&rollout_path).is_absolute(),
        "rollout_path must be absolute: {rollout_path}"
    );
    assert_eq!(
        rollout_path,
        written.paths[0].to_string_lossy(),
        "threads.rollout_path must point at the written rollout file"
    );
    assert_eq!(
        cwd, "/data/projects/myapp",
        "threads.cwd must be the workspace"
    );
    assert_eq!(
        thread_source.as_deref(),
        Some("user"),
        "threads.thread_source must be 'user'"
    );
}

/// A missing Codex state DB must not fail the write; the rollout is still
/// produced and a clear warning is surfaced.
#[test]
fn writer_codex_missing_state_db_warns_but_still_writes() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());
    // No state_*.sqlite present.

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .expect("write should still succeed without a state DB");

    assert!(written.paths[0].exists(), "rollout file should be written");
    assert!(
        !written.warnings.is_empty(),
        "a missing state DB should surface a warning"
    );
    assert!(
        written.warnings.iter().any(|w| w.contains("state_")),
        "warning should mention the missing state DB: {:?}",
        written.warnings
    );
}

/// The session_meta payload must carry both `id` and `session_id` (Codex reads
/// one or the other depending on version).
#[test]
fn writer_codex_session_meta_has_both_id_and_session_id() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();
    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let meta: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();

    let id = meta["payload"]["id"].as_str().expect("payload.id");
    let session_id = meta["payload"]["session_id"]
        .as_str()
        .expect("payload.session_id");
    assert_eq!(
        id, session_id,
        "payload.id and payload.session_id must match"
    );
    assert_eq!(id, written.session_id);
    assert_eq!(
        meta["payload"]["thread_source"].as_str(),
        Some("user"),
        "session_meta payload should mark thread_source=user"
    );
}

#[test]
fn writer_codex_session_meta_is_first_line() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(content.lines().next().unwrap()).unwrap();

    assert_eq!(
        first_line["type"], "session_meta",
        "Codex first line should be session_meta"
    );
    assert!(
        first_line["payload"]["id"].as_str().is_some(),
        "session_meta should have payload.id"
    );
    assert_eq!(
        first_line["payload"]["cwd"], "/data/projects/myapp",
        "session_meta should have correct cwd"
    );
}

#[test]
fn writer_codex_user_messages_are_event_msg() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let user_events: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["type"] == "event_msg" && l["payload"]["type"] == "user_message")
        .collect();
    assert_eq!(
        user_events.len(),
        2,
        "Codex should have 2 user event_msg lines"
    );
    assert_eq!(user_events[0]["payload"]["message"], "Fix the login bug");
    assert_eq!(user_events[1]["payload"]["message"], "Also check the tests");
}

#[test]
fn writer_codex_assistant_messages_are_response_item() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let response_items: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["type"] == "response_item")
        .collect();
    assert_eq!(
        response_items.len(),
        2,
        "Codex should have 2 response_item lines"
    );
    assert_eq!(response_items[0]["payload"]["role"], "assistant");
    assert_eq!(response_items[1]["payload"]["role"], "assistant");
}

#[test]
fn writer_codex_reasoning_messages() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let mut session = simple_session();
    // Replace second assistant message with a reasoning message.
    session.messages[3] = CanonicalMessage {
        idx: 3,
        role: MessageRole::Assistant,
        content: "Thinking about the tests...".to_string(),
        timestamp: Some(1_700_000_010_000),
        author: Some("reasoning".to_string()),
        tool_calls: vec![],
        tool_results: vec![],
        extra: serde_json::Value::Null,
    };

    let written = Codex
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let reasoning_events: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["type"] == "event_msg" && l["payload"]["type"] == "agent_reasoning")
        .collect();
    assert_eq!(
        reasoning_events.len(),
        1,
        "Codex should have 1 agent_reasoning event"
    );
    assert_eq!(
        reasoning_events[0]["payload"]["text"],
        "Thinking about the tests..."
    );
}

#[test]
fn writer_codex_top_level_timestamps_are_strings() {
    // Regression for issue #16: current Codex readers deserialize each rollout
    // line's top-level `timestamp` as an RFC3339 *string*. Emitting numeric
    // timestamps (the pre-#16 behavior) made the rollout unreadable by Codex
    // even after the session was discoverable.
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    for (i, line) in content.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = entry.get("timestamp");
        assert!(ts.is_some(), "Codex line {i}: missing timestamp");
        let ts = ts.unwrap();
        let s = ts
            .as_str()
            .unwrap_or_else(|| panic!("Codex line {i}: timestamp should be a string, got: {ts}"));
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("Codex line {i}: timestamp not RFC3339 ({e}): {s}"));
    }
}

#[test]
fn writer_codex_date_hierarchy_in_path() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let path_str = written.paths[0].to_string_lossy().to_string();
    let components: Vec<&str> = path_str.split('/').collect();

    // Should contain "sessions" directory.
    let sessions_idx = components
        .iter()
        .position(|c| *c == "sessions")
        .expect("Codex path should contain 'sessions'");

    // After "sessions": year/month/day/file.
    assert!(
        components.len() > sessions_idx + 4,
        "Codex path should have year/month/day/file after sessions/"
    );

    let year = components[sessions_idx + 1];
    assert!(
        year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()),
        "Codex path year should be 4 digits, got '{year}'"
    );
    let month = components[sessions_idx + 2];
    assert!(
        month.len() == 2 && month.chars().all(|c| c.is_ascii_digit()),
        "Codex path month should be 2 digits, got '{month}'"
    );
    let day = components[sessions_idx + 3];
    assert!(
        day.len() == 2 && day.chars().all(|c| c.is_ascii_digit()),
        "Codex path day should be 2 digits, got '{day}'"
    );

    let filename = components.last().unwrap();
    assert!(
        filename.starts_with("rollout-"),
        "Codex filename should start with 'rollout-'"
    );
    assert!(
        filename.ends_with(".jsonl"),
        "Codex filename should end with '.jsonl'"
    );
}

#[test]
fn writer_codex_tool_calls_in_response_content() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let written = Codex
        .write_session(&tool_call_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let response_items: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["type"] == "response_item")
        .collect();

    // First response_item should have tool_use in its content blocks.
    let first_content = response_items[0]["payload"]["content"]
        .as_array()
        .expect("Codex response_item content should be array");
    let has_tool_use = first_content.iter().any(|b| b["type"] == "tool_use");
    assert!(
        has_tool_use,
        "Codex response_item should contain tool_use block"
    );
}

// ===========================================================================
// Gemini writer tests
// ===========================================================================

#[test]
fn writer_gemini_roundtrip() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let session = simple_session();
    let written = Gemini
        .write_session(&session, &WriteOptions { force: false })
        .expect("Gemini write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "Gemini should produce exactly one file"
    );
    assert!(written.paths[0].exists(), "Gemini output file should exist");
    assert!(
        written.resume_command.starts_with("gemini --resume"),
        "Gemini resume command format"
    );

    let readback = Gemini
        .read_session(&written.paths[0])
        .expect("Gemini read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Gemini roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "Gemini roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "Gemini roundtrip msg {i}: content mismatch"
        );
    }
    // Gemini workspace is derived from message content heuristics,
    // not stored explicitly. With simple text messages, it won't survive.
}

#[test]
fn writer_gemini_output_valid_json() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let _: serde_json::Value =
        serde_json::from_str(&content).expect("Gemini output should be valid JSON");
}

#[test]
fn writer_gemini_top_level_fields() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert!(
        root["sessionId"].is_string(),
        "Gemini should have sessionId"
    );
    assert!(
        root["projectHash"].is_string(),
        "Gemini should have projectHash"
    );
    assert!(
        root["startTime"].is_string(),
        "Gemini should have startTime"
    );
    assert!(
        root["lastUpdated"].is_string(),
        "Gemini should have lastUpdated"
    );
    assert!(
        root["messages"].is_array(),
        "Gemini should have messages array"
    );
    assert_eq!(
        root["messages"].as_array().unwrap().len(),
        4,
        "Gemini should have 4 messages"
    );
}

#[test]
fn writer_gemini_message_types() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let messages = root["messages"].as_array().unwrap();

    assert_eq!(messages[0]["type"], "user", "Gemini msg 0 should be 'user'");
    assert_eq!(
        messages[1]["type"], "gemini",
        "Gemini msg 1 should be 'gemini'"
    );
    assert_eq!(messages[2]["type"], "user", "Gemini msg 2 should be 'user'");
    assert_eq!(
        messages[3]["type"], "gemini",
        "Gemini msg 3 should be 'gemini'"
    );
}

#[test]
fn writer_gemini_timestamps_are_rfc3339() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Top-level timestamps.
    for field in ["startTime", "lastUpdated"] {
        let ts = match root[field].as_str() {
            Some(ts) => ts,
            None => {
                panic!("Gemini: {field} should be string");
            }
        };
        if let Err(e) = chrono::DateTime::parse_from_rfc3339(ts) {
            panic!("Gemini: {field} '{ts}' not valid RFC3339: {e}");
        }
    }

    // Per-message timestamps.
    for (i, msg) in root["messages"].as_array().unwrap().iter().enumerate() {
        if let Some(ts) = msg["timestamp"].as_str()
            && let Err(e) = chrono::DateTime::parse_from_rfc3339(ts)
        {
            panic!("Gemini msg {i}: timestamp '{ts}' not valid RFC3339: {e}");
        }
    }
}

#[test]
fn writer_gemini_hash_directory_structure() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let path = &written.paths[0];
    // Should be under <GEMINI_HOME>/tmp/<hash>/chats/session-*.json
    let parent = path.parent().unwrap();
    assert_eq!(
        parent.file_name().unwrap().to_str().unwrap(),
        "chats",
        "Gemini file should be in a 'chats' directory"
    );

    let hash_dir = parent.parent().unwrap();
    let hash_name = hash_dir.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        hash_name.len(),
        64,
        "Gemini hash directory should be 64-char hex SHA256, got len={}",
        hash_name.len()
    );
    assert!(
        hash_name.chars().all(|c| c.is_ascii_hexdigit()),
        "Gemini hash dir should be hex chars, got '{hash_name}'"
    );

    assert!(
        path.extension().is_some_and(|e| e == "json"),
        "Gemini file should have .json extension"
    );
    let filename = path.file_name().unwrap().to_str().unwrap();
    assert!(
        filename.starts_with("session-"),
        "Gemini filename should start with 'session-'"
    );
}

#[test]
fn writer_gemini_extra_fields_preserved() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let mut session = simple_session();
    // Simulate grounding metadata on the assistant message.
    session.messages[1].extra = serde_json::json!({
        "type": "model",
        "content": "I'll fix that now.",
        "groundingMetadata": {"sourceCount": 2},
        "citations": [{"uri": "doc://ref1"}]
    });

    let written = Gemini
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let msg1 = &root["messages"].as_array().unwrap()[1];

    assert!(
        msg1["groundingMetadata"].is_object(),
        "Gemini should preserve groundingMetadata from extra"
    );
    assert!(
        msg1["citations"].is_array(),
        "Gemini should preserve citations from extra"
    );
}

#[test]
fn writer_gemini_project_hash_matches_workspace() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", tmp.path());

    let written = Gemini
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    let stored_hash = root["projectHash"].as_str().unwrap();
    let expected_hash =
        casr::providers::gemini::project_hash(std::path::Path::new("/data/projects/myapp"));
    assert_eq!(
        stored_hash, expected_hash,
        "Gemini projectHash should match SHA256 of workspace"
    );
}

// ===========================================================================
// Cross-provider: default workspace fallback
// ===========================================================================

#[test]
fn writer_cc_default_workspace_uses_tmp() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let mut session = simple_session();
    session.workspace = None;

    let written = ClaudeCode
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(
        first["cwd"], "/tmp",
        "CC should fall back to /tmp when workspace is None"
    );
}

#[test]
fn writer_codex_default_workspace_uses_tmp() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let mut session = simple_session();
    session.workspace = None;

    let written = Codex
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(
        first["payload"]["cwd"], "/tmp",
        "Codex should fall back to /tmp when workspace is None"
    );
}

// ===========================================================================
// Cline writer tests
// ===========================================================================

#[test]
fn writer_cline_roundtrip() {
    let _lock = CLINE_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLINE_HOME", tmp.path());

    let session = simple_session();
    let written = Cline
        .write_session(&session, &WriteOptions { force: false })
        .expect("Cline write_session should succeed");

    assert_eq!(written.paths.len(), 3, "Cline should write 3 task files");
    assert!(
        written.session_id.chars().all(|c| c.is_ascii_digit()),
        "Cline task ids should be numeric"
    );
    assert_eq!(written.resume_command, "code .");

    // The shared task history state file should include the new task id.
    let history_path = tmp.path().join("state/taskHistory.json");
    assert!(
        history_path.is_file(),
        "Cline should write taskHistory.json"
    );
    let history_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    let items = history_json
        .as_array()
        .expect("taskHistory.json should be an array");
    assert!(
        items
            .iter()
            .any(|v| v.get("id").and_then(|x| x.as_str()) == Some(&written.session_id)),
        "taskHistory.json should include the written task id"
    );

    let readback = Cline
        .read_session(&written.paths[0])
        .expect("Cline read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Cline roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(orig.role, rb.role, "Cline roundtrip msg {i}: role mismatch");
        assert_eq!(
            orig.content, rb.content,
            "Cline roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "Cline roundtrip: workspace"
    );
    assert_eq!(
        readback.model_name, session.model_name,
        "Cline roundtrip: model_name should survive via taskHistory.json"
    );
}

// ===========================================================================
// Amp writer tests
// ===========================================================================

#[test]
fn writer_amp_roundtrip() {
    let _lock = AMP_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("XDG_DATA_HOME", tmp.path());

    let session = simple_session();
    let written = Amp
        .write_session(&session, &WriteOptions { force: false })
        .expect("Amp write_session should succeed");

    assert_eq!(written.paths.len(), 1, "Amp should write one thread file");
    assert!(
        written.session_id.starts_with("T-"),
        "Amp session IDs should start with 'T-'"
    );
    assert!(
        written.paths[0].starts_with(tmp.path().join("amp").join("threads")),
        "Amp thread should be written under $XDG_DATA_HOME/amp/threads"
    );
    assert!(
        written.resume_command.contains(&written.session_id),
        "Amp resume command should reference written session ID"
    );

    let readback = Amp
        .read_session(&written.paths[0])
        .expect("Amp read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Amp roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(orig.role, rb.role, "Amp roundtrip msg {i}: role mismatch");
        assert_eq!(
            orig.content, rb.content,
            "Amp roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "Amp roundtrip: workspace"
    );
}

#[test]
fn writer_amp_output_has_expected_shape() {
    let _lock = AMP_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("XDG_DATA_HOME", tmp.path());

    let written = Amp
        .write_session(&simple_session(), &WriteOptions { force: false })
        .expect("Amp write_session should succeed");

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert!(root["id"].is_string(), "Amp thread should have string id");
    assert!(
        root["created"].is_i64(),
        "Amp thread should have numeric created"
    );
    assert!(
        root["messages"].is_array(),
        "Amp thread should have messages array"
    );
    assert_eq!(
        root["messages"].as_array().unwrap().len(),
        4,
        "Amp thread should contain one entry per message"
    );
}

// ===========================================================================
// ChatGPT writer tests
// ===========================================================================

#[test]
fn writer_chatgpt_roundtrip() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CHATGPT_HOME", tmp.path());

    let session = simple_session();
    let written = ChatGpt
        .write_session(&session, &WriteOptions { force: false })
        .expect("ChatGPT write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "ChatGPT should produce exactly one file"
    );
    assert!(
        written.paths[0].exists(),
        "ChatGPT output file should exist"
    );
    assert!(
        written.resume_command.contains("chatgpt.com"),
        "ChatGPT resume command should reference chatgpt.com"
    );

    let readback = ChatGpt
        .read_session(&written.paths[0])
        .expect("ChatGPT read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "ChatGPT roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "ChatGPT roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "ChatGPT roundtrip msg {i}: content mismatch"
        );
    }
}

#[test]
fn writer_chatgpt_output_valid_json_with_mapping() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CHATGPT_HOME", tmp.path());

    let written = ChatGpt
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value =
        serde_json::from_str(&content).expect("ChatGPT output should be valid JSON");

    assert!(root["id"].is_string(), "ChatGPT should have string id");
    assert!(
        root["mapping"].is_object(),
        "ChatGPT should have mapping object"
    );

    let mapping = root["mapping"].as_object().unwrap();
    // 4 messages → 4 mapping nodes (plus possible root node).
    assert!(
        mapping.len() >= 4,
        "ChatGPT mapping should have at least 4 nodes, got {}",
        mapping.len()
    );
}

#[test]
fn writer_chatgpt_timestamps_are_float_seconds() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CHATGPT_HOME", tmp.path());

    let written = ChatGpt
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Top-level timestamps should be numeric (seconds).
    assert!(
        root["create_time"].is_f64() || root["create_time"].is_i64(),
        "ChatGPT create_time should be numeric"
    );
    assert!(
        root["update_time"].is_f64() || root["update_time"].is_i64(),
        "ChatGPT update_time should be numeric"
    );
}

#[test]
fn writer_chatgpt_mapping_has_parent_chain() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CHATGPT_HOME", tmp.path());

    let written = ChatGpt
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let mapping = root["mapping"].as_object().unwrap();

    // Every node with a message should have a parent pointer (string or null).
    for (node_id, node) in mapping {
        if node.get("message").is_some() {
            assert!(
                node.get("parent").is_some(),
                "ChatGPT mapping node '{node_id}' should have parent field"
            );
        }
    }
}

// ===========================================================================
// ClawdBot writer tests
// ===========================================================================

#[test]
fn writer_clawdbot_roundtrip() {
    let _lock = CLAWDBOT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAWDBOT_HOME", tmp.path());

    let session = simple_session();
    let written = ClawdBot
        .write_session(&session, &WriteOptions { force: false })
        .expect("ClawdBot write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "ClawdBot should produce exactly one file"
    );
    assert!(
        written.paths[0].exists(),
        "ClawdBot output file should exist"
    );
    assert!(
        written.resume_command.contains("clawdbot"),
        "ClawdBot resume command should reference clawdbot"
    );

    let readback = ClawdBot
        .read_session(&written.paths[0])
        .expect("ClawdBot read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "ClawdBot roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "ClawdBot roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "ClawdBot roundtrip msg {i}: content mismatch"
        );
    }
}

#[test]
fn writer_clawdbot_output_valid_jsonl() {
    let _lock = CLAWDBOT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAWDBOT_HOME", tmp.path());

    let written = ClawdBot
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // ClawdBot has no session format of its own: it writes the
    // `@mariozechner/pi-coding-agent` `SessionManager` envelope, which is a
    // session header followed by one wrapped entry per message. A top-level
    // `role`/`content` line is a shape nothing in that ecosystem has ever
    // written or been able to read.
    assert_eq!(
        lines.len(),
        5,
        "ClawdBot should write a session header plus one entry per message"
    );

    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        header["type"], "session",
        "first line is the session header"
    );
    assert_eq!(header["id"], "src-simple");
    assert_eq!(header["cwd"], "/data/projects/myapp");

    let mut expected_parent = serde_json::Value::Null;
    for (i, line) in lines[1..].iter().enumerate() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(entry) => entry,
            Err(e) => {
                panic!("ClawdBot line {i} not valid JSON: {e}\nContent: {line}");
            }
        };
        assert_eq!(entry["type"], "message", "ClawdBot entry {i}: type");
        assert!(
            entry["id"].is_string(),
            "ClawdBot entry {i}: needs an entry id"
        );
        assert_eq!(
            entry["parentId"], expected_parent,
            "ClawdBot entry {i}: entries chain through parentId"
        );
        assert!(
            entry["message"]["role"].is_string(),
            "ClawdBot entry {i}: role lives on the wrapped message"
        );
        assert!(
            !entry["message"]["content"].is_null(),
            "ClawdBot entry {i}: should have content"
        );
        expected_parent = entry["id"].clone();
    }
}

#[test]
fn writer_clawdbot_timestamps_are_rfc3339() {
    let _lock = CLAWDBOT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAWDBOT_HOME", tmp.path());

    let written = ClawdBot
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    for (i, line) in content.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(ts_str) = entry["timestamp"].as_str()
            && let Err(e) = chrono::DateTime::parse_from_rfc3339(ts_str)
        {
            panic!("ClawdBot line {i}: timestamp '{ts_str}' not valid RFC3339: {e}");
        }
    }
}

// ===========================================================================
// Vibe writer tests
// ===========================================================================

#[test]
fn writer_vibe_roundtrip() {
    let _lock = VIBE_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("VIBE_HOME", tmp.path());

    let session = simple_session();
    let written = Vibe
        .write_session(&session, &WriteOptions { force: false })
        .expect("Vibe write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "Vibe should produce exactly one file"
    );
    assert!(written.paths[0].exists(), "Vibe output file should exist");
    assert!(
        written.resume_command.contains("vibe"),
        "Vibe resume command should reference vibe"
    );

    let readback = Vibe
        .read_session(&written.paths[0])
        .expect("Vibe read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Vibe roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(orig.role, rb.role, "Vibe roundtrip msg {i}: role mismatch");
        assert_eq!(
            orig.content, rb.content,
            "Vibe roundtrip msg {i}: content mismatch"
        );
    }
}

#[test]
fn writer_vibe_directory_structure() {
    let _lock = VIBE_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("VIBE_HOME", tmp.path());

    let written = Vibe
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let path = &written.paths[0];
    // Should be under <VIBE_HOME>/<session_id>/messages.jsonl
    let filename = path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        filename, "messages.jsonl",
        "Vibe output should be named messages.jsonl"
    );
    let session_dir = path.parent().unwrap();
    assert!(
        session_dir.starts_with(tmp.path()),
        "Vibe session dir should be under VIBE_HOME"
    );
}

#[test]
fn writer_vibe_output_valid_jsonl() {
    let _lock = VIBE_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("VIBE_HOME", tmp.path());

    let written = Vibe
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 4, "Vibe should write one line per message");
    for (i, line) in lines.iter().enumerate() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(entry) => entry,
            Err(e) => {
                panic!("Vibe line {i} not valid JSON: {e}\nContent: {line}");
            }
        };
        assert!(entry["role"].is_string(), "Vibe line {i}: should have role");
        assert!(
            entry["content"].is_string(),
            "Vibe line {i}: should have content"
        );
    }
}

// ===========================================================================
// Factory writer tests
// ===========================================================================

#[test]
fn writer_factory_roundtrip() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("FACTORY_HOME", tmp.path());

    let session = simple_session();
    let written = Factory
        .write_session(&session, &WriteOptions { force: false })
        .expect("Factory write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "Factory should produce exactly one file"
    );
    assert!(
        written.paths[0].exists(),
        "Factory output file should exist"
    );
    assert!(
        written.resume_command.contains("factory"),
        "Factory resume command should reference factory"
    );

    let readback = Factory
        .read_session(&written.paths[0])
        .expect("Factory read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "Factory roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "Factory roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "Factory roundtrip msg {i}: content mismatch"
        );
    }
}

#[test]
fn writer_factory_session_start_header() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("FACTORY_HOME", tmp.path());

    let written = Factory
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(content.lines().next().unwrap()).unwrap();

    assert_eq!(
        first_line["type"], "session_start",
        "Factory first line should be session_start"
    );
    assert!(
        first_line["id"].is_string(),
        "Factory session_start should have id"
    );
    assert!(
        first_line["cwd"].is_string(),
        "Factory session_start should have cwd"
    );
}

#[test]
fn writer_factory_output_valid_jsonl() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("FACTORY_HOME", tmp.path());

    let written = Factory
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // session_start + 4 messages.
    assert_eq!(
        lines.len(),
        5,
        "Factory should write session_start + 4 message lines"
    );
    for (i, line) in lines.iter().enumerate() {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!("Factory line {i} not valid JSON: {e}\nContent: {line}");
        }
    }
}

#[test]
fn writer_factory_message_structure() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("FACTORY_HOME", tmp.path());

    let written = Factory
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Lines after header should be type: "message" with nested message object.
    for (i, entry) in lines.iter().skip(1).enumerate() {
        assert_eq!(
            entry["type"],
            "message",
            "Factory line {}: type should be 'message'",
            i + 1
        );
        assert!(
            entry["message"].is_object(),
            "Factory line {}: should have nested message object",
            i + 1
        );
        assert!(
            entry["message"]["role"].is_string(),
            "Factory line {}: message should have role",
            i + 1
        );
        assert!(
            entry["message"]["content"].is_string(),
            "Factory line {}: message should have content",
            i + 1
        );
    }
}

// ===========================================================================
// OpenClaw writer tests
// ===========================================================================

#[test]
fn writer_openclaw_roundtrip() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("OPENCLAW_HOME", tmp.path());

    let session = simple_session();
    let written = OpenClaw
        .write_session(&session, &WriteOptions { force: false })
        .expect("OpenClaw write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "OpenClaw should produce exactly one file"
    );
    assert!(
        written.paths[0].exists(),
        "OpenClaw output file should exist"
    );
    assert!(
        written.resume_command.contains("openclaw"),
        "OpenClaw resume command should reference openclaw"
    );

    let readback = OpenClaw
        .read_session(&written.paths[0])
        .expect("OpenClaw read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "OpenClaw roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "OpenClaw roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "OpenClaw roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "OpenClaw roundtrip: workspace"
    );
}

#[test]
fn writer_openclaw_session_header() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("OPENCLAW_HOME", tmp.path());

    let written = OpenClaw
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(content.lines().next().unwrap()).unwrap();

    assert_eq!(
        first_line["type"], "session",
        "OpenClaw first line should be type 'session'"
    );
    assert!(
        first_line["id"].is_string(),
        "OpenClaw session header should have id"
    );
    assert!(
        first_line["timestamp"].is_string(),
        "OpenClaw session header should have timestamp"
    );
    // `CURRENT_SESSION_VERSION` in `@openclaw/ai@2026.7.1-2` is the number 3,
    // and OpenClaw compares it numerically: `migrateToCurrentVersion` reads
    // `header.version ?? 1` and returns early on `>= 3`. This used to assert
    // `is_string()`, which only ever held because casr wrote the string
    // "0.1.0" — a value that fails that comparison, so OpenClaw treated every
    // casr-written transcript as needing migration and rewrote it.
    assert_eq!(
        first_line["version"], 3,
        "OpenClaw session header version should be the current schema number"
    );
}

#[test]
fn writer_openclaw_output_valid_jsonl() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("OPENCLAW_HOME", tmp.path());

    let written = OpenClaw
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // session header + model_change + 4 messages. The `model_change` line is
    // there because `simple_session()` names a model, and OpenClaw carries the
    // session model in an entry rather than in the header — its `SessionHeader`
    // has no `modelId` field to put it in.
    assert_eq!(
        lines.len(),
        6,
        "OpenClaw should write session header + model_change + 4 message lines"
    );
    for (i, line) in lines.iter().enumerate() {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!("OpenClaw line {i} not valid JSON: {e}\nContent: {line}");
        }
    }
}

#[test]
fn writer_openclaw_message_ids_are_sequential() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("OPENCLAW_HOME", tmp.path());

    let written = OpenClaw
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Message IDs are m1, m2, m3, m4. Selected by entry type rather than by
    // skipping a fixed prefix: the header is not the only non-message line —
    // a session that names a model also carries a `model_change` entry — and
    // a positional skip silently starts asserting against the wrong rows when
    // that changes.
    let messages: Vec<&serde_json::Value> =
        lines.iter().filter(|e| e["type"] == "message").collect();
    assert_eq!(messages.len(), 4, "expected the 4 message lines");
    for (i, entry) in messages.iter().enumerate() {
        let expected_id = format!("m{}", i + 1);
        assert_eq!(
            entry["id"].as_str().unwrap(),
            expected_id,
            "OpenClaw message {i} should have id '{expected_id}'"
        );
    }
}

#[test]
fn writer_openclaw_tool_calls_in_content() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("OPENCLAW_HOME", tmp.path());

    let written = OpenClaw
        .write_session(&tool_call_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Second message (index 1 after header → line 2) is assistant with tool call.
    let assistant = &lines[2];
    let msg_content = &assistant["message"]["content"];

    // Content should be array when tool calls exist.
    if let Some(arr) = msg_content.as_array() {
        let has_tool = arr.iter().any(|b| b["type"] == "toolCall");
        assert!(
            has_tool,
            "OpenClaw assistant with tool calls should have toolCall block"
        );
    }
}

// ===========================================================================
// Pi-Agent writer tests
// ===========================================================================

#[test]
fn writer_piagent_roundtrip() {
    let _lock = PI_AGENT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("PI_AGENT_HOME", tmp.path());

    let session = simple_session();
    let written = PiAgent
        .write_session(&session, &WriteOptions { force: false })
        .expect("PiAgent write_session should succeed");

    assert_eq!(
        written.paths.len(),
        1,
        "PiAgent should produce exactly one file"
    );
    assert!(
        written.paths[0].exists(),
        "PiAgent output file should exist"
    );
    assert!(
        written.resume_command.contains("pi --session"),
        "PiAgent resume command should reference pi --session"
    );

    let readback = PiAgent
        .read_session(&written.paths[0])
        .expect("PiAgent read_session should parse written output");

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "PiAgent roundtrip: message count"
    );
    for (i, (orig, rb)) in session
        .messages
        .iter()
        .zip(readback.messages.iter())
        .enumerate()
    {
        assert_eq!(
            orig.role, rb.role,
            "PiAgent roundtrip msg {i}: role mismatch"
        );
        assert_eq!(
            orig.content, rb.content,
            "PiAgent roundtrip msg {i}: content mismatch"
        );
    }
    assert_eq!(
        readback.workspace, session.workspace,
        "PiAgent roundtrip: workspace"
    );
}

#[test]
fn writer_piagent_session_header() {
    let _lock = PI_AGENT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("PI_AGENT_HOME", tmp.path());

    let written = PiAgent
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(content.lines().next().unwrap()).unwrap();

    assert_eq!(
        first_line["type"], "session",
        "PiAgent first line should be type 'session'"
    );
    assert!(
        first_line["id"].is_string(),
        "PiAgent session header should have id"
    );
    assert!(
        first_line["timestamp"].is_string(),
        "PiAgent session header should have timestamp"
    );
}

#[test]
fn writer_piagent_filename_has_underscore() {
    let _lock = PI_AGENT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("PI_AGENT_HOME", tmp.path());

    let written = PiAgent
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let filename = written.paths[0].file_name().unwrap().to_str().unwrap();
    assert!(
        filename.contains('_'),
        "PiAgent filename should contain underscore for discovery, got '{filename}'"
    );
    assert!(
        filename.ends_with(".jsonl"),
        "PiAgent filename should end with .jsonl"
    );
}

#[test]
fn writer_piagent_output_valid_jsonl() {
    let _lock = PI_AGENT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("PI_AGENT_HOME", tmp.path());

    let written = PiAgent
        .write_session(&simple_session(), &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // session header + 4 messages.
    assert_eq!(
        lines.len(),
        5,
        "PiAgent should write session header + 4 message lines"
    );
    for (i, line) in lines.iter().enumerate() {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!("PiAgent line {i} not valid JSON: {e}\nContent: {line}");
        }
    }
}

#[test]
fn writer_piagent_tool_role_normalized() {
    let _lock = PI_AGENT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("PI_AGENT_HOME", tmp.path());

    let mut session = simple_session();
    // Replace a message with Tool role.
    session.messages[2] = CanonicalMessage {
        idx: 2,
        role: MessageRole::Tool,
        content: "File contents here".to_string(),
        timestamp: Some(1_700_000_007_000),
        author: None,
        tool_calls: vec![],
        tool_results: vec![],
        extra: serde_json::Value::Null,
    };

    let written = PiAgent
        .write_session(&session, &WriteOptions { force: false })
        .unwrap();

    let content = std::fs::read_to_string(&written.paths[0]).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // The Tool role message (line 3, index 2 after header) should be written as toolResult.
    let tool_line = &lines[3]; // header + user + assistant + tool
    let role = tool_line["message"]["role"].as_str().unwrap_or("");
    assert_eq!(
        role, "toolResult",
        "PiAgent should normalize Tool role to 'toolResult'"
    );
}

// ===========================================================================
// Structured IR readers and writers
//
// Everything above is the flat track. The block below pins the four structured
// modules — `codex_ir`, `codex_ir_write`, `claude_code_ir`,
// `claude_code_ir_write` — against silent loss: material that crosses without a
// `Loss`, a state assignment built out of a field that was not there, and a
// sealed field re-labelled by the field it arrived in rather than by the vendor
// that minted it. None of these tests touch the process environment, so none of
// them take the env lock.
// ===========================================================================

mod structured_ir {
    use std::io::Write;

    use casr::budget::ContextBudget;
    use casr::ir::{
        Block, Body, Capsule, CapsuleBinding, CapsuleKind, Event, Fidelity, SessionIr, ToolInput,
        Visibility,
    };
    use casr::providers::{claude_code_ir, claude_code_ir_write, codex_ir, codex_ir_write};
    use serde_json::{Value, json};

    fn write_lines(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        file.flush().expect("flush");
        file
    }

    fn reparse<T>(lines: &[String], read: fn(&std::path::Path) -> anyhow::Result<T>) -> T {
        let file = write_lines(lines);
        read(file.path()).expect("the writer produced a session its own reader rejects")
    }

    const CODEX_META: &str = r#"{"timestamp":"2026-07-25T10:00:00.000Z","type":"session_meta","payload":{"id":"s-1","cli_version":"0.145.0","model_provider":"openai","cwd":"/work","timestamp":"2026-07-25T10:00:00.000Z"}}"#;

    fn claude_user(uuid: &str, parent: &str, text: &str) -> String {
        let parent = if parent.is_empty() {
            "null".to_string()
        } else {
            format!("\"{parent}\"")
        };
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent},"isSidechain":false,"sessionId":"s1","cwd":"/work","version":"2.1.220","timestamp":"2026-07-25T10:00:00.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn codex_rollout(lines: &[String]) -> SessionIr {
        let mut all = vec![CODEX_META.to_string()];
        all.extend_from_slice(lines);
        let file = write_lines(&all);
        codex_ir::read(file.path()).expect("rollout parses")
    }

    fn claude_transcript(lines: &[String]) -> SessionIr {
        let file = write_lines(lines);
        claude_code_ir::read(file.path()).expect("transcript parses")
    }

    fn event(id: &str, line: u64, body: Body) -> Event {
        Event {
            id: id.to_string(),
            parent: None,
            branch: casr::ir::Branch::Main,
            turn: Some("t1".to_string()),
            ts: None,
            visibility: Visibility::Model,
            body,
            capsules: Vec::new(),
            source: casr::ir::SourceRef {
                line,
                sha256: String::new(),
            },
        }
    }

    fn text_message(id: &str, line: u64, role: casr::ir::Role, text: &str) -> Event {
        event(
            id,
            line,
            Body::Message {
                role,
                blocks: vec![Block::Text { text: text.into() }],
            },
        )
    }

    // -----------------------------------------------------------------------
    // F1 — a target-native sealed field must not be minted out of an
    // unrecognised block
    // -----------------------------------------------------------------------

    /// `codex_ir` re-labels an `encrypted_content` item as an OpenAI capsule by
    /// the field it arrived in. A block that reaches the Codex writer as
    /// [`Block::Unknown`] never passed a vendor gate, so writing its bytes back
    /// verbatim mints an OpenAI capsule out of material no gate ever saw.
    #[test]
    fn a_sealed_envelope_inside_an_unknown_block_is_not_written_as_a_codex_capsule() {
        let anthropic_blob = "ANTHROPIC_SEALED_BLOB_AAAA";
        let source = claude_transcript(&[
            claude_user("u1", "", "hi"),
            format!(
                r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:01.000Z","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"encrypted_content","encrypted_content":"{anthropic_blob}"}},{{"type":"text","text":"done"}}]}}}}"#
            ),
        ]);
        assert_eq!(
            source
                .model_visible()
                .iter()
                .map(|event| event.capsules.len())
                .sum::<usize>(),
            0,
            "the reader files this under Block::Unknown, so no capsule gate ever sees it"
        );

        let rendered = codex_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");

        assert!(
            !rendered.lines.iter().any(|line| line.contains(anthropic_blob)),
            "an Anthropic blob written into a Codex rollout is bytes OpenAI must reject"
        );

        let back = reparse(&rendered.lines, codex_ir::read);
        let relabelled: Vec<CapsuleKind> = back
            .model_visible()
            .iter()
            .flat_map(|event| event.capsules.iter())
            .map(|capsule| capsule.kind)
            .collect();
        assert!(
            relabelled.is_empty(),
            "the blob came back as {relabelled:?}: a sealed field was re-labelled by the \
             field it arrived in rather than by the vendor that minted it"
        );
        assert!(
            !rendered.losses.is_empty(),
            "dropping sealed material silently is the failure this gate exists to stop"
        );
    }

    /// The mirror image: an unrecognised block carrying an Anthropic sealed
    /// field must not be written into a Claude transcript, where the reader
    /// would read it back as a real `thinking` capsule.
    #[test]
    fn a_sealed_envelope_inside_an_unknown_block_is_not_written_as_a_claude_capsule() {
        let mut source = SessionIr::new("codex", "s1");
        source.origin.provider = Some("openai".into());
        source.events = vec![
            text_message("u1", 1, casr::ir::Role::User, "hi"),
            event(
                "a1",
                2,
                Body::Message {
                    role: casr::ir::Role::Assistant,
                    blocks: vec![
                        Block::Unknown {
                            native_type: Some("redacted_thinking".into()),
                            raw: json!({"type": "redacted_thinking", "data": "OPENAI_SEALED_XYZ"}),
                        },
                        Block::Text {
                            text: "done".into(),
                        },
                    ],
                },
            ),
        ];

        let rendered = claude_code_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");
        let back = reparse(&rendered.lines, claude_code_ir::read);
        let minted: Vec<CapsuleKind> = back
            .model_visible()
            .iter()
            .flat_map(|event| event.capsules.iter())
            .map(|capsule| capsule.kind)
            .collect();
        assert!(
            minted.is_empty(),
            "an unrecognised block became {minted:?}: Anthropic will reject a signature \
             it never issued"
        );
    }

    // -----------------------------------------------------------------------
    // F2 — a missing context list is not an empty one
    // -----------------------------------------------------------------------

    /// A `compacted` record with no array `replacement_history` supersedes the
    /// whole conversation with nothing. Compaction is a state assignment, so an
    /// empty one deletes everything that came before it.
    #[test]
    fn a_codex_compaction_with_no_replacement_history_keeps_the_live_context() {
        let ir = codex_rollout(&[
            r#"{"timestamp":"2026-07-25T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#.to_string(),
            r#"{"timestamp":"2026-07-25T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second"}]}}"#.to_string(),
            // Syntactically valid, and `replacement_history` is simply absent.
            r#"{"timestamp":"2026-07-25T10:00:03.000Z","type":"compacted","payload":{"window_id":"w2","previous_window_id":"w1"}}"#.to_string(),
        ]);

        let visible = ir.model_visible();
        let texts: Vec<String> = visible
            .iter()
            .filter_map(|event| {
                let Body::Message { blocks, .. } = &event.body else {
                    return None;
                };
                Some(blocks.iter().filter_map(Block::as_text).collect::<String>())
            })
            .collect();
        assert_eq!(
            texts,
            ["first", "second"],
            "a compaction that names no replacement history must not delete the conversation"
        );
        assert!(
            ir.capture.unknown > 0 || !ir.capture.notes.is_empty(),
            "and the malformed record has to be loud: {:?}",
            ir.capture
        );
    }

    /// Same shape on the Claude side: a `compact_boundary` whose
    /// `preservedMessages.allUuids` is missing or not an array.
    #[test]
    fn a_claude_boundary_with_no_preserved_list_keeps_the_live_context() {
        let ir = claude_transcript(&[
            claude_user("u1", "", "first"),
            claude_user("u2", "u1", "second"),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","logicalParentUuid":"u2","sessionId":"s1","compactMetadata":{"trigger":"auto","preservedMessages":{"anchorUuid":"u2"}}}"#.to_string(),
        ]);

        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            visible,
            ["u1", "u2"],
            "a boundary that names no preserved set must not supersede the whole session"
        );
        assert!(
            ir.capture.unknown > 0 || !ir.capture.notes.is_empty(),
            "and it has to be loud: {:?}",
            ir.capture
        );
    }

    // -----------------------------------------------------------------------
    // F3 — the collision repair must not collide
    // -----------------------------------------------------------------------

    /// `#dup<n>` is minted from `events.len()`, which an input-controlled uuid
    /// can name in advance. `Sink::emit` does not re-check after renaming, so
    /// the minted id lands on top of the existing one.
    #[test]
    fn the_duplicate_id_repair_never_mints_an_id_the_transcript_already_used() {
        let ir = claude_transcript(&[
            // events.len() == 1 after this one, so `a#dup2` is what the repair
            // will mint for the third record.
            claude_user("a#dup2", "", "planted"),
            claude_user("a", "", "first"),
            claude_user("a", "", "edited"),
        ]);

        let mut ids: Vec<&str> = ir.events.iter().map(|e| e.id.as_str()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            total,
            "two events share an id, which is the ambiguity the sink exists to remove: {:?}",
            ir.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(ir.events.len(), 3, "nothing may be dropped");
    }

    // -----------------------------------------------------------------------
    // F4 — capsules on a message are capsules
    // -----------------------------------------------------------------------

    /// A Codex `agent_message` carries readable text *and* sealed material —
    /// 4,638 of them in the local corpus. The Claude writer writes the text and
    /// never looks at the capsules, so the seal is dropped with no counter and
    /// no `Loss`, and the conversion grades itself complete.
    #[test]
    fn a_foreign_capsule_on_a_message_is_counted_when_it_is_dropped() {
        let mut assistant = text_message("a1", 2, casr::ir::Role::Assistant, "done");
        assistant.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
            bound: CapsuleBinding {
                provider: "openai".into(),
                model: None,
            },
            sealed: "OPENAI_SEALED_ON_A_MESSAGE".into(),
        });
        let mut source = SessionIr::new("codex", "s1");
        source.origin.provider = Some("openai".into());
        source.events = vec![text_message("u1", 1, casr::ir::Role::User, "hi"), assistant];

        let rendered = claude_code_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");
        let back = reparse(&rendered.lines, claude_code_ir::read);
        let report = casr::compare::compare(&source, &back, "anthropic");

        assert!(
            !rendered.losses.is_empty(),
            "the capsule was dropped and nothing was recorded"
        );
        assert!(
            rendered.fidelity >= Fidelity::ContextNoReasoning,
            "a conversion that dropped sealed material may not grade itself {:?}",
            rendered.fidelity
        );
        assert!(
            report.fidelity() <= rendered.fidelity,
            "the writer claimed {:?}; the file only supports {:?}",
            rendered.fidelity,
            report.fidelity()
        );
    }

    // -----------------------------------------------------------------------
    // F5 — an Anthropic capsule has a subtype
    // -----------------------------------------------------------------------

    /// `redacted_thinking.data` and `thinking.signature` are both Anthropic
    /// capsules and both pass the vendor gate, but they are not the same field.
    /// Writing the first into the second hands Anthropic a signature it never
    /// issued, and the round trip changes the capsule kind while the loss list
    /// stays empty.
    #[test]
    fn a_redacted_thinking_capsule_goes_back_as_redacted_thinking() {
        let source = claude_transcript(&[
            claude_user("u1", "", "hi"),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:01.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"redacted_thinking","data":"REDACTED_SEALED_BLOB"},{"type":"text","text":"done"}]}}"#.to_string(),
        ]);
        let kinds: Vec<CapsuleKind> = source
            .model_visible()
            .iter()
            .flat_map(|event| event.capsules.iter())
            .map(|capsule| capsule.kind)
            .collect();
        assert_eq!(kinds, [CapsuleKind::AnthropicRedactedThinking]);

        let rendered = claude_code_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");
        let back = reparse(&rendered.lines, claude_code_ir::read);
        let round_tripped: Vec<CapsuleKind> = back
            .model_visible()
            .iter()
            .flat_map(|event| event.capsules.iter())
            .map(|capsule| capsule.kind)
            .collect();
        assert_eq!(
            round_tripped,
            [CapsuleKind::AnthropicRedactedThinking],
            "the capsule changed kind crossing a same-vendor write, and no loss says so"
        );
        assert!(rendered.losses.is_empty(), "{:?}", rendered.losses);
    }

    // -----------------------------------------------------------------------
    // F6 — block order is model-visible
    // -----------------------------------------------------------------------

    /// Text that came before a tool call must not be replayed after it.
    #[test]
    fn text_before_a_tool_use_stays_before_it() {
        let source = claude_transcript(&[
            claude_user("u1", "", "hi"),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"before"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#.to_string(),
        ]);

        let kinds: Vec<&str> = source
            .model_visible()
            .iter()
            .map(|event| event.body.kind())
            .collect();
        assert_eq!(
            kinds,
            ["message", "message", "tool_call"],
            "the reader moved the text after the tool call: {kinds:?}"
        );

        let rendered = claude_code_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");
        let record: Value =
            serde_json::from_str(rendered.lines.last().expect("assistant record")).expect("json");
        let types: Vec<&str> = record["message"]["content"]
            .as_array()
            .expect("content array")
            .iter()
            .map(|block| block["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            types,
            ["text", "tool_use"],
            "the model was shown the text first and must be shown it first again"
        );
    }

    // -----------------------------------------------------------------------
    // F8 — `tool_use.input` is an object
    // -----------------------------------------------------------------------

    /// Codex writes `function_call.arguments` as a JSON string. Handing that
    /// string to Claude as `tool_use.input` produces a transcript the Anthropic
    /// API rejects: all 15,607 corpus `tool_use` blocks carry an object.
    #[test]
    fn a_json_tool_call_reaches_claude_as_an_object() {
        let mut source = SessionIr::new("codex", "s1");
        source.origin.provider = Some("openai".into());
        source.events = vec![event(
            "c1",
            1,
            Body::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                namespace: None,
                input: ToolInput::Json {
                    value: json!({"path": "a.txt"}),
                    original: Some("{\"path\":\"a.txt\"}".into()),
                },
            },
        )];

        let rendered = claude_code_ir_write::render(
            &source,
            "sid",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("renders");
        let record: Value = serde_json::from_str(&rendered.lines[0]).expect("json");
        let input = &record["message"]["content"][0]["input"];
        assert!(
            input.is_object(),
            "`tool_use.input` must be an object, not {input}"
        );
        assert_eq!(input["path"], json!("a.txt"));
    }

    // -----------------------------------------------------------------------
    // F9 — a replacement message keeps its own turn
    // -----------------------------------------------------------------------

    /// Every one of the 176,027 `replacement_history` items in the local corpus
    /// carries its own `turn_id`, and a single `compacted` record routinely
    /// spans dozens of distinct ones. Stamping the builder's current turn on all
    /// of them collapses them into one, and `replay::roll_back` counts distinct
    /// turns — so one rollback then removes the entire compacted history.
    #[test]
    fn compacted_replacement_messages_keep_their_own_turn() {
        let ir = codex_rollout(&[
            r#"{"timestamp":"2026-07-25T10:00:01.000Z","type":"turn_context","payload":{"turn_id":"t9","model":"gpt-5"}}"#.to_string(),
            r#"{"timestamp":"2026-07-25T10:00:02.000Z","type":"compacted","payload":{"window_id":"w2","replacement_history":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"one"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"two"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t2"}}
            ]}}"#.replace('\n', "").replace("                ", ""),
        ]);

        let turns: Vec<Option<&str>> = ir
            .model_visible()
            .iter()
            .filter(|event| matches!(event.body, Body::Message { .. }))
            .map(|event| event.turn.as_deref())
            .collect();
        assert_eq!(
            turns,
            [Some("t1"), Some("t2")],
            "the replacement items were re-stamped with the builder's turn"
        );

        // And the writer has to put it back, or the round trip loses it.
        let rendered =
            codex_ir_write::render(&ir, "sid", chrono::Utc::now(), &ContextBudget::UNLIMITED)
                .expect("renders");
        let back = reparse(&rendered.lines, codex_ir::read);
        let back_turns: Vec<Option<&str>> = back
            .model_visible()
            .iter()
            .filter(|event| matches!(event.body, Body::Message { .. }))
            .map(|event| event.turn.as_deref())
            .collect();
        assert_eq!(back_turns, [Some("t1"), Some("t2")]);
    }

    // -----------------------------------------------------------------------
    // F7 — `replacement_history` may not truncate an event's payloads
    // -----------------------------------------------------------------------

    /// A compaction whose preserved context holds a tool pair must write both
    /// halves back. The old fallback took `payloads(event).into_iter().next()`,
    /// which is a silent truncation waiting for the first body that renders as
    /// more than one native item.
    #[test]
    fn every_payload_of_a_preserved_event_reaches_replacement_history() {
        let mut sealed = event(
            "cmp",
            1,
            Body::SealedContext {
                native_id: Some("cmp_1".into()),
                meta: Value::Null,
            },
        );
        sealed.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiCompactedContext,
            bound: CapsuleBinding {
                provider: "openai".into(),
                model: None,
            },
            sealed: "CCCC".into(),
        });
        let mut source = SessionIr::new("codex", "s1");
        source.origin.provider = Some("openai".into());
        source.events = vec![
            sealed,
            event(
                "call",
                2,
                Body::ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    namespace: None,
                    input: ToolInput::Freeform {
                        text: "ls".into(),
                    },
                },
            ),
            event(
                "cm",
                3,
                Body::Compaction {
                    context: vec!["cmp".into(), "call".into()],
                    supersedes: Vec::new(),
                    note: None,
                    window_from: None,
                    window_to: None,
                },
            ),
        ];

        let rendered =
            codex_ir_write::render(&source, "sid", chrono::Utc::now(), &ContextBudget::UNLIMITED)
                .expect("renders");
        let compacted: Value = rendered
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == json!("compacted"))
            .expect("a compacted envelope");
        let history = compacted["payload"]["replacement_history"]
            .as_array()
            .expect("replacement history");
        assert_eq!(history.len(), 2, "both preserved events must be written");
        assert_eq!(history[1]["type"], json!("custom_tool_call"));
    }
}
