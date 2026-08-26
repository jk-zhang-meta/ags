//! Two Claude Code shapes that only a real transcript taught us.
//!
//! # Why a synthesised fixture and not the transcript
//!
//! Both defects here were found against one private local session
//! (`90b138a7`, 2,160 lines). The file is the user's conversation and must
//! never enter the repository, so what is committed is its *shape*: the record
//! ordering, the parent links, and the seven record types that carry no `uuid`
//! and no `parentUuid` at all. The content is invented; the topology is not.
//!
//! # The topology that matters
//!
//! Claude Code's own loader (`@anthropic-ai/claude-code`, the Bun-compiled
//! `claude` binary, v2.1.220) builds its resume transcript from exactly four
//! record types — `user`, `assistant`, `attachment`, `system` — and reconstructs
//! the conversation by walking `parentUuid` back from one leaf. Every other
//! record type is session state it reads for other purposes and never threads.
//! Two consequences are pinned below:
//!
//! - The leaf comes from `last-prompt.leafUuid`, not from the end of the file.
//!   The last line of a live transcript is routinely a `file-history-snapshot`
//!   with neither `uuid` nor `parentUuid`; a reader that took "the last record"
//!   would start its walk on a record that is in no graph.
//! - After a compaction, the summary the compaction wrote hangs off the
//!   boundary marker. A message the user submitted while the compaction was
//!   still running is written with its *pre-compaction* parent, so the live
//!   branch can run back into the discarded history and pass the summary by.
//!   Claude Code survives that by rewriting the graph on load; casr survives it
//!   because `replay::resolve` keeps the boundary's own children.

mod test_env;

use std::io::Write;
use std::path::PathBuf;

use ags::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolResult};
use ags::providers::claude_code::ClaudeCode;
use ags::providers::{Provider, WriteOptions, claude_code_ir};
use ags::replay::resolve;

static CC_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that sets an env var and restores the original value on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: the caller holds `CC_ENV` for the whole scope, which is the
        // same lock every other env-touching test in this binary takes.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: same lock still held.
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// #48 — a message carrying both text and tool results
// ---------------------------------------------------------------------------

/// A non-assistant message with **both** `content` and `tool_results` survives.
///
/// The writer used to emit the results *instead of* the text, and the reader
/// extracts text only from `text` blocks — so the text was deleted on the way
/// out and its absence confirmed on the way back in. Writer and reader agreed,
/// the round-trip compared clean, and 64 bytes of user-visible message went
/// missing. Nothing populated both fields at once until an openclaw conversion
/// did, which is the only reason this was ever observed.
///
/// Both directions are asserted here, against the real `write_session` /
/// `read_session` pair rather than the block builder alone, because a
/// block-shape assertion is exactly the kind of check that passes while the
/// round-trip loses data.
#[test]
fn a_message_with_both_text_and_tool_results_survives_the_round_trip() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let session = CanonicalSession {
        session_id: "both-fields".to_string(),
        provider_slug: "test-source".to_string(),
        workspace: Some(PathBuf::from("/data/projects/myapp")),
        title: Some("tool result with commentary".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        messages: vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::Tool,
            content: "the file was already up to date".to_string(),
            timestamp: Some(1_700_000_000_000),
            author: None,
            tool_calls: Vec::new(),
            tool_results: vec![ToolResult {
                call_id: Some("call-42".to_string()),
                content: "0 replacements made".to_string(),
                is_error: false,
            }],
            extra: serde_json::Value::Null,
        }],
        model_name: None,
        metadata: serde_json::Value::Null,
        source_path: PathBuf::from("/dev/null"),
    };

    let written = ClaudeCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("write_session");
    let readback = ClaudeCode
        .read_session(&written.paths[0])
        .expect("read_session");

    assert_eq!(readback.messages.len(), 1);
    let got = &readback.messages[0];
    assert_eq!(
        got.content, "the file was already up to date",
        "the message's own text must not be dropped just because it also \
         carries a tool result"
    );
    assert_eq!(got.tool_results.len(), 1);
    assert_eq!(got.tool_results[0].content, "0 replacements made");
    assert_eq!(got.tool_results[0].call_id.as_deref(), Some("call-42"));

    // Block order is Claude Code's, not ours: 11 user records across the local
    // corpus are `(tool_result, text)` and none are `(text, tool_result)`, and
    // the Anthropic API requires tool results to lead the block list.
    let line = std::fs::read_to_string(&written.paths[0]).unwrap();
    let entry: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    let blocks = entry
        .pointer("/message/content")
        .and_then(|c| c.as_array())
        .expect("content should be a block array");
    assert_eq!(blocks[0]["type"], "tool_result");
    assert_eq!(blocks[1]["type"], "text");
}

