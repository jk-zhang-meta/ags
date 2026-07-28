//! OpenClaw provider — reads and resumes `@openclaw/ai` session transcripts.
//!
//! Session files: `~/.openclaw/agents/<agent-id>/sessions/*.jsonl`
//! Override root: `OPENCLAW_STATE_DIR`, else `$OPENCLAW_HOME/.openclaw`
//!
//! # Why this is not [`crate::providers::pi_session`]
//!
//! ClawdBot and `pi` share a reader because they share a writer: both drive
//! `@mariozechner/pi-coding-agent`'s `SessionManager`. OpenClaw did too, once.
//! It does not now, and the two formats have diverged far enough that one
//! parser answerable to both would be wrong for each.
//!
//! Established from the released package — `npm pack openclaw@2026.7.1-2`,
//! whose `package.json` depends on `@openclaw/ai@2026.7.1-2` and on no version
//! of `@mariozechner/pi-coding-agent` at all. Against `pi@0.32.3`, that fork:
//!
//! - stamps `version: 3` (`CURRENT_SESSION_VERSION` in
//!   `dist/session-manager-RXl7XED7.d.ts`); pi stamps 2;
//! - adds a tenth record type, `session_info`, and an eleventh, `leaf`, which
//!   is a *navigation control* rather than conversation (`transcript-tree.ts`:
//!   "Leaf rows are navigation controls: they select targetId as the active
//!   leaf");
//! - renames a member of the message union. Both `Message`s are
//!   user/assistant/toolResult (`dist/types-CFIUY_La.d.ts:232`), and both
//!   `AgentMessage`s widen it the same way (`dist/types-D0CdrmU4.d.ts:343`) —
//!   `bashExecution` is already `pi`'s (`dist/core/messages.d.ts:16-27` in
//!   `pi@0.32.3`), so the fork's change is that `pi`'s `hookMessage` is
//!   OpenClaw's `custom`. A reader written against `pi`'s role names finds no
//!   `hookMessage` here and drops every one of them.
//!
//!   Of the two, only `bashExecution` has no `content`: its model-visible text
//!   has to be built by `bashExecutionToText`, and a reader that reaches for
//!   `content` drops a shell transcript the model was shown. `custom` does
//!   declare `content: string | (TextContent | ImageContent)[]`
//!   (`dist/types-D0CdrmU4.d.ts:294`) — as the table below already says, and as
//!   the parser has always read it.
//!
//! So the parse stays here. What is shared with pi is only the shape of the
//! answer — [`Transcript::unrepresented`] follows
//! [`crate::providers::pi_session::PiTranscript::unrepresented`], and the
//! reported string lands in `metadata.unrepresented` exactly as `clawdbot.rs`
//! does it.
//!
//! # The file is a tree, and reading it as a list is a correctness bug
//!
//! Entries carry `id`/`parentId`. The model's context is **not** the file: it
//! is the path from the live leaf to the root, which is what
//! `SessionManager.buildSessionContext()` builds and what
//! `packages/agent-core/src/harness/session/session.ts` computes from it.
//!
//! `SessionManager.branch(id)` moves the leaf backwards; the next append
//! becomes a *sibling* of whatever came after. "Existing entries are not
//! modified or deleted", so the abandoned path stays in the file forever.
//! Concatenating message lines therefore replays turns the user rewound —
//! content that, from OpenClaw's point of view, no longer exists. Measured on
//! a transcript written by OpenClaw's own `SessionManager`: 16 entries in the
//! file, 14 on the live branch, and the 2 off it were a rejected rewrite.
//!
//! This is the same defect `gemini.rs` fixes with `fold_jsonl`, and it is
//! solved the same way and for the same reason: [`crate::replay`] folds a
//! [`crate::ir::SessionIr`] of typed `Body` events, OpenClaw has no structured
//! reader to produce one, and building an IR reader is a far larger change
//! than reading the format. The walk stays private to this parser.
//!
//! Compaction narrows the context further: a `compaction` entry on the path
//! replaces everything before `firstKeptEntryId` with its own summary. Both
//! effects are mirrored here from `buildSessionContext`, and what either
//! removes is counted rather than dropped quietly.
//!
//! # What carries model-visible content
//!
//! From `convertToLlm` (`dist/proxy-BzhBz8iM.js`), which is the last thing to
//! touch a message before it becomes a provider request:
//!
//! | Record | Becomes | Carried in |
//! |---|---|---|
//! | `message` / `user` | user turn | `content` (string or blocks) |
//! | `message` / `assistant` | assistant turn | `content` blocks |
//! | `message` / `toolResult` | tool turn | `content` blocks, `toolCallId`, `isError` |
//! | `message` / `bashExecution` | **user** turn | `command` + `output` + `exitCode`, rendered by `bashExecutionToText` |
//! | `message` / `custom` | **user** turn | `content` (string or blocks) |
//! | `custom_message` | **user** turn | entry-level `content` |
//! | `branch_summary` | **user** turn | `summary`, wrapped |
//! | `compaction` | **user** turn | `summary`, wrapped |
//!
//! `bashExecution` and `custom` map onto [`MessageRole::User`] because that is
//! precisely what OpenClaw hands the model; preserving the wire role would
//! describe the file more literally and the conversation less truthfully.
//!
//! Carrying no model-visible content, and so represented as nothing:
//! `custom` entries ("Does NOT participate in LLM context"), `label`,
//! `thinking_level_change`, `leaf`. `model_change` becomes `model_name` and
//! `session_info` becomes the session's native name, so neither is a loss.
//!
//! Content blocks are `text` (`.text`), `thinking` (**`.thinking`**, not
//! `.text`), `toolCall`, and `image`.
//!
//! A `toolCall` block is represented exactly once, and which channel carries it
//! depends on the record. On an assistant turn it is structural — it comes back
//! in [`CanonicalMessage::tool_calls`], because that is the record whose writer
//! emits a real `toolCall` block. Everywhere else it has no structural channel
//! and is rendered into the text as `[Tool: <name>]`. See [`ToolCallText`]:
//! doing both on the assistant turn duplicates the call in every conversion
//! sourced from a native OpenClaw transcript.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use tracing::{debug, info, trace, warn};

use crate::discovery::DetectionResult;
use crate::launch::LaunchSpec;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, normalize_role,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, UnreadableSource, WriteOptions, WrittenSession, read_dir_reporting,
    walk_entry_reporting,
};

const OPENCLAW_WRITE_REFUSAL: &str = "OpenClaw is read/resume-only: its session index has no \
cross-process file lock, so direct transcript and sessions.json writes can lose active gateway \
updates. A safe import must use OpenClaw's authenticated gateway lifecycle; use OpenClaw as a \
conversion source, not a target.";

/// OpenClaw's default agent id. Native sessions are keyed by agent, and an
/// agent id is mandatory when targeting the TUI.
const DEFAULT_AGENT_ID: &str = "main";

/// `CURRENT_SESSION_VERSION` from `@openclaw/ai@2026.7.1-2`. Written as a
/// number, because OpenClaw compares it numerically: `migrateToCurrentVersion`
/// reads `header.version ?? 1` and returns early on `>= 3`. A string version
/// fails that comparison, and OpenClaw then reports the file as migrated and
/// rewrites it.
#[cfg(test)]
const SESSION_VERSION: u64 = 3;

/// The record types `isCanonicalSessionEntryType` accepts
/// (`src/config/sessions/transcript-tree.ts`). `session` is the header and
/// `leaf` is a navigation control, so neither appears here.
const CANONICAL_RECORD_TYPES: [&str; 9] = [
    "message",
    "thinking_level_change",
    "model_change",
    "compaction",
    "branch_summary",
    "custom",
    "custom_message",
    "label",
    "session_info",
];

/// The roles `appendMessage` can persist, from `AgentMessage`.
/// `branchSummary` and `compactionSummary` are deliberately absent: OpenClaw
/// refuses them through `appendMessage` and stores them as their own entry
/// types instead.
const PERSISTED_MESSAGE_ROLES: [&str; 5] =
    ["user", "assistant", "toolResult", "bashExecution", "custom"];

// The wrappers `convertToLlm` puts around a summary before the model sees it.
const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// OpenClaw provider implementation.
pub struct OpenClaw;

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

