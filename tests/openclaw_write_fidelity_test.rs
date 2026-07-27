//! What an OpenClaw transcript casr wrote must still say once OpenClaw — or
//! casr — reads it back.
//!
//! Every assertion here is anchored to a measurement taken against the shipped
//! packages (`npm pack openclaw@2026.7.1-2`), by running OpenClaw's own
//! `loadEntriesFromFile` → `buildSessionContext` → `convertToLlm` and its
//! `readTranscriptFileState` record validator over the bytes casr produces.
//! casr reading back its own output is not an oracle; the vendor's reader is.

mod test_env;

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole};
use casr::providers::openclaw::OpenClaw;
use casr::providers::{Provider, WriteOptions};
use serde_json::json;

static OPENCLAW_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: every caller holds `OPENCLAW_ENV` for the whole lifetime of
        // the guard, so no other test reads or mutates the environment
        // concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn message(idx: usize, role: MessageRole, content: &str) -> CanonicalMessage {
    CanonicalMessage {
        idx,
        role,
        content: content.to_string(),
        timestamp: Some(1_700_000_000_000 + idx as i64 * 1000),
        author: None,
        tool_calls: vec![],
        tool_results: vec![],
        extra: json!({}),
    }
}

fn session_with(messages: Vec<CanonicalMessage>, model_name: Option<&str>) -> CanonicalSession {
    CanonicalSession {
        session_id: "write-fidelity".to_string(),
        provider_slug: "kiro".to_string(),
        workspace: Some(PathBuf::from("/home/user/project")),
        title: None,
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages,
        metadata: json!({}),
        source_path: PathBuf::from("/tmp/source.json"),
        model_name: model_name.map(String::from),
    }
}

/// Write through the real `write_session` and read the file back through the
/// real `read_session`, exactly as `pipeline`'s read-back verification does.
fn write_then_read(session: &CanonicalSession) -> (CanonicalSession, String) {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("OPENCLAW_STATE_DIR", tmp.path());

    let written = OpenClaw
        .write_session(session, &WriteOptions { force: false })
        .expect("OpenClaw write_session");
    let path = written.paths.first().expect("a written path");
    let bytes = std::fs::read_to_string(path).expect("written transcript is readable");
    let readback = OpenClaw.read_session(path).expect("OpenClaw read_session");
    (readback, bytes)
}

fn entries(rendered: &str) -> Vec<serde_json::Value> {
    rendered
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every written line is JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// The regression: a system turn used to make the whole conversion fail
// ---------------------------------------------------------------------------

/// `AgentMessage` (`dist/types-D0CdrmU4.d.ts`) is
/// `UserMessage | AssistantMessage | ToolResultMessage | BashExecutionMessage |
/// CustomMessage`. There is no `system` member, and the writer emitting one
/// made every conversion of a session with a system turn fail read-back
/// verification outright.
///
/// Measured on `openclaw@2026.7.1-2`: a `{"role":"system"}` row is dropped by
/// `convertToLlm` (`default: return;`) and rejected by the persisted-record
/// validator — `readTranscriptFileState` accepted only the *other* row.
#[test]
fn a_system_turn_survives_a_write_and_read_back() {
    let session = session_with(
        vec![
            message(0, MessageRole::System, "You are a helpful assistant."),
            message(1, MessageRole::User, "Fix the bug"),
            message(2, MessageRole::Assistant, "On it."),
        ],
        None,
    );

    let (readback, rendered) = write_then_read(&session);

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "a system turn must survive: this is the read-back count the pipeline \
         verifies, and a mismatch is a hard VerifyFailed + rollback\nrendered:\n{rendered}"
    );
    assert_eq!(readback.messages[0].content, "You are a helpful assistant.");

    // And the file must not name a role OpenClaw cannot persist.
    let persisted = ["user", "assistant", "toolResult", "bashExecution", "custom"];
    for entry in entries(&rendered) {
        if entry["type"] != "message" {
            continue;
        }
        let role = entry["message"]["role"].as_str().unwrap_or("");
        assert!(
            persisted.contains(&role),
            "role {role:?} is outside the union `appendMessage` can persist"
        );
    }
}