// ---------------------------------------------------------------------------
// The `90b138a7` shape
// ---------------------------------------------------------------------------

/// Build the transcript. Ids are readable rather than uuid-shaped on purpose:
/// nothing in the reader parses them, and a failure message naming `inflight`
/// is worth more than one naming `53b608db`.
fn shape_transcript() -> String {
    let mut lines: Vec<serde_json::Value> = Vec::new();
    let mut ts = 0;
    let mut stamp = || {
        ts += 1;
        format!("2026-07-27T00:00:{ts:02}.000Z")
    };

    // Every record in a real transcript carries `sessionId`, including the ones
    // that carry no `uuid`.
    macro_rules! msg {
        ($ty:expr, $uuid:expr, $parent:expr, $text:expr) => {{
            let mut v = serde_json::json!({
                "type": $ty,
                "sessionId": "shape-90b138a7",
                "uuid": $uuid,
                "parentUuid": $parent,
                "timestamp": stamp(),
                "message": { "role": $ty, "content": $text },
            });
            // Claude always writes assistant content as a block array and its
            // full API envelope; only `user` content is ever a bare string.
            if $ty == "assistant" {
                v["message"]["model"] = serde_json::json!("claude-opus-4");
                v["message"]["content"] =
                    serde_json::json!([{ "type": "text", "text": $text }]);
                v["message"]["id"] = serde_json::json!(concat!("msg_", $uuid));
                v["message"]["type"] = serde_json::json!("message");
            }
            lines.push(v);
        }};
    }

    // The seven types that never carry `uuid` or `parentUuid`. Claude rewrites
    // them on every submit and keeps every copy, which is why there are more of
    // them than there are turns.
    macro_rules! chrome {
        ($ty:expr) => {
            lines.push(serde_json::json!({ "type": $ty, "sessionId": "shape-90b138a7" }))
        };
    }
    macro_rules! last_prompt {
        ($leaf:expr) => {
            lines.push(serde_json::json!({
                "type": "last-prompt",
                "sessionId": "shape-90b138a7",
                "leafUuid": $leaf,
                "lastPrompt": "…",
            }))
        };
    }

    msg!("user", "u-root", serde_json::Value::Null, "let's begin");
    msg!("assistant", "a-1", "u-root", "on it");
    chrome!("mode");
    chrome!("permission-mode");
    last_prompt!("u-root");
    chrome!("ai-title");
    chrome!("queue-operation");

    msg!("user", "u-2", "a-1", "next");
    msg!("assistant", "a-2", "u-2", "done");
    lines.push(serde_json::json!({
        "type": "attachment",
        "sessionId": "shape-90b138a7",
        "uuid": "att-1",
        "parentUuid": "a-2",
        "timestamp": stamp(),
        "attachment": { "type": "queued_command" },
    }));
    chrome!("file-history-delta");
    lines.push(serde_json::json!({
        "type": "system",
        "subtype": "turn_duration",
        "sessionId": "shape-90b138a7",
        "uuid": "sys-1",
        "parentUuid": "att-1",
        "timestamp": stamp(),
    }));

    // First boundary. `parentUuid` is null and the real link is
    // `logicalParentUuid`; the preserved set is the *tail* Claude kept, not the
    // post-compaction context, and the summary arrives on the next line.
    lines.push(serde_json::json!({
        "type": "system",
        "subtype": "compact_boundary",
        "sessionId": "shape-90b138a7",
        "uuid": "cb-1",
        "parentUuid": serde_json::Value::Null,
        "logicalParentUuid": "sys-1",
        "timestamp": stamp(),
        "compactMetadata": {
            "trigger": "auto",
            "preservedMessages": {
                "anchorUuid": "sum-1",
                "uuids": ["a-2"],
                "allUuids": ["a-2", "sys-1"],
            },
        },
    }));
    let mut summary_one = serde_json::json!({
        "type": "user",
        "sessionId": "shape-90b138a7",
        "uuid": "sum-1",
        "parentUuid": "cb-1",
        "timestamp": stamp(),
        "isCompactSummary": true,
        "message": { "role": "user", "content": "SUMMARY ONE" },
    });
    summary_one["isSidechain"] = serde_json::json!(false);
    lines.push(summary_one);

    msg!("user", "u-3", "sum-1", "carry on");
    msg!("assistant", "a-3", "u-3", "sure");
    last_prompt!("u-3");
    chrome!("mode");
    lines.push(serde_json::json!({
        "type": "system",
        "subtype": "turn_duration",
        "sessionId": "shape-90b138a7",
        "uuid": "sys-2",
        "parentUuid": "a-3",
        "timestamp": stamp(),
    }));
    chrome!("file-history-snapshot");

    // Second boundary: `u-3` is *not* preserved, so anything still pointing at
    // it is pointing at history this boundary discarded.
    lines.push(serde_json::json!({
        "type": "system",
        "subtype": "compact_boundary",
        "sessionId": "shape-90b138a7",
        "uuid": "cb-2",
        "parentUuid": serde_json::Value::Null,
        "logicalParentUuid": "sys-2",
        "timestamp": stamp(),
        "compactMetadata": {
            "trigger": "auto",
            "preservedMessages": {
                "anchorUuid": "sum-2",
                "uuids": ["a-3"],
                "allUuids": ["a-3", "sys-2"],
            },
        },
    }));
    // Submitted while the compaction was running: written *before* the summary
    // and parented on `u-3`, which the boundary above just discarded.
    msg!("user", "inflight", "u-3", "actually, one more thing");
    let mut summary_two = serde_json::json!({
        "type": "user",
        "sessionId": "shape-90b138a7",
        "uuid": "sum-2",
        "parentUuid": "cb-2",
        "timestamp": stamp(),
        "isCompactSummary": true,
        "message": { "role": "user", "content": "SUMMARY TWO" },
    });
    summary_two["isSidechain"] = serde_json::json!(false);
    lines.push(summary_two);
    msg!("assistant", "a-4", "inflight", "of course");
    last_prompt!("inflight");
    chrome!("permission-mode");
    chrome!("ai-title");
    chrome!("queue-operation");
    // THE LAST RECORD IN THE FILE, exactly as the real transcript ends: no
    // `uuid`, no `parentUuid`, in no graph.
    chrome!("file-history-snapshot");

    let mut out = String::new();
    for line in lines {
        out.push_str(&serde_json::to_string(&line).unwrap());
        out.push('\n');
    }
    out
}