/// One transcript line, with its tree links resolved.
struct Entry {
    kind: String,
    id: String,
    /// Resolved parent; `None` is a root. For a `leaf` control this is its
    /// `targetId`, which is how `parseSessionTranscriptTreeEntry` links it.
    parent: Option<String>,
    /// `type: "leaf"` — selects the active leaf rather than carrying content.
    leaf_control: bool,
    /// `appendMode: "side"` — parked beside the live branch on purpose, rather
    /// than left behind by a rewind. See [`read_transcript`].
    side_append: bool,
    value: serde_json::Value,
}

/// Everything a transcript states, before this provider names any of it.
#[derive(Default)]
struct Transcript {
    header_id: Option<String>,
    cwd: Option<String>,
    /// Latest `session_info.name` — the user's display name for the session.
    session_name: Option<String>,
    model_id: Option<String>,
    messages: Vec<CanonicalMessage>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    /// Everything that reached this reader and produced no message, by cause.
    /// Empty means every line was accounted for; it never means the reader did
    /// not look.
    unrepresented: BTreeMap<String, u64>,
}

impl Transcript {
    fn count(&mut self, key: impl Into<String>) {
        *self.unrepresented.entry(key.into()).or_insert(0) += 1;
    }

    /// A stable `"abandoned 2, label 1"`, or `None` when nothing was left over.
    ///
    /// `None` rather than `""`: the caller puts this in metadata, and an absent
    /// key says "nothing was dropped" without claiming it in prose.
    fn describe_unrepresented(&self) -> Option<String> {
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

/// Whether a `toolCall` block is this path's only record of the call.
///
/// Only the assistant arm of [`entry_message`] also calls
/// [`extract_tool_calls`], so only there is the call already represented — and
/// representing it twice is not merely redundant. A native assistant turn is a
/// `text` block plus a `toolCall` block, exactly what
/// `AssistantMessage.content` declares
/// (`(TextContent | ThinkingContent | ToolCall)[]`,
/// `dist/types-CFIUY_La.d.ts:206`); a reader that also renders the block into
/// the text invents another `[Tool: …]` in canonical content.
///
/// Measured on `openclaw@2026.7.1-2` over one native turn:
/// turn: `readTranscriptFileState` accepts both message rows and `convertToLlm`
/// yields one assistant message holding one `text` part and one `toolCall`
/// part. The vendor reads the call once out of that file, so the file is right
/// and the second copy was the reader's.
#[derive(Clone, Copy, PartialEq)]
enum ToolCallText {
    /// Render as `[Tool: <name>]` — nothing else on this path records it.
    Rendered,
    /// Skip: the caller returns the call in [`CanonicalMessage::tool_calls`].
    Structural,
}

/// Flatten an OpenClaw `content` into a single string.
///
/// `content` is a plain string or an array of typed blocks. Note `thinking`
/// carries its prose under `thinking`, not `text` — reading `text` here yields
/// empty reasoning on every session that has any.
fn flatten_content(
    content: &serde_json::Value,
    out: &mut Transcript,
    tool_calls: ToolCallText,
) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in arr {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
            "thinking" => {
                if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                    parts.push(format!("[Thinking] {t}"));
                }
            }
            "toolCall" if tool_calls == ToolCallText::Rendered => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                parts.push(format!("[Tool: {name}]"));
            }
            // Already returned structurally by the caller. Not counted as
            // unrepresented, because it is represented.
            "toolCall" => {}
            // Base64 image bytes. A string-typed message cannot hold them, so
            // they are counted rather than silently discarded.
            "image" => out.count("content block (image)"),
            other => out.count(format!("content block (unrecognised:{other})")),
        }
    }
    parts.join("\n")
}

/// Extract the `toolCall` blocks of a content array.
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

/// Render a shell execution record the way OpenClaw renders it for the model.
///
/// Mirrors `bashExecutionToText`. This record type has no `content` field at
/// all; its model-visible text exists only once something builds it.
fn bash_execution_to_text(msg: &serde_json::Value) -> String {
    let command = msg.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let output = msg.get("output").and_then(|v| v.as_str()).unwrap_or("");
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    if msg.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = msg.get("exitCode").and_then(serde_json::Value::as_i64)
        && code != 0
    {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if msg.get("truncated").and_then(serde_json::Value::as_bool) == Some(true)
        && let Some(path) = msg.get("fullOutputPath").and_then(|v| v.as_str())
    {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    text
}

/// Read every line, resolving each record's place in the tree.
///
/// Records with no `parentId` field are linked to the running append cursor.
/// That is OpenClaw's own compatibility rule for rows written by older
/// appenders (`parseParentlessCanonicalEntry`: "Treat those rows as a linear
/// continuation of the current append cursor"), and it is also what keeps
/// transcripts casr itself wrote before this change readable.
fn scan(path: &Path, out: &mut Transcript) -> anyhow::Result<(Vec<Entry>, Option<String>)> {
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut entries: Vec<Entry> = Vec::new();
    let mut leaf: Option<String> = None;
    let mut append_cursor: Option<String> = None;

    for line_result in reader.lines() {
        let Ok(line) = line_result else { continue };
        if line.trim().is_empty() {
            continue;
        }
        // A truncated tail is the normal state of a session an agent is still
        // writing, so a bad line does not fail the file — but it is counted,
        // because "the file ended mid-write" and "this is not a transcript"
        // look identical from a silent skip.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            out.count("unparseable line");
            continue;
        };

        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // `SessionHeader` is exactly `{type, version?, id, timestamp, cwd,
        // parentSession?}`. It carries no `modelId`: this reader used to look
        // for one, inherited from `pi_session`, whose header does have
        // `provider`/`modelId`. Nothing writes it here, so the model comes from
        // `model_change` alone — see the state markers in `read_transcript`.
        if kind == "session" {
            out.header_id = value.get("id").and_then(|v| v.as_str()).map(String::from);
            out.cwd = value.get("cwd").and_then(|v| v.as_str()).map(String::from);
            if let Some(ts) = value.get("timestamp").and_then(parse_timestamp) {
                out.started_at = Some(ts);
            }
            continue;
        }

        let Some(id) = value.get("id").and_then(|v| v.as_str()).map(String::from) else {
            let label = if kind.is_empty() {
                "(no type field)"
            } else {
                kind
            };
            out.count(format!("record without id ({label})"));
            continue;
        };

        let is_leaf = kind == "leaf";
        let canonical = CANONICAL_RECORD_TYPES.contains(&kind);
        if !is_leaf && !canonical {
            // Either the format grew a record type or this is not an OpenClaw
            // transcript. Both are things an operator has to be able to see.
            let label = if kind.is_empty() {
                "(no type field)"
            } else {
                kind
            };
            warn!(
                provider = "openclaw",
                path = %path.display(),
                record_type = label,
                "unrecognised OpenClaw session record type; not represented in the session"
            );
            out.count(format!("unrecognised:{label}"));
            continue;
        }

        let side_append = value.get("appendMode").and_then(|v| v.as_str()) == Some("side");

        let parent = if is_leaf {
            value
                .get("targetId")
                .and_then(|v| v.as_str())
                .map(String::from)
        } else if let Some(raw) = value.get("parentId") {
            raw.as_str().map(String::from)
        } else {
            append_cursor.clone()
        };

        // Leaf tracking, from `scanSessionTranscriptTree`: a leaf control
        // selects its target; any other canonical row becomes the leaf unless
        // it was appended to the side.
        if is_leaf {
            leaf = parent.clone();
            append_cursor = value
                .get("appendParentId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| parent.clone());
        } else if !side_append {
            leaf = Some(id.clone());
            append_cursor = Some(id.clone());
        }

        entries.push(Entry {
            kind: kind.to_string(),
            id,
            parent,
            leaf_control: is_leaf,
            side_append,
            value,
        });
    }

    Ok((entries, leaf))
}

/// The path from the live leaf back to the root, in conversation order.
///
/// `leaf` controls are navigation, not content: the walk resolves *through*
/// them to their target, matching `resolveCanonicalParentId`.
fn live_branch<'a>(entries: &'a [Entry], leaf: Option<&str>) -> Vec<&'a Entry> {
    let by_id: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut path: Vec<&Entry> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut current = leaf;

    while let Some(id) = current {
        // A parent cycle is corruption, not a shape to follow forever.
        if !seen.insert(id) {
            break;
        }
        let Some(entry) = by_id.get(id) else { break };
        if !entry.leaf_control {
            path.push(entry);
        }
        current = entry.parent.as_deref();
    }

    path.reverse();
    path
}

