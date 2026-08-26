//! How Cursor and Kiro handle a turn they cannot represent (defect #74).
//!
//! Cursor has no safe import path: creating only its local conversation rows
//! leaves the composer absent from the UI index, so its test asserts refusal.
//! Kiro has exactly two conversational speakers, so a system prompt and an
//! unrecognised source role have to be folded onto one of them.
//! `pipeline::folded_role` declares that Kiro fold and
//! `conformance_test::no_writer_folds_a_role_without_declaring_it` keeps it
//! honest. Until #74 Kiro chose the agent — telling a resumed session that the
//! model itself issued the operator's instruction.
//!
//! # Why this reads the file and not the session
//!
//! casr's own reader is not an oracle for casr's own writer. `parse_envelope`
//! maps `"AssistantMessage"` to `MessageRole::Assistant`, so a writer that
//! picked the wrong speaker and a reader that agrees with it produce a perfectly
//! clean round trip. The Kiro test therefore opens the JSONL line and asserts
//! on the raw `kind` that the vendor's own code will read.
//!
//! # The vendor vocabularies these numbers come from
//!
//! * `kiro-cli-chat` 2.14.2: `adjacently tagged enum LogEntryV1` with
//!   `struct variant LogEntryV1::{Prompt, AssistantMessage, ToolResults,
//!   Compaction, ResetTo}`. No system member, and an adjacently tagged enum
//!   with no unit variant rejects a sixth `kind` outright.

mod test_env;

use std::path::{Path, PathBuf};

use ags::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolResult};
use ags::providers::cursor::Cursor;
use ags::providers::kiro::Kiro;
use ags::providers::{Provider, WriteOptions};

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

/// Cursor rejects an operator-bearing import until it can safely update the
/// workbench's composer index; a locally readable `cursorDiskKV` row is not a
/// session the Cursor UI can show.
#[test]
fn cursor_refuses_an_operator_session_without_creating_state() {
    let _lock = ENV.lock().unwrap();
    let cursor_home = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::set("CURSOR_HOME", cursor_home.path());

    let error = Cursor
        .write_session(&operator_session(), &WriteOptions { force: true })
        .expect_err("Cursor target must refuse");
    assert_eq!(
        error.to_string(),
        Cursor.write_refusal().unwrap(),
        "direct writer and pipeline capability must agree"
    );
    assert!(
        !cursor_home.path().join("User").exists(),
        "Cursor refusal created target-store state"
    );
}

/// A Kiro journal line is a `LogEntryV1`, and the operator belongs on `Prompt`.
///
/// `System` already went there. `Other` went to `AssistantMessage`, which is
/// the same words attributed to the agent. There is no third conversational
/// variant to reach for: `Compaction` carries a summary and a snapshot,
/// `ResetTo` carries an index, and a `kind` outside the five is a
/// deserialization error rather than a degraded read. A Tool turn with both
/// prose and a result needs two entries because Kiro reads prose only from
/// `AssistantMessage` and results only from `ToolResults.data.results`.
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
    assert_eq!(
        entries.len(),
        6,
        "the Tool turn needs separate model-visible text and result envelopes"
    );

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
        "AssistantMessage",
        "ToolResults ignores ordinary text, so the visible commentary must use Kiro's assistant \
         channel; the pipeline declares that speaker change as a loss"
    );

    let result_envelope: serde_json::Value = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .find(|envelope: &serde_json::Value| envelope["kind"] == "ToolResults")
        .expect("the structural observation needs a ToolResults envelope");
    assert_eq!(
        result_envelope["data"]["results"]["c1"]["result"]["Success"]["items"][0]["Text"],
        "matched 3 lines"
    );
}