fn write_shape() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("shape-90b138a7.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(shape_transcript().as_bytes()).unwrap();
    (dir, path)
}

/// The live head comes from Claude's graph state, not the last file record.
///
/// Seven record types in this transcript — `mode`, `permission-mode`,
/// `last-prompt`, `ai-title`, `queue-operation`, `file-history-delta`,
/// `file-history-snapshot` — carry neither `uuid` nor `parentUuid`, and the
/// file *ends* on one of them. Claude Code's loader ignores all seven when it
/// builds the graph (its transcript predicate admits only `user`, `assistant`,
/// `attachment` and `system`). The recorded leaf is a non-explicit hint, so the
/// loader advances from `inflight` to its newest main-chain descendant `a-4`.
/// A reader that instead took the end of the file would begin its walk on a
/// record with no parent and no ancestors and resolve the session to nothing.
#[test]
fn the_live_head_comes_from_vendor_graph_not_the_last_file_record() {
    let (_dir, path) = write_shape();
    let ir = claude_code_ir::read(&path).unwrap();

    assert_eq!(
        ir.live_head.as_deref(),
        Some("a-4"),
        "a non-explicit leaf advances to its newest main-chain descendant"
    );

    // The seven are captured — this reader reports rather than discards — but
    // as session state, never as conversation and never in the graph.
    for kind in [
        "mode",
        "permission-mode",
        "last-prompt",
        "ai-title",
        "queue-operation",
        "file-history-delta",
        "file-history-snapshot",
    ] {
        let events: Vec<_> = ir
            .events
            .iter()
            .filter(|event| match &event.body {
                ags::ir::Body::Control { control_kind, .. } => control_kind == kind,
                _ => false,
            })
            .collect();
        assert!(!events.is_empty(), "{kind} should be captured as a control");
        for event in events {
            assert_eq!(
                event.visibility,
                ags::ir::Visibility::Ui,
                "{kind} is session state, not model context"
            );
            assert_eq!(event.parent, None, "{kind} carries no parent link");
        }
    }
}