/// Turn one on-branch entry into the message OpenClaw would show the model.
fn entry_message(entry: &Entry, out: &mut Transcript) -> Option<CanonicalMessage> {
    // The wrapper carries an ISO timestamp; the inner message carries epoch
    // millis. Both are written on every real transcript, so the wrapper is
    // authoritative and the inner one is the fallback for a truncated file.
    let inner_ts = entry
        .value
        .get("message")
        .and_then(|m| m.get("timestamp"))
        .and_then(parse_timestamp);
    let timestamp = entry
        .value
        .get("timestamp")
        .and_then(parse_timestamp)
        .or(inner_ts);

    let build = |role: MessageRole, content: String, author: Option<String>| {
        (!content.trim().is_empty()).then(|| CanonicalMessage {
            idx: 0,
            role,
            content,
            timestamp,
            author,
            tool_calls: vec![],
            tool_results: vec![],
            extra: entry.value.clone(),
        })
    };

    match entry.kind.as_str() {
        "compaction" => {
            let summary = entry.value.get("summary").and_then(|v| v.as_str())?;
            build(
                MessageRole::User,
                format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"),
                None,
            )
        }
        "branch_summary" => {
            let summary = entry.value.get("summary").and_then(|v| v.as_str())?;
            build(
                MessageRole::User,
                format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"),
                None,
            )
        }
        "custom_message" => {
            let content = entry.value.get("content").map_or_else(String::new, |c| {
                flatten_content(c, out, ToolCallText::Rendered)
            });
            build(MessageRole::User, content, None)
        }
        "message" => {
            let Some(msg) = entry.value.get("message") else {
                // A `message` record with no body is malformed, not a record
                // type this reader chose not to represent.
                out.count("message (no body)");
                return None;
            };
            let role_str = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

            if !PERSISTED_MESSAGE_ROLES.contains(&role_str) {
                let label = if role_str.is_empty() {
                    "(no role)"
                } else {
                    role_str
                };
                out.count(format!("message (unrecognised role:{label})"));
                return None;
            }

            match role_str {
                "assistant" => {
                    let content_val = msg.get("content");
                    // The only arm that also extracts the calls, and so the
                    // only one where rendering them as text too would state
                    // each call twice. See [`ToolCallText`].
                    let content = content_val.map_or_else(String::new, |c| {
                        flatten_content(c, out, ToolCallText::Structural)
                    });
                    let tool_calls = content_val.map(extract_tool_calls).unwrap_or_default();
                    let author = msg
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .or_else(|| out.model_id.clone());
                    // An assistant turn that is nothing but a tool call has no
                    // text, and dropping it would lose the call.
                    if content.trim().is_empty() && tool_calls.is_empty() {
                        out.count("message (assistant, empty)");
                        return None;
                    }
                    Some(CanonicalMessage {
                        idx: 0,
                        role: MessageRole::Assistant,
                        content,
                        timestamp,
                        author,
                        tool_calls,
                        tool_results: vec![],
                        extra: entry.value.clone(),
                    })
                }
                // A tool's output. The prose the model was shown goes in
                // `content`; `toolCallId` and `isError` go in `tool_results`,
                // because nothing else here can hold them — `content` is text
                // and `author` is the tool's name.
                //
                // `convertToLlm` (`dist/proxy-BzhBz8iM.js`) reads all three off
                // this record, so all three are read here. The call id is what
                // makes the observation an answer to a particular call rather
                // than a loose paragraph, and it is what
                // [`crate::pipeline::repair_tool_pairing`] pairs on: with it
                // empty, every OpenClaw tool call arrived at the budget as an
                // orphan.
                //
                // Both fields together used to be unrepresentable. `claude_code`
                // wrote `tool_result` blocks *instead of* `msg.content` for a
                // non-assistant message that had results, and its reader takes
                // text only from `text` blocks — so the observation was deleted
                // on write and its absence confirmed on read, and the pipeline's
                // read-back check failed with "wrote N bytes, read back 0
                // bytes". That writer now emits `tool_result` and then `text`
                // (`claude_code.rs`, `writer_non_assistant_keeps_content_alongside_tool_results`),
                // and `tests/claude_code_shape_test.rs`'s
                // `a_message_with_both_text_and_tool_results_survives_the_round_trip`
                // asserts the whole round-trip rather than the block shape. Every
                // other writer in the tree that emits `tool_results` already kept
                // `content` beside them.
                "toolResult" => {
                    let content = msg.get("content").map_or_else(String::new, |c| {
                        flatten_content(c, out, ToolCallText::Rendered)
                    });
                    if content.trim().is_empty() {
                        out.count("message (toolResult, no text)");
                        return None;
                    }
                    let mut message = build(
                        MessageRole::Tool,
                        content.clone(),
                        msg.get("toolName")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    )?;
                    message.tool_results = vec![ToolResult {
                        call_id: msg
                            .get("toolCallId")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        content,
                        is_error: msg
                            .get("isError")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    }];
                    Some(message)
                }
                "bashExecution" => {
                    // `excludeFromContext` keeps a command in session history
                    // and out of the model's context. Honouring it is the
                    // difference between the transcript and the conversation.
                    if msg
                        .get("excludeFromContext")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    {
                        out.count("message (bashExecution, excluded from context)");
                        return None;
                    }
                    build(MessageRole::User, bash_execution_to_text(msg), None)
                }
                // "user" and "custom" both reach the model as a user turn.
                _ => {
                    let content = msg.get("content").map_or_else(String::new, |c| {
                        flatten_content(c, out, ToolCallText::Rendered)
                    });
                    if content.trim().is_empty() {
                        out.count(format!("message ({role_str}, no text)"));
                        return None;
                    }
                    build(normalize_role("user"), content, None)
                }
            }
        }
        // `model_change` becomes `model_name` and `session_info` becomes the
        // native name, so both are represented and neither is a loss.
        "model_change" | "session_info" => None,
        // The rest are real records that carry no model-visible content:
        // `custom` ("Does NOT participate in LLM context"), `label`,
        // `thinking_level_change`. Counted so the report can say every line
        // was accounted for.
        other => {
            out.count(other.to_string());
            None
        }
    }
}

