//! Where Cursor and Kiro put a turn they have no slot for (defect #74).
//!
//! Both formats carry exactly two conversational speakers, so a system prompt
//! and an unrecognised source role have to be folded onto one of them.
//! `pipeline::folded_role` already declares *that* a fold happens and
//! `conformance_test::no_writer_folds_a_role_without_declaring_it` keeps that
//! declaration honest. What neither of them constrains is **which** speaker,
//! and until #74 both writers chose the agent — telling a resumed session that
//! the model itself issued the operator's instruction.
//!
//! # Why this reads the file and not the session
//!
//! casr's own reader is not an oracle for casr's own writer. `determine_bubble_role`
//! maps an unknown numeric bubble type to `MessageRole::Assistant` and
//! `parse_envelope` maps `"AssistantMessage"` to `MessageRole::Assistant`, so a
//! writer that picked the wrong speaker and a reader that agrees with it produce
//! a perfectly clean round trip. These tests therefore open the SQLite value and
//! the JSONL line and assert on the raw `type` / `kind` that the vendor's own
//! code will read.
//!
//! # The vendor vocabularies these numbers come from
//!
//! * Cursor 3.13.10, `resources/app/out/vs/workbench/workbench.desktop.main.js`:
//!   `makeEnum("aiserver.v1.ConversationMessage.MessageType", [{no:0,
//!   name:"MESSAGE_TYPE_UNSPECIFIED"}, {no:1, name:"MESSAGE_TYPE_HUMAN"},
//!   {no:2, name:"MESSAGE_TYPE_AI"}])`. No system member, and `UNSPECIFIED` is
//!   referenced nowhere in that bundle.
//! * `kiro-cli-chat` 2.14.2: `adjacently tagged enum LogEntryV1` with
//!   `struct variant LogEntryV1::{Prompt, AssistantMessage, ToolResults,
//!   Compaction, ResetTo}`. No system member, and an adjacently tagged enum
//!   with no unit variant rejects a sixth `kind` outright.

mod test_env;

use std::path::{Path, PathBuf};

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolResult};
use casr::providers::cursor::Cursor;
use casr::providers::kiro::Kiro;
use casr::providers::{Provider, WriteOptions};

static ENV: test_env::EnvLock = test_env::EnvLock;

/// Every `kind` the shipped `kiro-cli-chat` will deserialize. A `kind` outside
/// this set is an `unknown variant` error, not a tolerated extension.
const LOG_ENTRY_V1_KINDS: [&str; 5] = [
    "Prompt",
    "AssistantMessage",
    "ToolResults",
    "Compaction",
    "ResetTo",
];

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold `ENV` for the duration, so no other thread reads
        // or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(val) => unsafe { std::env::set_var(self.key, val) },
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
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        extra: serde_json::json!({}),
    }
}