/// The compaction's summary survives a live branch that bypasses it.
///
/// This is the whole `90b138a7` defect in one assertion. `inflight` was
/// submitted mid-compaction and is parented on `u-3`, which the boundary
/// discarded, so the walk from the leaf runs back into superseded history and
/// never meets `sum-2`. Before the fix `sum-2` was reported as `AbandonedFork`
/// and the replay handed the target a conversation tail with no statement of
/// what the compaction had removed — on the real transcript, 4 events resolved
/// out of 1,267 captured, and the one record standing in for the other 1,263
/// was the one deleted.
#[test]
fn the_compaction_summary_survives_a_bypassing_leaf() {
    let (_dir, path) = write_shape();
    let ir = claude_code_ir::read(&path).unwrap();
    let plan = resolve(&ir);

    assert_eq!(plan.checkpoints, ["cb-1", "cb-2"]);
    assert_eq!(
        plan.events,
        ["sum-2", "a-3", "inflight", "a-4"],
        "`sum-2` is the boundary's anchor before its preserved tail `a-3`; \
         `inflight` and its reply are the live branch"
    );
    assert!(
        !plan.excluded.iter().any(|excluded| excluded.id == "sum-2"),
        "the compaction summary is not an abandoned fork"
    );
}

/// The pre-compaction history is superseded, not pruned.
///
/// Stated separately so that a regression cannot pass the assertion above by
/// widening the keep set until nothing is dropped at all: everything the
/// boundary discarded must still be reported as `Superseded`, under the
/// boundary that discarded it.
#[test]
fn the_discarded_history_is_still_reported_as_superseded() {
    let (_dir, path) = write_shape();
    let ir = claude_code_ir::read(&path).unwrap();
    let plan = resolve(&ir);

    let superseded: Vec<&str> = plan
        .excluded
        .iter()
        .filter_map(|excluded| match &excluded.reason {
            ags::replay::ExclusionReason::Superseded { .. } => Some(excluded.id.as_str()),
            _ => None,
        })
        .collect();
    assert!(superseded.contains(&"u-root"));
    assert!(superseded.contains(&"u-3"));
    assert!(
        !superseded.contains(&"sum-2"),
        "a summary written after the boundary cannot be superseded by it"
    );
}