/// Parse a transcript into the conversation OpenClaw would replay.
fn read_transcript(path: &Path) -> anyhow::Result<Transcript> {
    let mut out = Transcript::default();
    let (entries, leaf) = scan(path, &mut out)?;

    let branch = live_branch(&entries, leaf.as_deref());

    // Everything in the file but off the live branch produces no message, but
    // not for one reason, and the report has to say which.
    //
    // `abandoned` is a path the *user* rewound away from: `SessionManager
    // .branch(id)` moved the leaf back and the next append became a sibling,
    // leaving the old turns in the file forever.
    //
    // `side branch` is not that. `appendMode: "side"` marks a row parked by
    // `mergePromptReleasedSessionEntries` — "preserve entries appended while
    // the active prompt released its file lock; attach them as a side branch so
    // rewrites retain external state without moving the prepared reply branch
    // or adding delivery mirrors to its context". Nobody rewound anything, and
    // reporting it as a rewind describes a session that did not happen.
    let on_branch: HashSet<&str> = branch.iter().map(|e| e.id.as_str()).collect();
    let mut abandoned = 0u64;
    let mut side = 0u64;
    for entry in entries
        .iter()
        .filter(|e| !e.leaf_control && !on_branch.contains(e.id.as_str()))
    {
        if entry.side_append {
            side += 1;
        } else {
            abandoned += 1;
        }
    }
    if abandoned > 0 {
        *out.unrepresented
            .entry("abandoned".to_string())
            .or_insert(0) += abandoned;
    }
    if side > 0 {
        *out.unrepresented
            .entry("side branch".to_string())
            .or_insert(0) += side;
    }

    // State markers are read along the whole branch first, because
    // `buildSessionContext` resolves them regardless of where compaction cuts.
    //
    // It resolves the model from the last `model_change` *or* the last
    // assistant turn, whichever comes later on the path. Only the
    // `model_change` half is taken here: OpenClaw appends one when a session
    // starts and again on every switch, so a transcript that states a model
    // states it here, and an assistant turn's `model` is already read as that
    // message's `author` rather than the session's.
    for entry in &branch {
        match entry.kind.as_str() {
            "model_change" => {
                if let Some(m) = entry.value.get("modelId").and_then(|v| v.as_str()) {
                    out.model_id = Some(m.to_string());
                }
            }
            "session_info" => {
                if let Some(name) = entry.value.get("name").and_then(|v| v.as_str()) {
                    out.session_name = Some(name.to_string());
                }
            }
            _ => {}
        }
    }

    // Compaction: the newest one on the branch replaces everything before
    // `firstKeptEntryId` with its own summary. Mirrors `buildSessionContext`,
    // including the order — the summary goes first, then what was kept.
    let compaction_at = branch.iter().rposition(|e| e.kind == "compaction");
    let selected: Vec<&Entry> = match compaction_at {
        Some(index) => {
            let first_kept = branch[index]
                .value
                .get("firstKeptEntryId")
                .and_then(|v| v.as_str());
            let mut kept: Vec<&Entry> = vec![branch[index]];
            let mut found = false;
            let mut dropped = 0u64;
            for entry in &branch[..index] {
                if Some(entry.id.as_str()) == first_kept {
                    found = true;
                }
                if found {
                    kept.push(entry);
                } else {
                    dropped += 1;
                }
            }
            kept.extend_from_slice(&branch[index + 1..]);
            if dropped > 0 {
                *out.unrepresented
                    .entry("compacted_away".to_string())
                    .or_insert(0) += dropped;
            }
            kept
        }
        None => branch.clone(),
    };

    for entry in selected {
        if let Some(message) = entry_message(entry, &mut out) {
            if out.started_at.is_none() {
                out.started_at = message.timestamp;
            }
            if message.timestamp.is_some() {
                out.ended_at = message.timestamp;
            }
            out.messages.push(message);
        }
    }

    reindex_messages(&mut out.messages);
    Ok(out)
}

impl OpenClaw {
    /// OpenClaw's mutable state directory, resolved the way OpenClaw resolves
    /// it. Both variables below are OpenClaw's own and mean what OpenClaw means
    /// by them:
    ///
    /// 1. `OPENCLAW_STATE_DIR` — "Override the mutable state directory". It is
    ///    an explicit path variable, so it outranks `OPENCLAW_HOME`.
    /// 2. `OPENCLAW_HOME` — "Override the home directory used for OpenClaw path
    ///    defaults". It replaces the *home* directory, so `.openclaw` is joined
    ///    onto it. OpenClaw's own `docker-compose.yml` shows the pair:
    ///    `OPENCLAW_HOME=/home/node` alongside
    ///    `OPENCLAW_STATE_DIR=/home/node/.openclaw`.
    /// 3. `~/.openclaw`.
    ///
    /// An empty value counts as unset.
    fn state_dir() -> PathBuf {
        if let Some(state) =
            std::env::var_os("OPENCLAW_STATE_DIR").filter(|value| !value.is_empty())
        {
            return PathBuf::from(state);
        }
        let home = match std::env::var_os("OPENCLAW_HOME").filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => dirs::home_dir().unwrap_or_default(),
        };
        home.join(".openclaw")
    }

    fn session_key(session_id: &str) -> String {
        format!(
            "agent:{DEFAULT_AGENT_ID}:{}",
            session_id.to_ascii_lowercase()
        )
    }

    fn resume_spec(session_id: &str) -> LaunchSpec {
        LaunchSpec::new(
            "openclaw",
            [
                "tui".to_string(),
                "--session".to_string(),
                Self::session_key(session_id),
            ],
        )
    }

    /// Every agent's sessions directory, so that a session belonging to a
    /// non-default agent (`openclaw sessions --agent work`) is still found.
    /// Only directories that exist are returned.
    fn session_dirs_reporting(unreadable: &mut Vec<UnreadableSource>) -> Vec<PathBuf> {
        let agents_dir = Self::state_dir().join("agents");
        // An `agents/` that exists and cannot be read hides every session on
        // the machine. `let Ok(entries) = .. else { return Vec::new() }`
        // reported that as "OpenClaw has no sessions".
        let mut dirs: Vec<PathBuf> = read_dir_reporting(&agents_dir, unreadable)
            .into_iter()
            .map(|entry| entry.path().join("sessions"))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        dirs
    }

    /// `session_dirs_reporting` for the callers with nowhere to put a read
    /// failure — `detect`, `session_roots`, `owns_session`.
    fn session_dirs() -> Vec<PathBuf> {
        Self::session_dirs_reporting(&mut Vec::new())
    }
}

/// OpenClaw's own `isPrimarySessionTranscriptFileName`, from
/// `dist/paths-C2C4lJH6.js` in `openclaw@2026.7.1-2`:
///
/// ```js
/// if (fileName === "sessions.json") return false;
/// if (!fileName.endsWith(".jsonl")) return false;
/// if (isTrajectoryRuntimeArtifactName(fileName)) return false;
/// if (isCompactionCheckpointTranscriptFileName(fileName)) return false;
/// return !isSessionArchiveArtifactName(fileName);
/// ```
///
/// It is transcribed rather than approximated because OpenClaw writes at least
/// twelve other things into that one directory — `sessions.json` and its
/// `.bak.<epochMs>`/`.tmp` neighbours, `<id>.trajectory.jsonl`,
/// `<id>.checkpoint.<uuid>.jsonl`, `<name>.deleted|reset|bak.<ISO>`,
/// `<id>.jsonl.lock`, `<id>.jsonl.compact.<uuid>.tmp`,
/// `<id>.jsonl.bak-<pid>-<ms>`, `<id>.jsonl.corrupt-<ISO>-<hex>.jsonl`, and a
/// `skills-prompts/sha256/` tree — and three of those end in `.jsonl`.
///
/// The last of them, `.corrupt-….jsonl`, is *not* excluded by OpenClaw's rule
/// either. It is a real transcript OpenClaw set aside after failing to parse
/// it, so admitting it costs a duplicate row rather than a wrong one; matching
/// the tool is worth more than diverging to fix its own edge case.
fn is_primary_transcript_name(name: &str) -> bool {
    if name == "sessions.json" || !name.ends_with(".jsonl") {
        return false;
    }
    // `<sessionId>.trajectory.jsonl` — the trajectory runtime artifact.
    if name.ends_with(".trajectory.jsonl") {
        return false;
    }
    // `<sessionId>.checkpoint.<uuid>.jsonl` — a compaction checkpoint.
    let stem = &name[..name.len() - ".jsonl".len()];
    if let Some((before, uuid)) = stem.rsplit_once('.')
        && before.ends_with(".checkpoint")
        && is_uuid(uuid)
    {
        return false;
    }
    // `<name>.<deleted|reset|bak>.<ISO stamp>` — an archived transcript. The
    // suffix follows `.jsonl`, so any name still ending in `.jsonl` at this
    // point has not been archived; the check is kept explicit because the
    // archive writer takes the whole path and appends.
    !matches!(
        stem.rsplit_once('.').map(|(_, tail)| tail),
        Some("deleted" | "reset" | "bak")
    )
}