/// One turn per role, each with text unique enough to find in the artifact.
fn operator_session() -> CanonicalSession {
    let mut observation = message(4, MessageRole::Tool, "OBSERVATION matched 3 lines");
    observation.tool_results = vec![ToolResult {
        call_id: Some("c1".to_string()),
        content: "matched 3 lines".to_string(),
        is_error: false,
    }];

    CanonicalSession {
        session_id: "operator-role-target".to_string(),
        provider_slug: "codex".to_string(),
        workspace: Some(PathBuf::from("/tmp/project")),
        title: Some("operator role target".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages: vec![
            message(0, MessageRole::User, "TYPED fix the bug"),
            message(1, MessageRole::System, "SYSTEM you are a helpful assistant"),
            message(
                2,
                MessageRole::Other("developer".to_string()),
                "DEVELOPER be terse",
            ),
            message(3, MessageRole::Assistant, "SPOKEN on it"),
            observation,
        ],
        metadata: serde_json::json!({}),
        source_path: PathBuf::from("/tmp/source.jsonl"),
        model_name: Some("gpt-5".to_string()),
    }
}

/// Which marker text landed on which raw tag, read back out of the artifact.
fn tag_by_marker<T: Clone>(pairs: &[(String, T)], marker: &str) -> T {
    pairs
        .iter()
        .find(|(text, _)| text.contains(marker))
        .unwrap_or_else(|| panic!("no record in the written artifact carries {marker:?}"))
        .1
        .clone()
}

/// A Cursor bubble is `MESSAGE_TYPE_HUMAN` or `MESSAGE_TYPE_AI`, and the
/// operator belongs on the human side.
///
/// The claim is not that Cursor has an operator channel — it has none, and
/// `pipeline::folded_role` files that anonymisation as a `Loss`. The claim is
/// that of the two available bubbles, the one that says "the model said this"
/// is a worse account of a system prompt than the one that says "this arrived
/// from outside the model", because only the second is recoverable by a human
/// reading the resumed session.
///
/// A tool observation is deliberately the other way. Every tool bubble the
/// Cursor workbench constructs is AI-typed with the payload beside it —
/// `{...Qb(), codeBlocks: [], type: yo.AI, text: "", capabilityType:
/// Vs.TOOL_FORMER, toolFormerData: r}` — so an AI bubble is where Cursor itself
/// puts an observation, and moving it would be a fresh lie rather than a fix.
#[test]
fn cursor_writes_the_operator_to_a_human_bubble() {
    const HUMAN: i64 = 1;
    const AI: i64 = 2;

    let _lock = ENV.lock().unwrap();
    let cursor_home = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::set("CURSOR_HOME", cursor_home.path());

    let written = Cursor
        .write_session(&operator_session(), &WriteOptions { force: true })
        .expect("cursor write");
    assert_eq!(written.paths.len(), 1, "cursor writes one virtual path");

    let db = cursor_home.path().join("User/globalStorage/state.vscdb");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open the written state.vscdb");
    let bubbles: Vec<(String, i64)> = {
        let mut stmt = conn
            .prepare("SELECT value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%'")
            .expect("prepare bubble query");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query bubbles");
        rows.map(|raw| {
            let bubble: serde_json::Value =
                serde_json::from_str(&raw.expect("bubble row")).expect("bubble value is JSON");
            (
                bubble["text"].as_str().unwrap_or_default().to_string(),
                bubble["type"]
                    .as_i64()
                    .expect("every bubble carries a numeric `type`"),
            )
        })
        .collect()
    };
    assert_eq!(bubbles.len(), 5, "one bubble per canonical message");

    for (text, ty) in &bubbles {
        assert!(
            *ty == HUMAN || *ty == AI,
            "bubble {text:?} was written as type {ty}, which is outside \
             aiserver.v1.ConversationMessage.MessageType. Cursor's turn grouping pushes a bubble \
             only on an explicit `=== yo.HUMAN` or `=== yo.AI`, so a third value is not a neutral \
             channel — it is a turn Cursor never shows."
        );
    }

    assert_eq!(
        tag_by_marker(&bubbles, "SYSTEM"),
        HUMAN,
        "a system prompt was written as an AI bubble: the resumed session is told the model \
         issued its own instructions"
    );
    assert_eq!(
        tag_by_marker(&bubbles, "DEVELOPER"),
        HUMAN,
        "an unrecognised source role was written as an AI bubble: same inversion, and the source's \
         own name for the turn is already gone"
    );
    assert_eq!(tag_by_marker(&bubbles, "TYPED"), HUMAN);
    assert_eq!(tag_by_marker(&bubbles, "SPOKEN"), AI);
    assert_eq!(
        tag_by_marker(&bubbles, "OBSERVATION"),
        AI,
        "a tool observation belongs on the AI bubble — that is the only bubble type Cursor's own \
         `toolFormerData` is ever attached to"
    );
}

/// A Kiro journal line is a `LogEntryV1`, and the operator belongs on `Prompt`.
///
/// `System` already went there. `Other` went to `AssistantMessage`, which is
/// the same words attributed to the agent. There is no third conversational
/// variant to reach for: `Compaction` carries a summary and a snapshot,
/// `ResetTo` carries an index, and a `kind` outside the five is a
/// deserialization error rather than a degraded read.
#[test]
fn kiro_writes_the_operator_to_a_prompt_record() {
    let _lock = ENV.lock().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::set("KIRO_HOME", kiro_home.path());

    let written = Kiro
        .write_session(&operator_session(), &WriteOptions { force: true })
        .expect("kiro write");

    let jsonl = kiro_home
        .path()
        .join("sessions")
        .join("cli")
        .join(format!("{}.jsonl", written.session_id));
    let raw =
        std::fs::read_to_string(&jsonl).unwrap_or_else(|e| panic!("read {}: {e}", jsonl.display()));

    let entries: Vec<(String, String)> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let envelope: serde_json::Value =
                serde_json::from_str(line).expect("every journal line is JSON");
            let kind = envelope["kind"]
                .as_str()
                .expect("every envelope carries a string `kind`")
                .to_string();
            let text = envelope["data"]["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["data"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            (text, kind)
        })
        .collect();
    assert_eq!(entries.len(), 5, "one envelope per canonical message");

    for (text, kind) in &entries {
        assert!(
            LOG_ENTRY_V1_KINDS.contains(&kind.as_str()),
            "envelope {text:?} was written with kind {kind:?}, which is not a LogEntryV1 variant. \
             The enum is adjacently tagged with no unit variant, so kiro-cli rejects the line \
             rather than degrading it."
        );
    }

    assert_eq!(
        tag_by_marker(&entries, "SYSTEM"),
        "Prompt",
        "a system prompt must not be replayed as the agent's own words"
    );
    assert_eq!(
        tag_by_marker(&entries, "DEVELOPER"),
        "Prompt",
        "an unrecognised source role was written as `AssistantMessage`: Kiro replays that as the \
         agent, so the resumed session believes it wrote its own instructions"
    );
    assert_eq!(tag_by_marker(&entries, "TYPED"), "Prompt");
    assert_eq!(tag_by_marker(&entries, "SPOKEN"), "AssistantMessage");
    assert_eq!(
        tag_by_marker(&entries, "OBSERVATION"),
        "ToolResults",
        "Kiro has a real tool variant, so a tool observation is not a fold at all"
    );
}