/// `MessageRole::Other` has the same problem and the same answer: whatever the
/// source tool called the turn, OpenClaw has exactly one non-assistant,
/// non-tool slot the model actually sees.
#[test]
fn an_unrecognised_source_role_survives_a_write_and_read_back() {
    let session = session_with(
        vec![
            message(0, MessageRole::Other("developer".to_string()), "Be terse."),
            message(1, MessageRole::User, "Hi"),
        ],
        None,
    );

    let (readback, rendered) = write_then_read(&session);

    assert_eq!(
        readback.messages.len(),
        session.messages.len(),
        "an Other(_) turn must survive\nrendered:\n{rendered}"
    );
    assert_eq!(readback.messages[0].content, "Be terse.");
}

/// The end-to-end shape the bug was reported as: a real conversion, through the
/// CLI, of a session whose reader emits [`MessageRole::System`].
///
/// ClawdBot is the source because its reader emits `System` for a `system` row
/// (`pi_session` normalises the role) and its transcripts are a flat directory
/// of JSONL files, so the fixture is three lines and no manifest.
#[test]
fn a_system_turn_survives_an_end_to_end_conversion_into_openclaw() {
    let tmp = tempfile::tempdir().unwrap();
    let clawdbot = tmp.path().join("clawdbot");
    std::fs::create_dir_all(&clawdbot).unwrap();
    std::fs::write(
        clawdbot.join("sess-with-system.jsonl"),
        [
            r#"{"type":"session","version":2,"id":"sess-with-system","timestamp":"2026-02-14T09:12:00.000Z","cwd":"/home/user/project"}"#,
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"system","content":"You are helpful."}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:05.000Z","message":{"role":"user","content":"Hi"}}"#,
            r#"{"type":"message","id":"c3","parentId":"b2","timestamp":"2026-02-14T09:12:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let binary = env!("CARGO_BIN_EXE_casr");
    let output = StdCommand::new(binary)
        .args([
            "--json",
            "resume",
            "ocl",
            "sess-with-system",
            "--source",
            "cwb",
            "--force",
        ])
        .env("CLAWDBOT_HOME", &clawdbot)
        .env("OPENCLAW_STATE_DIR", tmp.path().join("openclaw"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("NO_COLOR", "1")
        .output()
        .expect("casr should run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "ClawdBot→OpenClaw conversion of a session with a system turn must \
         succeed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true, "{stdout}");
    let written = parsed["written_paths"][0]
        .as_str()
        .expect("a written path")
        .to_string();
    let rendered = std::fs::read_to_string(&written).expect("written transcript is readable");
    let message_rows = entries(&rendered)
        .into_iter()
        .filter(|entry| entry["type"] == "message")
        .count();
    assert_eq!(message_rows, 3, "all three turns reached the file");
}

// ---------------------------------------------------------------------------
// The session's model
// ---------------------------------------------------------------------------

/// The writer stated the model nowhere: not in the header (`SessionHeader` has
/// no `modelId` field — `dist/session-manager-RXl7XED7.d.ts`) and not as a
/// `model_change` entry, so `model_name` came back `None` from every
/// conversion into OpenClaw.
#[test]
fn the_session_model_survives_a_write_and_read_back() {
    let session = session_with(
        vec![
            message(0, MessageRole::User, "Fix the bug"),
            message(1, MessageRole::Assistant, "On it."),
        ],
        Some("claude-opus-4-5"),
    );

    let (readback, rendered) = write_then_read(&session);

    assert_eq!(
        readback.model_name,
        Some("claude-opus-4-5".to_string()),
        "the session's model must survive the round trip\nrendered:\n{rendered}"
    );
}

/// And it must be stated in the record OpenClaw itself reads it from:
/// `buildSessionContext` resolves the model from `model_change` entries, never
/// from the header.
#[test]
fn the_session_model_is_written_as_a_model_change_entry() {
    let session = session_with(
        vec![message(0, MessageRole::User, "Fix the bug")],
        Some("claude-opus-4-5"),
    );

    let (_, rendered) = write_then_read(&session);
    let rows = entries(&rendered);

    let model_change = rows
        .iter()
        .find(|entry| entry["type"] == "model_change")
        .unwrap_or_else(|| panic!("a model_change entry\nrendered:\n{rendered}"));
    assert_eq!(model_change["modelId"], "claude-opus-4-5");
    assert!(
        model_change["id"].is_string(),
        "SessionEntryBase requires an id"
    );
    assert!(
        model_change.get("parentId").is_some(),
        "SessionEntryBase requires parentId, chained like every other entry"
    );
    assert!(
        model_change["timestamp"].is_string(),
        "SessionEntryBase's timestamp is an ISO string"
    );
    // casr does not observe the LLM provider of a converted session, so the
    // field is absent rather than guessed. Absence is a fact; "anthropic"
    // inferred from a model-id prefix would be a claim.
    assert!(
        model_change.get("provider").is_none_or(|v| v.is_null()),
        "provider must not be invented"
    );

    // Nothing may claim a model in the header: SessionHeader is exactly
    // {type, version?, id, timestamp, cwd, parentSession?}.
    let header = rows
        .iter()
        .find(|entry| entry["type"] == "session")
        .expect("a header");
    assert!(
        header.get("modelId").is_none(),
        "SessionHeader has no modelId field"
    );
}

/// A session whose model casr never learned must not gain a `model_change` row
/// stating one.
#[test]
fn no_model_change_is_written_when_the_model_is_unknown() {
    let session = session_with(vec![message(0, MessageRole::User, "Fix the bug")], None);
    let (readback, rendered) = write_then_read(&session);

    assert!(
        !entries(&rendered)
            .iter()
            .any(|entry| entry["type"] == "model_change"),
        "no model, no claim\nrendered:\n{rendered}"
    );
    assert_eq!(readback.model_name, None);
}

// ---------------------------------------------------------------------------
// Side-appended entries are not a rewind
// ---------------------------------------------------------------------------

/// `appendMode: "side"` marks an entry parked by
/// `mergePromptReleasedSessionEntries` — "entries appended while the active
/// prompt released its file lock … attached as a side branch so rewrites
/// retain external state without moving the prepared reply branch".
///
/// casr is right that such an entry is not model-visible, and wrong to file it
/// under "abandoned", which is the report for a path the *user* rewound away
/// from.
#[test]
fn a_side_appended_entry_is_not_reported_as_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("side.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","version":3,"id":"side","timestamp":"2026-02-01T16:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Hi"}}"#,
            r#"{"type":"message","id":"s1","parentId":"m1","appendMode":"side","timestamp":"2026-02-01T16:00:02Z","message":{"role":"user","content":"delivered while the lock was released"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let session = OpenClaw.read_session(&path).expect("read_session");
    let unrepresented = session.metadata["unrepresented"]
        .as_str()
        .expect("the side-appended entry is reported")
        .to_string();

    assert_eq!(
        session.messages.len(),
        2,
        "the side branch is not model-visible"
    );
    assert!(
        !unrepresented.contains("abandoned"),
        "a side append is not a rewind; got {unrepresented:?}"
    );
    assert!(
        unrepresented.contains("side"),
        "it needs a counter that says what it is; got {unrepresented:?}"
    );
}

/// A genuine rewind still reports as abandoned, so the new counter did not just
/// rename the old one.
#[test]
fn a_rewound_branch_is_still_reported_as_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rewound.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","version":3,"id":"rewound","timestamp":"2026-02-01T16:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Hi"}}"#,
            r#"{"type":"message","id":"dead","parentId":"m1","timestamp":"2026-02-01T16:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"rewound away"}]}}"#,
            r#"{"type":"leaf","id":"l1","targetId":"m1","timestamp":"2026-02-01T16:00:03Z"}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"kept"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let session = OpenClaw.read_session(&path).expect("read_session");
    let unrepresented = session.metadata["unrepresented"]
        .as_str()
        .expect("the rewound entry is reported")
        .to_string();
    assert!(unrepresented.contains("abandoned"), "got {unrepresented:?}");
}