/// A canonical lowercase 8-4-4-4-12 UUID, as OpenClaw's own regexes require.
fn is_uuid(candidate: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = candidate.split('-');
    for width in groups {
        match parts.next() {
            Some(part) if part.len() == width && part.chars().all(|c| c.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

impl Provider for OpenClaw {
    fn name(&self) -> &str {
        "OpenClaw"
    }

    fn slug(&self) -> &str {
        "openclaw"
    }

    fn cli_alias(&self) -> &str {
        "ocl"
    }

    fn detect(&self) -> DetectionResult {
        let dirs = Self::session_dirs();
        let state = Self::state_dir();
        // The state directory existing is enough to call OpenClaw installed:
        // the per-agent sessions directory is only created once a session runs.
        let state_exists = state.is_dir();
        let installed = !dirs.is_empty() || state_exists;
        let evidence = if !dirs.is_empty() {
            dirs.iter()
                .map(|dir| format!("sessions directory found: {}", dir.display()))
                .collect()
        } else if state_exists {
            vec![format!(
                "state directory found (no agent sessions yet): {}",
                state.display()
            )]
        } else {
            vec![]
        };
        trace!(provider = "openclaw", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        Self::session_dirs()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let mut listing = SessionListing::default();
        for root in Self::session_dirs_reporting(&mut listing.unreadable) {
            // One level, because that is how far OpenClaw looks. Every scanner
            // pointed at an agent's `sessions/` directory reads exactly one
            // level and guards on `isFile()` —
            // `doctor-state-integrity-D-B71ywJ.js:1484`,
            // `doctor-session-transcripts-CuHKQasv.js:243`,
            // `security-cli-BgOxd0Kk.js:307`,
            // `session-write-lock-BZ_4P1vk.js:428`, `store-BJJhlPrk.js:859`
            // and `:3224`, `engine-qmd-zad3_Bbe.js:147`,
            // `cli.runtime-BQudgd-S.js:308`,
            // `session-cost-usage-B0dBxiXW.js:226` — and the package contains
            // no `readdir(..., { recursive: true })` at all. `paths-C2C4lJH6.js`
            // states the rule outright when resolving a transcript path:
            // `const relativeSegments = parts.slice(sessionsIndex + 1); if
            // (relativeSegments.length !== 1) return;`.
            //
            // The one subdirectory OpenClaw creates there is
            // `sessions/skills-prompts/sha256/<2 hex>/<64 hex>.txt`
            // (`store-BJJhlPrk.js: readSessionPromptBlobFiles`) — a prompt blob
            // store, not sessions.
            //
            // The fix belongs here and not in `is_session_path`, which already
            // transcribes `isPrimarySessionTranscriptFileName` exactly. Under
            // `max_depth(4)` that predicate was being asked what a file was
            // named but never where it was, so an archived or hand-copied
            // `.jsonl` anywhere under the root was rendered as a session.
            for entry in walkdir::WalkDir::new(&root).max_depth(1) {
                let Some(entry) = walk_entry_reporting(entry, &mut listing.unreadable) else {
                    continue;
                };
                let path = entry.path();
                if !entry.file_type().is_file() || !self.is_session_path(path) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                listing
                    .sessions
                    .push((stem.to_string(), path.to_path_buf()));
            }
        }
        Some(listing)
    }

    /// See [`is_primary_transcript_name`] — OpenClaw's own rule, transcribed
    /// from the shipped package.
    fn is_session_path(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_primary_transcript_name)
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let path = self
            .list_sessions()?
            .sessions
            .into_iter()
            .find_map(|(listed_id, path)| (listed_id == session_id).then_some(path))?;
        debug!(
            provider = "openclaw",
            path = %path.display(),
            session_id,
            "owns listed session"
        );
        Some(path)
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading OpenClaw session");

        let transcript = read_transcript(path)?;

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let title = transcript
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let workspace = transcript.cwd.as_ref().map(PathBuf::from);

        // `unrepresented` is present only when something was actually left
        // over. Absent means every line was accounted for; it never means this
        // reader did not look.
        let mut metadata = serde_json::json!({
            "source": "openclaw",
            "cwd": transcript.cwd,
            "header_session_id": transcript.header_id,
            "unrepresented": transcript.describe_unrepresented(),
        });
        if let (Some(name), Some(obj)) = (&transcript.session_name, metadata.as_object_mut()) {
            obj.insert(
                crate::model::NATIVE_NAME_META_KEY.to_string(),
                serde_json::Value::String(name.clone()),
            );
        }

        info!(
            session_id,
            messages = transcript.messages.len(),
            unrepresented = transcript.describe_unrepresented(),
            "OpenClaw session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "openclaw".to_string(),
            workspace,
            title,
            started_at: transcript.started_at,
            ended_at: transcript.ended_at,
            messages: transcript.messages,
            metadata,
            source_path: path.to_path_buf(),
            model_name: transcript.model_id,
        })
    }

    fn write_session(
        &self,
        _session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        Err(anyhow::anyhow!(OPENCLAW_WRITE_REFUSAL))
    }

    fn write_refusal(&self) -> Option<&'static str> {
        Some(OPENCLAW_WRITE_REFUSAL)
    }

    fn resume_command(&self, session_id: &str) -> String {
        Self::resume_spec(session_id).display()
    }

    fn launch_spec(&self, session_id: &str) -> Option<LaunchSpec> {
        let spec = Self::resume_spec(session_id);
        if session_id.is_empty() {
            Some(spec)
        } else {
            Some(spec.targeting_session(&Self::session_key(session_id)))
        }
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Render a candidate OpenClaw transcript for parser-fidelity tests.
///
/// This is deliberately not a supported import path. OpenClaw's resume index
/// is shared gateway state without a cross-process file lock, so casr refuses
/// target writes until it can use the authenticated gateway lifecycle.
///
/// The reader's defects had writer twins, and both are fixed here:
///
/// - **The tree.** Every entry now carries `parentId`, chained. A file whose
///   rows have no parent link is only readable through OpenClaw's legacy
///   compatibility path, and nothing written today should need it.
/// - **`version`.** A number, and the current one — see [`SESSION_VERSION`].
/// - **Block content.** `AssistantMessage.content` is declared
///   `(TextContent | ThinkingContent | ToolCall)[]`, never a bare string, so
///   assistant turns are written as blocks.
/// - **Tool results.** Written as `toolResult` messages, which is where
///   OpenClaw keeps them; they used to be dropped entirely.
///
/// - **The session's model.** A `model_change` entry, because `SessionHeader`
///   has nowhere to put one and `buildSessionContext` reads it from nowhere
///   else.
///
/// Fields casr cannot know — `usage`, `stopReason`, `api`, `provider` — are left
/// absent rather than filled with zeros. A zero token count is a claim, and a
/// false one; absence is what casr actually knows.
///
/// That is a tolerated deviation, not a sanctioned one: `AssistantMessage`
/// declares `usage: Usage` and `stopReason: StopReason` as *required*
/// (`dist/types-CFIUY_La.d.ts:213-214`). It is safe because nothing enforces
/// them on a persisted row — `isAgentMessage` checks only `content` for an
/// assistant turn (`dist/transcript-rewrite-BHL7q_3D.js`), and OpenClaw's
/// consumers guard — so the file stays readable. Filling them would not.
#[cfg(test)]
fn render_session(session_id: &str, session: &CanonicalSession) -> String {
    let mut lines: Vec<String> = Vec::new();

    let workspace = session
        .workspace
        .as_ref()
        .and_then(|w| w.to_str())
        .unwrap_or("/tmp");
    let started = session
        .started_at
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let header = serde_json::json!({
        "type": "session",
        "version": SESSION_VERSION,
        "id": session_id,
        "timestamp": started,
        "cwd": workspace,
    });
    lines.push(serde_json::to_string(&header).unwrap_or_default());

    let mut parent: Option<String> = None;

    // The session's model, in the one record OpenClaw reads it from.
    //
    // `SessionHeader` is exactly `{type, version?, id, timestamp, cwd,
    // parentSession?}` (`dist/session-manager-RXl7XED7.d.ts`) — there is no
    // header field to put it in, and `buildSessionContext` resolves the model
    // from `model_change` entries and assistant turns, never from the header.
    // OpenClaw itself appends one of these when a session starts and again on
    // every switch, so this is the shape rather than an approximation of it.
    //
    // `provider` is omitted because casr does not observe it: `model_name` is a
    // model id, and inferring "anthropic" from a `claude-` prefix would be a
    // claim casr cannot support. That has a measured cost on
    // `openclaw@2026.7.1-2` and it is the smaller one: OpenClaw's runtime still
    // resolves `modelId` from this entry, but `isSessionEntry` requires a
    // non-empty `provider` string, so `readTranscriptFileState` drops the row
    // if OpenClaw ever rewrites the transcript, and `modelRegistry.find` cannot
    // restore the model without it. A wrong provider would not restore it
    // either — it would only make the failure say something false first.
    if let Some(model_id) = session.model_name.as_deref().filter(|m| !m.is_empty()) {
        let id = "mc1".to_string();
        let entry = serde_json::json!({
            "type": "model_change",
            "id": id,
            "parentId": parent,
            "timestamp": started,
            "modelId": model_id,
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
        parent = Some(id);
    }

    for (i, msg) in session.messages.iter().enumerate() {
        let ts_ms = msg
            .timestamp
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let ts_str = chrono::DateTime::from_timestamp_millis(ts_ms)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

        // Only the five roles in [`PERSISTED_MESSAGE_ROLES`] exist. `system` is
        // not one of them, and neither is whatever the source tool called a
        // turn: `Message` is `UserMessage | AssistantMessage |
        // ToolResultMessage` (`dist/types-CFIUY_La.d.ts`), widened by
        // `AgentMessage` (`dist/types-D0CdrmU4.d.ts`) with `bashExecution` and
        // `custom` only.
        //
        // Measured on `openclaw@2026.7.1-2` against a `{"role":"system"}` row:
        // `convertToLlm` drops it (`default: return;`), so the model never sees
        // it, and `isAgentMessage` rejects it (`default: return false;`), so
        // `readTranscriptFileState` accepts only the *other* rows and OpenClaw
        // deletes it from the file on the next rewrite.
        //
        // So it becomes a user turn — the same slot `bashExecution`, `custom`,
        // `custom_message` and both summary records land in, and the same
        // mapping the reader applies coming the other way (see the table in the
        // module docs). `custom_message` was the alternative and preserves no
        // more: it also reaches the model as a user turn, this reader also
        // reads it back as [`MessageRole::User`], and its `customType` would
        // have to carry a role name OpenClaw itself never writes.
        //
        // This belongs only to the candidate renderer used by parser-fidelity
        // tests. It is not a supported import path. A future gateway-backed
        // writer would also have to declare this role fold before enabling
        // OpenClaw as a target.
        let role_str = match &msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "toolResult",
            MessageRole::System | MessageRole::Other(_) => "user",
        };

        let inner = if role_str == "assistant" {
            // Blocks, per `AssistantMessage.content`.
            let mut blocks = Vec::new();
            if !msg.content.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": msg.content}));
            }
            for tc in &msg.tool_calls {
                blocks.push(serde_json::json!({
                    "type": "toolCall",
                    "id": tc.id.as_deref().unwrap_or(""),
                    "name": tc.name,
                    "arguments": tc.arguments,
                }));
            }
            let mut m = serde_json::json!({
                "role": "assistant",
                "content": blocks,
                "timestamp": ts_ms,
            });
            if let Some(ref author) = msg.author {
                m["model"] = serde_json::Value::String(author.clone());
            }
            m
        } else if role_str == "toolResult" {
            let first = msg.tool_results.first();
            serde_json::json!({
                "role": "toolResult",
                "toolCallId": first.and_then(|r| r.call_id.clone()).unwrap_or_default(),
                "toolName": msg.author.clone().unwrap_or_default(),
                "content": [{"type": "text", "text": msg.content}],
                "isError": first.is_some_and(|r| r.is_error),
                "timestamp": ts_ms,
            })
        } else {
            serde_json::json!({
                "role": role_str,
                "content": msg.content,
                "timestamp": ts_ms,
            })
        };

        let id = format!("m{}", i + 1);
        let entry = serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": ts_str,
            "message": inner,
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
        parent = Some(id);
    }

    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolResult;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn read_openclaw(lines: &[&str]) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(tmp.path(), "test.jsonl", lines);
        let provider = OpenClaw;
        provider.read_session(&path).expect("read_session failed")
    }

    // -----------------------------------------------------------------------
    // The tree — the correctness cases
    // -----------------------------------------------------------------------

    /// The defect this reader was rewritten for. `SessionManager.branch()`
    /// leaves the abandoned path in the file; replaying the file shows the
    /// model content the user removed.
    #[test]
    fn an_abandoned_branch_is_not_replayed() {
        let session = read_openclaw(&[
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-02-14T09:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"one"}}"#,
            r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-02-14T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply one"}]}}"#,
            r#"{"type":"message","id":"x1","parentId":"a2","timestamp":"2026-02-14T09:00:03.000Z","message":{"role":"user","content":"ABANDONED"}}"#,
            r#"{"type":"message","id":"x2","parentId":"x1","timestamp":"2026-02-14T09:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"ABANDONED REPLY"}]}}"#,
            r#"{"type":"message","id":"b1","parentId":"a2","timestamp":"2026-02-14T09:00:05.000Z","message":{"role":"user","content":"three"}}"#,
        ]);

        let text: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(text, vec!["one", "reply one", "three"]);
        assert!(
            !session
                .messages
                .iter()
                .any(|m| m.content.contains("ABANDONED")),
            "content the user rewound away must not be replayed"
        );
        assert_eq!(session.metadata["unrepresented"], "abandoned 2");
    }

    /// A `leaf` control selects the active leaf explicitly, and the walk must
    /// resolve through it rather than treating it as a message.
    #[test]
    fn a_leaf_control_selects_the_branch() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"kept"}}"#,
            r#"{"type":"message","id":"x1","parentId":"a1","timestamp":"2026-02-14T09:00:02.000Z","message":{"role":"user","content":"LATER BUT ABANDONED"}}"#,
            r#"{"type":"leaf","id":"L1","parentId":"x1","targetId":"a1","timestamp":"2026-02-14T09:00:03.000Z"}"#,
        ]);
        let text: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(text, vec!["kept"]);
    }

    /// Rows with no `parentId` are OpenClaw's legacy shape — and the shape casr
    /// itself used to write. They must still read as one linear conversation.
    #[test]
    fn parentless_rows_read_as_a_linear_conversation() {
        let session = read_openclaw(&[
            r#"{"type":"session","id":"s","timestamp":"2026-02-14T09:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"m1","timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"one"}}"#,
            r#"{"type":"message","id":"m2","timestamp":"2026-02-14T09:00:02.000Z","message":{"role":"user","content":"two"}}"#,
        ]);
        let text: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(text, vec!["one", "two"]);
        assert!(session.metadata["unrepresented"].is_null());
    }

    /// Compaction replaces the history before `firstKeptEntryId` with its
    /// summary. Replaying what it replaced shows the model a conversation it
    /// no longer has.
    #[test]
    fn compaction_replaces_the_history_it_summarised() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"ANCIENT"}}"#,
            r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-02-14T09:00:02.000Z","message":{"role":"user","content":"kept"}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"a2","timestamp":"2026-02-14T09:00:03.000Z","summary":"we discussed things","firstKeptEntryId":"a2","tokensBefore":50000}"#,
            r#"{"type":"message","id":"a3","parentId":"c1","timestamp":"2026-02-14T09:00:04.000Z","message":{"role":"user","content":"after"}}"#,
        ]);

        assert!(
            !session
                .messages
                .iter()
                .any(|m| m.content.contains("ANCIENT"))
        );
        assert!(
            session.messages[0].content.contains("we discussed things"),
            "the compaction summary is model-visible and must survive"
        );
        let text: Vec<&str> = session
            .messages
            .iter()
            .skip(1)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(text, vec!["kept", "after"]);
        assert_eq!(session.metadata["unrepresented"], "compacted_away 1");
    }

    // -----------------------------------------------------------------------
    // Content that reaches the model
    // -----------------------------------------------------------------------

    /// `ThinkingContent.thinking`, not `.text`. Reading `.text` yields empty
    /// reasoning on every session that has any.
    #[test]
    fn thinking_blocks_read_their_thinking_field_not_text() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weighing it up"},{"type":"text","text":"done"}]}}"#,
        ]);
        assert_eq!(
            session.messages[0].content,
            "[Thinking] weighing it up\ndone"
        );
    }

    /// A `bashExecution` record has no `content` field at all. Reaching for
    /// `content` finds nothing and drops a shell transcript the model saw.
    #[test]
    fn bash_execution_records_are_rendered_not_dropped() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"bashExecution","command":"npm test","output":"1 failing","exitCode":1,"cancelled":false,"truncated":false,"timestamp":1771060324000}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(
            session.messages[0].content,
            "Ran `npm test`\n```\n1 failing\n```\n\nCommand exited with code 1"
        );
    }

    /// `excludeFromContext` keeps a command in history and out of context.
    #[test]
    fn bash_execution_excluded_from_context_is_counted_not_shown() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"bashExecution","command":"ls","output":"a","exitCode":0,"excludeFromContext":true,"timestamp":1771060324000}}"#,
        ]);
        assert!(session.messages.is_empty());
        assert_eq!(
            session.metadata["unrepresented"],
            "message (bashExecution, excluded from context) 1"
        );
    }

    /// `custom_message` "DOES participate in LLM context" — the type
    /// definition says so in those words.
    #[test]
    fn custom_message_entries_reach_the_model() {
        let session = read_openclaw(&[
            r#"{"type":"custom_message","id":"c1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","customType":"skill","content":"Loaded skill: release","display":true}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Loaded skill: release");
    }

    /// A `custom` *message* carries content; a `custom` *entry* does not.
    #[test]
    fn custom_role_messages_reach_the_model_but_custom_entries_do_not() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"custom","customType":"memory","content":"Project rule: retry 3x.","display":true,"timestamp":1771060324000}}"#,
            r#"{"type":"custom","id":"c1","parentId":"m1","timestamp":"2026-02-14T09:00:02.000Z","customType":"telemetry","data":{"turns":3}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Project rule: retry 3x.");
        assert_eq!(session.metadata["unrepresented"], "custom 1");
    }

    #[test]
    fn branch_summaries_reach_the_model() {
        let session = read_openclaw(&[
            r#"{"type":"branch_summary","id":"b1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","fromId":"x","summary":"tried Rust, rejected"}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert!(session.messages[0].content.contains("tried Rust, rejected"));
        assert!(session.messages[0].content.contains("came back from"));
    }

    /// A tool's output keeps its text and gets the Tool role — the pre-fix
    /// reader gave it `Other("toolresult")`, which every writer then rendered
    /// as an ordinary user turn.
    #[test]
    fn tool_results_keep_their_role_and_their_text() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"out"}],"isError":false}}"#,
        ]);
        assert_eq!(session.messages[0].role, MessageRole::Tool);
        assert_eq!(session.messages[0].content, "out");
        assert_eq!(session.messages[0].author.as_deref(), Some("bash"));
        // `toolCallId` and `isError` have nowhere else to go: `content` is text
        // and `author` is the tool's name. Empty here meant every OpenClaw tool
        // call reached the budget unpaired.
        assert_eq!(session.messages[0].tool_results.len(), 1);
        assert_eq!(
            session.messages[0].tool_results[0].call_id.as_deref(),
            Some("c1")
        );
        assert_eq!(session.messages[0].tool_results[0].content, "out");
        assert!(!session.messages[0].tool_results[0].is_error);
    }

    /// `isError` is the difference between "the tool answered" and "the tool
    /// failed", and it is a field of the record rather than of its text.
    #[test]
    fn tool_result_failure_is_carried_as_a_failure() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"toolResult","toolCallId":"c9","toolName":"bash","content":[{"type":"text","text":"no such file"}],"isError":true}}"#,
        ]);
        assert_eq!(session.messages[0].tool_results.len(), 1);
        assert!(session.messages[0].tool_results[0].is_error);
        assert_eq!(
            session.messages[0].tool_results[0].call_id.as_deref(),
            Some("c9")
        );
    }

    #[test]
    fn tool_calls_are_extracted() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"toolCall","id":"tc1","name":"read_file","arguments":{"path":"/test.rs"}}]}}"#,
        ]);
        assert_eq!(session.messages[0].tool_calls.len(), 1);
        assert_eq!(session.messages[0].tool_calls[0].name, "read_file");
        assert_eq!(
            session.messages[0].tool_calls[0].id,
            Some("tc1".to_string())
        );
    }

    /// The placeholder is dropped in the assistant arm and nowhere else.
    ///
    /// `flatten_content` is shared with the `user`/`custom`, `custom_message`
    /// and `toolResult` paths, and none of those calls `extract_tool_calls`, so
    /// none of them ever duplicated anything and on all three `[Tool: …]` is
    /// the *only* record of the call. Widening the fix to `flatten_content`
    /// itself would silently drop content from record types that never had the
    /// bug — this is the assertion that says it did not.
    #[test]
    fn reader_tool_call_text_is_rendered_where_it_is_the_only_record() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"see"},{"type":"toolCall","id":"tc1","name":"grep","arguments":{}}]}}"#,
            r#"{"type":"custom_message","id":"m2","parentId":"m1","timestamp":"2026-02-14T09:00:02.000Z","content":[{"type":"text","text":"skill"},{"type":"toolCall","id":"tc2","name":"fetch","arguments":{}}]}"#,
            r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-02-14T09:00:03.000Z","message":{"role":"toolResult","toolCallId":"tc1","toolName":"grep","content":[{"type":"text","text":"hit"},{"type":"toolCall","id":"tc3","name":"nested","arguments":{}}]}}"#,
        ]);
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].content, "see\n[Tool: grep]");
        assert_eq!(session.messages[1].content, "skill\n[Tool: fetch]");
        assert_eq!(session.messages[2].content, "hit\n[Tool: nested]");
        // And none of the three claims the call structurally, so the text is
        // not a second copy of anything.
        assert!(session.messages.iter().all(|m| m.tool_calls.is_empty()));
    }

    /// An assistant turn that is only a tool call has no text. Dropping empty
    /// content would lose the call with it.
    #[test]
    fn an_assistant_turn_that_is_only_a_tool_call_survives() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"tc1","name":"bash","arguments":{}}]}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].tool_calls.len(), 1);
    }

    #[test]
    fn image_blocks_are_counted_not_silently_dropped() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","data":"AAA","mimeType":"image/png"}]}}"#,
        ]);
        assert_eq!(session.messages[0].content, "look");
        assert_eq!(session.metadata["unrepresented"], "content block (image) 1");
    }

    // -----------------------------------------------------------------------
    // Accounting
    // -----------------------------------------------------------------------

    #[test]
    fn records_that_carry_no_model_visible_content_are_counted() {
        let session = read_openclaw(&[
            r#"{"type":"label","id":"l1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","targetId":"x","label":"cp"}"#,
            r#"{"type":"thinking_level_change","id":"t1","parentId":"l1","timestamp":"2026-02-14T09:00:02.000Z","thinkingLevel":"high"}"#,
        ]);
        assert_eq!(session.messages.len(), 0);
        assert_eq!(
            session.metadata["unrepresented"],
            "label 1, thinking_level_change 1"
        );
    }

    #[test]
    fn a_record_type_outside_the_published_set_is_flagged_as_unrecognised() {
        let session = read_openclaw(&[
            r#"{"type":"quantum_entry","id":"q1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z"}"#,
        ]);
        assert_eq!(
            session.metadata["unrepresented"],
            "unrecognised:quantum_entry 1"
        );
    }

    #[test]
    fn unrepresented_is_absent_when_everything_was_accounted_for() {
        let session = read_openclaw(&[
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-02-14T09:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
        ]);
        assert!(session.metadata["unrepresented"].is_null());
    }

    #[test]
    fn malformed_lines_are_counted_without_failing_the_file() {
        let session = read_openclaw(&[
            "",
            "not-json",
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","message":{"role":"user","content":"survived"}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "survived");
        assert_eq!(session.metadata["unrepresented"], "unparseable line 1");
    }

    // -----------------------------------------------------------------------
    // Envelope
    // -----------------------------------------------------------------------

    #[test]
    fn reader_wrapped_messages() {
        let session = read_openclaw(&[
            r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-02-01T16:00:00.000Z","cwd":"/home/user/project"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00.828Z","message":{"role":"user","content":[{"type":"text","text":"Hello OpenClaw"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:06.672Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"},{"type":"toolCall","id":"tc1","name":"exec","arguments":{}}],"model":"claude-opus-4-5"}}"#,
        ]);

        assert_eq!(session.provider_slug, "openclaw");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello OpenClaw");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        // This used to assert `content.contains("[Tool: exec]")`, and that
        // assertion was encoding the defect rather than a requirement: an
        // assistant turn's `toolCall` blocks are returned in `tool_calls`, so
        // rendering them into the text as well stated the call twice and made
        // conversions sourced from OpenClaw duplicate the call in prose.
        // What is real — the call is not lost — is asserted
        // structurally just below, and `[Tool: …]` is still rendered for the
        // three record types that have no structural channel (see
        // [`ToolCallText`], and the test named
        // `reader_tool_call_text_is_rendered_where_it_is_the_only_record`).
        assert_eq!(session.messages[1].content, "Hi there!");
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert_eq!(session.messages[1].tool_calls[0].name, "exec");
        assert_eq!(
            session.messages[1].author,
            Some("claude-opus-4-5".to_string())
        );
        assert_eq!(session.workspace, Some(PathBuf::from("/home/user/project")));
        assert_eq!(session.metadata["header_session_id"], "abc");
        assert!(session.started_at.is_some());
    }

    #[test]
    fn reader_plain_string_content() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","message":{"role":"user","content":"Plain string, no blocks"}}"#,
        ]);
        assert_eq!(session.messages[0].content, "Plain string, no blocks");
    }

    #[test]
    fn reader_session_id_from_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "my-openclaw-session.jsonl",
            &[
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","message":{"role":"user","content":"test"}}"#,
            ],
        );
        let session = OpenClaw.read_session(&path).unwrap();
        assert_eq!(session.session_id, "my-openclaw-session");
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Welcome"}]}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Refactor the auth module"}}"#,
        ]);
        assert_eq!(session.title.as_deref(), Some("Refactor the auth module"));
    }

    /// `session_info` is the user's display name for the session — a record
    /// type `pi` has never had, and one of the reasons this parser is separate.
    #[test]
    fn session_info_becomes_the_native_name() {
        let session = read_openclaw(&[
            r#"{"type":"session_info","id":"i1","parentId":null,"timestamp":"2026-02-14T09:00:01.000Z","name":"uploader retry work"}"#,
        ]);
        assert_eq!(
            crate::model::native_name_from_metadata(&session.metadata).as_deref(),
            Some("uploader retry work")
        );
        assert!(
            session.metadata["unrepresented"].is_null(),
            "session_info is represented, so nothing is left over"
        );
    }

    #[test]
    fn reader_empty_file() {
        let session = read_openclaw(&[]);
        assert!(session.messages.is_empty());
        assert!(session.title.is_none());
    }

    #[test]
    fn reader_model_change_tracked() {
        let session = read_openclaw(&[
            r#"{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","provider":"openai","modelId":"gpt-5"}"#,
            r#"{"type":"message","id":"m1","parentId":"mc1","timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"test"}}"#,
        ]);
        assert_eq!(session.model_name, Some("gpt-5".to_string()));
    }

    #[test]
    fn reader_timestamps_parsed() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00.000Z","message":{"role":"user","content":"First"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T17:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Second"}]}}"#,
        ]);
        assert!(session.started_at.unwrap() < session.ended_at.unwrap());
    }

    #[test]
    fn reader_wrapper_timestamp_preferred() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00.828Z","message":{"role":"user","content":"test","timestamp":1769961600827}}"#,
        ]);
        assert!(session.messages[0].timestamp.is_some());
    }

    #[test]
    fn reader_reindexes_messages() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","message":{"role":"user","content":"A"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"B"}]}}"#,
        ]);
        assert_eq!(session.messages[0].idx, 0);
        assert_eq!(session.messages[1].idx, 1);
    }

    #[test]
    fn reader_message_without_inner_message_counted() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z"}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Valid"}}"#,
        ]);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.metadata["unrepresented"], "message (no body) 1");
    }

    #[test]
    fn reader_metadata_has_source() {
        let session = read_openclaw(&[
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:00Z","message":{"role":"user","content":"test"}}"#,
        ]);
        assert_eq!(session.metadata["source"], "openclaw");
    }

    // -----------------------------------------------------------------------
    // Writer
    // -----------------------------------------------------------------------

    fn sample_session() -> CanonicalSession {
        CanonicalSession {
            session_id: "roundtrip-test".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(PathBuf::from("/home/user/project")),
            title: Some("Test".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_001_000_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "Fix the bug".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "I'll fix it now.".to_string(),
                    timestamp: Some(1_700_000_500_000),
                    author: Some("claude-3-opus".to_string()),
                    tool_calls: vec![ToolCall {
                        id: Some("tc1".to_string()),
                        name: "read_file".to_string(),
                        arguments: json!({"path": "/test.rs"}),
                    }],
                    tool_results: vec![],
                    extra: json!({}),
                },
                CanonicalMessage {
                    idx: 2,
                    role: MessageRole::Tool,
                    content: "file contents".to_string(),
                    timestamp: Some(1_700_000_600_000),
                    author: Some("read_file".to_string()),
                    tool_calls: vec![],
                    tool_results: vec![ToolResult {
                        call_id: Some("tc1".to_string()),
                        content: "file contents".to_string(),
                        is_error: false,
                    }],
                    extra: json!({}),
                },
            ],
            metadata: json!({"source": "claude-code"}),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            model_name: None,
        }
    }

    fn render_candidate_and_read_back(session: &CanonicalSession) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(format!("{}.jsonl", session.session_id));
        std::fs::write(&target, render_session(&session.session_id, session)).unwrap();
        OpenClaw.read_session(&target).unwrap()
    }

    /// The candidate renderer's twin of the reader's tree defect: a transcript with no
    /// `parentId` is only readable through OpenClaw's legacy path.
    #[test]
    fn candidate_renderer_links_every_entry_into_the_tree() {
        let session = sample_session();
        let rendered = render_session("roundtrip-test", &session);
        let lines: Vec<&str> = rendered.lines().collect();

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            header["version"], 3,
            "version is the current one, as a number"
        );

        let first: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(first["parentId"].is_null(), "the first entry is the root");
        let second: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(second["parentId"], first["id"]);
        let third: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(third["parentId"], second["id"]);
    }

    /// `AssistantMessage.content` is declared as a block array, never a string.
    #[test]
    fn candidate_renderer_emits_assistant_content_as_blocks() {
        let session = sample_session();
        let rendered = render_session("roundtrip-test", &session);
        let assistant: serde_json::Value =
            serde_json::from_str(rendered.lines().nth(2).unwrap()).unwrap();
        let content = assistant["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "toolCall");
        assert!(
            assistant["message"]["timestamp"].is_number(),
            "the inner message timestamp is epoch millis"
        );
    }

    #[test]
    fn candidate_renderer_emits_tool_results_as_tool_result_messages() {
        let session = sample_session();
        let rendered = render_session("roundtrip-test", &session);
        let tool: serde_json::Value =
            serde_json::from_str(rendered.lines().nth(3).unwrap()).unwrap();
        assert_eq!(tool["message"]["role"], "toolResult");
        assert_eq!(tool["message"]["toolCallId"], "tc1");
        assert_eq!(tool["message"]["toolName"], "read_file");
    }

    #[test]
    fn candidate_renderer_roundtrip() {
        let original = sample_session();
        let readback = render_candidate_and_read_back(&original);
        assert_eq!(readback.messages.len(), 3);
        assert_eq!(readback.messages[0].role, MessageRole::User);
        assert_eq!(readback.messages[0].content, "Fix the bug");
        assert_eq!(readback.messages[1].role, MessageRole::Assistant);
        assert!(readback.messages[1].content.contains("I'll fix it now."));
        assert_eq!(
            readback.messages[1].author,
            Some("claude-3-opus".to_string())
        );
        assert_eq!(readback.messages[1].tool_calls.len(), 1);
        assert_eq!(readback.messages[2].role, MessageRole::Tool);
        assert_eq!(readback.messages[2].content, "file contents");
        assert_eq!(readback.messages[2].author.as_deref(), Some("read_file"));
        assert_eq!(
            readback.workspace,
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn native_resume_command_uses_the_full_session_key() {
        assert_eq!(
            OpenClaw.resume_command("my-session"),
            "openclaw tui --session agent:main:my-session"
        );
    }

    // -----------------------------------------------------------------------
    // Provider metadata
    // -----------------------------------------------------------------------

    #[test]
    fn provider_metadata() {
        let provider = OpenClaw;
        assert_eq!(provider.name(), "OpenClaw");
        assert_eq!(provider.slug(), "openclaw");
        assert_eq!(provider.cli_alias(), "ocl");
        assert_eq!(provider.write_refusal(), Some(OPENCLAW_WRITE_REFUSAL));
    }
}
