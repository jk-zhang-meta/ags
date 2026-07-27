//! Reader for the `@mariozechner/pi-coding-agent` `SessionManager` transcript.
//!
//! This is one upstream file format, written by one upstream library, and read
//! here by more than one provider. `pi` writes it directly; ClawdBot writes it
//! through the same library — `clawdbot`'s `package.json` depends on
//! `@mariozechner/pi-coding-agent`, and `dist/agents/pi-embedded-runner.js`
//! drives the transcript with `SessionManager.open(sessionFile)`. Two providers
//! reading one format with two hand-written parsers is how one of them rots
//! without anyone noticing, so the line loop lives here once.
//!
//! Deliberately *not* folded in: `src/providers/openclaw.rs`. OpenClaw is a
//! hard fork — as of `openclaw@2026.7.1-2` it no longer depends on
//! `@mariozechner/pi-coding-agent` at all, it vendors its own session manager
//! under `@openclaw/ai`, and that fork already emits a `session_info` record
//! type `pi` has never had. One parser answerable to two writers that are
//! actively drifting apart is the abstraction that breaks.
//!
//! ## File format
//!
//! JSONL. Every line is one record, discriminated by `type`. The first line is
//! the header; the rest form a tree via `id`/`parentId`.
//!
//! ```json
//! {"type":"session","version":2,"id":"…","timestamp":"…","cwd":"/home/u/p"}
//! {"type":"message","id":"5c4f098c","parentId":null,"timestamp":"…","message":{"role":"user","content":"…"}}
//! ```
//!
//! The record types are enumerated from the published type definitions in
//! `@mariozechner/pi-coding-agent@0.32.3`,
//! `dist/core/session-manager.d.ts` (`FileEntry = SessionHeader |
//! SessionEntry`), and documented in that package's `docs/session.md`.
//!
//! ## What this reader can and cannot represent
//!
//! [`CanonicalSession`] is message-level. Three record types map onto it
//! (`session`, `message`, `model_change`); the other six carry things the flat
//! track has nowhere to put. Those are not dropped quietly — every record this
//! reader does not represent is tallied by type in
//! [`PiTranscript::unrepresented`], and a type that is not in the published set
//! at all is additionally logged at `warn`, which is the default tracing level.
//! A reader that silently `continue`s on a record it does not recognise is
//! exactly the failure this module was written to fix.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use tracing::warn;

use crate::model::{
    CanonicalMessage, MessageRole, ToolCall, normalize_role, parse_timestamp, reindex_messages,
};

/// Record types carried by a `SessionManager` transcript.
///
/// From `session-manager.d.ts`: `SessionHeader` plus the eight `SessionEntry`
/// variants. Listed so that a record type outside this set is reported as
/// unrecognised rather than merged into the "known but unrepresentable" tally —
/// the two mean very different things to whoever reads the report.
const KNOWN_RECORD_TYPES: [&str; 9] = [
    "session",
    "message",
    "model_change",
    "thinking_level_change",
    "compaction",
    "branch_summary",
    "custom",
    "custom_message",
    "label",
];

/// Everything a `SessionManager` transcript states, before any one provider
/// decides what to call it.
///
/// Provider-specific choices — the slug, the session-id policy, the metadata
/// blob, which of these fields becomes `model_name` — stay with the provider.
/// This struct is only the parse.
#[derive(Debug, Default)]
pub struct PiTranscript {
    /// `id` from the session header, when there is one.
    pub header_id: Option<String>,
    /// `cwd` from the session header — the workspace the session ran in.
    pub cwd: Option<String>,
    /// Latest `provider`, from the header or the last `model_change`.
    pub provider: Option<String>,
    /// Latest `modelId`, from the header or the last `model_change`.
    pub model_id: Option<String>,
    pub messages: Vec<CanonicalMessage>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// Record types that reached this reader and produced no message, counted
    /// by type. Empty means every line in the file was accounted for.
    pub unrepresented: BTreeMap<String, u64>,
}

impl PiTranscript {
    /// The record types this reader could not represent, as a stable string —
    /// `"compaction 2, label 1"` — or `None` when nothing was left over.
    ///
    /// `None` rather than `"none"` or `""`: the caller puts this in session
    /// metadata, and an absent key says "nothing was dropped" without the
    /// reader having to claim it in prose.
    pub fn describe_unrepresented(&self) -> Option<String> {
        if self.unrepresented.is_empty() {
            return None;
        }
        Some(
            self.unrepresented
                .iter()
                .map(|(kind, count)| format!("{kind} {count}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Flatten a pi message `content` into a single string.
///
/// `content` is either a plain string or an array of typed blocks. The block
/// types are those of `@mariozechner/pi-ai`: `text`, `thinking`, `toolCall`,
/// `image`. Note `thinking` carries its prose under `thinking`, not `text` —
/// reading `text` here yields empty reasoning on every session that has any.
fn flatten_content(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|block| {
                let block_type = block.get("type").and_then(|t| t.as_str());
                match block_type {
                    Some("text") => block.get("text").and_then(|t| t.as_str()).map(String::from),
                    Some("thinking") => block
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .map(|t| format!("[Thinking] {t}")),
                    Some("toolCall") => {
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        Some(format!("[Tool: {name}]"))
                    }
                    Some("image") => None,
                    _ => None,
                }
            })
            .collect();
        return parts.join("\n");
    }
    String::new()
}

/// Extract the `toolCall` blocks of a pi message `content` array.
fn extract_tool_calls(content: &serde_json::Value) -> Vec<ToolCall> {
    let Some(arr) = content.as_array() else {
        return vec![];
    };
    arr.iter()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                return None;
            }
            Some(ToolCall {
                id: block.get("id").and_then(|v| v.as_str()).map(String::from),
                name: block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: block
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}

/// Parse a `SessionManager` transcript.
///
/// Unreadable and non-JSON lines are skipped, matching every other JSONL reader
/// here: a truncated tail is the normal state of a session an agent is still
/// writing, not a reason to fail the whole file.
pub fn read(path: &Path, log_provider: &'static str) -> anyhow::Result<PiTranscript> {
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut out = PiTranscript::default();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let record_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match record_type {
            "session" => {
                out.header_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
                out.cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
                if let Some(p) = val.get("provider").and_then(|v| v.as_str()) {
                    out.provider = Some(p.to_string());
                }
                if let Some(m) = val.get("modelId").and_then(|v| v.as_str()) {
                    out.model_id = Some(m.to_string());
                }
                if let Some(ts) = val.get("timestamp").and_then(parse_timestamp) {
                    out.started_at = Some(ts);
                }
            }
            "message" => {
                let Some(msg) = val.get("message") else {
                    // A `message` record with no `message` body is malformed,
                    // not a record type we chose not to represent.
                    *out.unrepresented
                        .entry("message (no body)".to_string())
                        .or_insert(0) += 1;
                    continue;
                };

                // `toolResult` is pi's role for a tool's output. Every other
                // role name is already one `normalize_role` knows.
                let role_str = msg
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let role = normalize_role(match role_str {
                    "toolResult" => "tool",
                    other => other,
                });

                let content_val = msg.get("content");
                let content = content_val.map(flatten_content).unwrap_or_default();
                if content.trim().is_empty() {
                    *out.unrepresented
                        .entry(format!("message ({role_str}, no text)"))
                        .or_insert(0) += 1;
                    continue;
                }

                let tool_calls = content_val.map(extract_tool_calls).unwrap_or_default();

                // The wrapper carries an ISO timestamp; the inner message
                // carries epoch millis. Both are written on every real
                // transcript, so the wrapper is authoritative and the inner one
                // is the fallback for a hand-written or truncated file.
                let ts = val
                    .get("timestamp")
                    .and_then(parse_timestamp)
                    .or_else(|| msg.get("timestamp").and_then(parse_timestamp));

                if out.started_at.is_none() {
                    out.started_at = ts;
                }
                if ts.is_some() {
                    out.ended_at = ts;
                }

                let author = if role == MessageRole::Assistant {
                    msg.get("model")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .or_else(|| out.model_id.clone())
                } else {
                    None
                };

                out.messages.push(CanonicalMessage {
                    idx: 0,
                    role,
                    content,
                    timestamp: ts,
                    author,
                    tool_calls,
                    tool_results: vec![],
                    extra: val,
                });
            }
            "model_change" => {
                if let Some(p) = val.get("provider").and_then(|v| v.as_str()) {
                    out.provider = Some(p.to_string());
                }
                if let Some(m) = val.get("modelId").and_then(|v| v.as_str()) {
                    out.model_id = Some(m.to_string());
                }
            }
            // Every remaining published record type. They are real session
            // content — a compaction is a whole span of history the agent
            // summarised away — but the flat track has no field to hold any of
            // them, so they are counted and reported rather than skipped.
            "thinking_level_change"
            | "compaction"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "label" => {
                *out.unrepresented
                    .entry(record_type.to_string())
                    .or_insert(0) += 1;
            }
            other => {
                // Not in the published set. Either the format grew a record
                // type or this is not a pi transcript at all; both are things
                // the operator has to be able to see.
                debug_assert!(!KNOWN_RECORD_TYPES.contains(&other));
                let label = if other.is_empty() {
                    "(no type field)"
                } else {
                    other
                };
                warn!(
                    provider = log_provider,
                    path = %path.display(),
                    record_type = label,
                    "unrecognised pi session record type; not represented in the session"
                );
                *out.unrepresented
                    .entry(format!("unrecognised:{label}"))
                    .or_insert(0) += 1;
            }
        }
    }

    reindex_messages(&mut out.messages);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn read_lines(lines: &[&str]) -> (PiTranscript, PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let parsed = read(&path, "test").unwrap();
        (parsed, path, tmp)
    }

    #[test]
    fn parses_the_session_manager_envelope() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"session","version":2,"id":"sess-1","timestamp":"2026-02-14T09:12:03.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:12:04.000Z","message":{"role":"user","content":"hi","timestamp":1771060324000}}"#,
            r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"model":"claude-sonnet-4-5"}}"#,
        ]);
        assert_eq!(t.header_id.as_deref(), Some("sess-1"));
        assert_eq!(t.cwd.as_deref(), Some("/w"));
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[0].role, MessageRole::User);
        assert_eq!(t.messages[0].content, "hi");
        assert_eq!(t.messages[1].role, MessageRole::Assistant);
        assert_eq!(t.messages[1].content, "hello");
        assert_eq!(t.messages[1].author.as_deref(), Some("claude-sonnet-4-5"));
        assert!(t.unrepresented.is_empty());
    }

    #[test]
    fn thinking_blocks_read_their_thinking_field_not_text() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"message","timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weighing it up"},{"type":"text","text":"done"}]}}"#,
        ]);
        assert_eq!(t.messages[0].content, "[Thinking] weighing it up\ndone");
    }

    #[test]
    fn tool_calls_are_extracted() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"message","timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"assistant","content":[{"type":"text","text":"running"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"cmd":"ls"}}]}}"#,
        ]);
        assert_eq!(t.messages[0].tool_calls.len(), 1);
        assert_eq!(t.messages[0].tool_calls[0].name, "bash");
        assert_eq!(t.messages[0].tool_calls[0].id.as_deref(), Some("c1"));
    }

    #[test]
    fn tool_result_role_maps_to_tool() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"message","timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"out"}],"isError":false}}"#,
        ]);
        assert_eq!(t.messages[0].role, MessageRole::Tool);
        assert_eq!(t.messages[0].content, "out");
    }

    #[test]
    fn model_change_updates_the_tracked_model() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-02-14T09:12:15.000Z","provider":"openai","modelId":"gpt-5-codex"}"#,
            r#"{"type":"message","timestamp":"2026-02-14T09:12:20.000Z","message":{"role":"assistant","content":"after"}}"#,
        ]);
        assert_eq!(t.model_id.as_deref(), Some("gpt-5-codex"));
        assert_eq!(t.provider.as_deref(), Some("openai"));
        assert_eq!(t.messages[0].author.as_deref(), Some("gpt-5-codex"));
        assert!(t.unrepresented.is_empty(), "model_change is represented");
    }

    #[test]
    fn published_but_unrepresentable_types_are_counted_not_dropped() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"compaction","id":"c1","parentId":null,"timestamp":"2026-02-14T09:00:00.000Z","summary":"s","firstKeptEntryId":"x","tokensBefore":50000}"#,
            r#"{"type":"branch_summary","id":"c2","parentId":"c1","timestamp":"2026-02-14T09:00:01.000Z","fromId":"x","summary":"s"}"#,
            r#"{"type":"label","id":"c3","parentId":"c2","timestamp":"2026-02-14T09:00:02.000Z","targetId":"x","label":"cp"}"#,
            r#"{"type":"custom","id":"c4","parentId":"c3","timestamp":"2026-02-14T09:00:03.000Z","customType":"h","data":{}}"#,
            r#"{"type":"custom_message","id":"c5","parentId":"c4","timestamp":"2026-02-14T09:00:04.000Z","customType":"h","content":"x","display":true}"#,
            r#"{"type":"thinking_level_change","id":"c6","parentId":"c5","timestamp":"2026-02-14T09:00:05.000Z","thinkingLevel":"high"}"#,
        ]);
        assert_eq!(t.messages.len(), 0);
        for kind in [
            "compaction",
            "branch_summary",
            "label",
            "custom",
            "custom_message",
            "thinking_level_change",
        ] {
            assert_eq!(
                t.unrepresented.get(kind),
                Some(&1),
                "{kind} must be counted, not skipped"
            );
        }
        assert_eq!(
            t.describe_unrepresented().as_deref(),
            Some(
                "branch_summary 1, compaction 1, custom 1, custom_message 1, label 1, thinking_level_change 1"
            )
        );
    }

    #[test]
    fn a_record_type_outside_the_published_set_is_flagged_as_unrecognised() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"session_info","id":"x","timestamp":"2026-02-14T09:00:00.000Z"}"#,
            r#"{"no_type_field":true}"#,
        ]);
        assert_eq!(t.unrepresented.get("unrecognised:session_info"), Some(&1));
        assert_eq!(
            t.unrepresented.get("unrecognised:(no type field)"),
            Some(&1)
        );
    }

    #[test]
    fn malformed_lines_are_skipped_without_failing_the_file() {
        let (t, _p, _g) = read_lines(&[
            "",
            "not json",
            r#"{"type":"message","timestamp":"2026-02-14T09:12:08.000Z","message":{"role":"user","content":"survived"}}"#,
            "{truncated",
        ]);
        assert_eq!(t.messages.len(), 1);
        assert_eq!(t.messages[0].content, "survived");
    }

    #[test]
    fn describe_unrepresented_is_none_when_everything_was_accounted_for() {
        let (t, _p, _g) = read_lines(&[
            r#"{"type":"session","id":"s","timestamp":"2026-02-14T09:12:03.000Z","cwd":"/w"}"#,
        ]);
        assert_eq!(t.describe_unrepresented(), None);
    }
}
