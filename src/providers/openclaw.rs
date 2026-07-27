//! OpenClaw provider — reads/writes the `@openclaw/ai` session transcript.
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
//! - widens the message union. `pi`'s `Message` is user/assistant/toolResult.
//!   OpenClaw's `AgentMessage` (`dist/types-D0CdrmU4.d.ts`) adds
//!   `bashExecution` and `custom` as *persisted* roles — and neither has a
//!   `content` field. A reader that reaches for `content` finds nothing and
//!   drops a shell transcript the model was shown.
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use tracing::{debug, info, trace, warn};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, normalize_role, parse_timestamp,
    reindex_messages, truncate_title,
};
use crate::providers::{Provider, WriteOptions, WrittenSession};

/// OpenClaw's default agent id. Sessions are keyed by agent, and an agent id is
/// mandatory in the path, so casr writes as the agent OpenClaw itself defaults
/// to rather than inventing one.
const DEFAULT_AGENT_ID: &str = "main";

/// `CURRENT_SESSION_VERSION` from `@openclaw/ai@2026.7.1-2`. Written as a
/// number, because OpenClaw compares it numerically: `migrateToCurrentVersion`
/// reads `header.version ?? 1` and returns early on `>= 3`. A string version
/// fails that comparison, and OpenClaw then reports the file as migrated and
/// rewrites it.
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

/// Flatten an OpenClaw `content` into a single string.
///
/// `content` is a plain string or an array of typed blocks. Note `thinking`
/// carries its prose under `thinking`, not `text` — reading `text` here yields
/// empty reasoning on every session that has any.
fn flatten_content(content: &serde_json::Value, out: &mut Transcript) -> String {
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
            "toolCall" => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                parts.push(format!("[Tool: {name}]"));
            }
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

        if kind == "session" {
            out.header_id = value.get("id").and_then(|v| v.as_str()).map(String::from);
            out.cwd = value.get("cwd").and_then(|v| v.as_str()).map(String::from);
            if let Some(m) = value.get("modelId").and_then(|v| v.as_str()) {
                out.model_id = Some(m.to_string());
            }
            if let Some(ts) = value.get("timestamp").and_then(parse_timestamp) {
                out.started_at = Some(ts);
            }
            continue;
        }

        let Some(id) = value.get("id").and_then(|v| v.as_str()).map(String::from) else {
            let label = if kind.is_empty() { "(no type field)" } else { kind };
            out.count(format!("record without id ({label})"));
            continue;
        };

        let is_leaf = kind == "leaf";
        let canonical = CANONICAL_RECORD_TYPES.contains(&kind);
        if !is_leaf && !canonical {
            // Either the format grew a record type or this is not an OpenClaw
            // transcript. Both are things an operator has to be able to see.
            let label = if kind.is_empty() { "(no type field)" } else { kind };
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
            value.get("targetId").and_then(|v| v.as_str()).map(String::from)
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
                flatten_content(c, out)
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
                    let content = content_val
                        .map_or_else(String::new, |c| flatten_content(c, out));
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
                // A tool's output. The text goes in `content`; `tool_results`
                // is deliberately left empty, matching
                // [`crate::providers::pi_session`], which reads the very same
                // record in the sibling format.
                //
                // Populating both is what the canonical model looks like it
                // wants, and it breaks conversion: `claude_code`'s writer drops
                // `msg.content` for any non-assistant message that has
                // `tool_results` and emits `tool_result` blocks instead, while
                // its reader's `claude_extract_text_content` never reads those
                // blocks back as text. Read-back verification then fails with
                // "wrote N bytes, read back 0 bytes" and the conversion is
                // rolled back. That asymmetry is a `claude_code` defect, not an
                // OpenClaw one — it is latent only because no reader in the
                // tree populated `content` and `tool_results` together — so it
                // is reported rather than worked around here. `toolCallId` and
                // `isError` are the cost, and they are structural rather than
                // model-visible: the prose the model was shown is `content`.
                "toolResult" => {
                    let content = msg
                        .get("content")
                        .map_or_else(String::new, |c| flatten_content(c, out));
                    if content.trim().is_empty() {
                        out.count("message (toolResult, no text)");
                        return None;
                    }
                    build(
                        MessageRole::Tool,
                        content,
                        msg.get("toolName")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    )
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
                    let content = msg
                        .get("content")
                        .map_or_else(String::new, |c| flatten_content(c, out));
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

    // Everything in the file but off the live branch is a path the user
    // abandoned. Replaying it would resurrect content OpenClaw removed.
    let on_branch: HashSet<&str> = branch.iter().map(|e| e.id.as_str()).collect();
    let abandoned = entries
        .iter()
        .filter(|e| !e.leaf_control && !on_branch.contains(e.id.as_str()))
        .count();
    if abandoned > 0 {
        *out.unrepresented.entry("abandoned".to_string()).or_insert(0) += abandoned as u64;
    }

    // State markers are read along the whole branch first: `buildSessionContext`
    // resolves the model from the last `model_change` (or the last assistant
    // turn) regardless of where compaction cuts.
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

    /// The sessions directory of one agent: `<state>/agents/<agent-id>/sessions`.
    ///
    /// Confirmed against the released package: `docs/concepts/session.md` gives
    /// "**Transcripts:** `~/.openclaw/agents/<agentId>/sessions/<sessionId>.jsonl`",
    /// and `dist/diagnostic-DhwkYT4X.js` builds exactly
    /// `join(resolveStateDir(), "agents", agentId, "sessions", `${runId}.jsonl`)`.
    fn agent_sessions_dir(agent_id: &str) -> PathBuf {
        Self::state_dir()
            .join("agents")
            .join(agent_id)
            .join("sessions")
    }

    /// Where casr writes: the default agent's sessions directory.
    fn home_dir() -> PathBuf {
        Self::agent_sessions_dir(DEFAULT_AGENT_ID)
    }

    /// Every agent's sessions directory, so that a session belonging to a
    /// non-default agent (`openclaw sessions --agent work`) is still found.
    /// Only directories that exist are returned.
    fn session_dirs() -> Vec<PathBuf> {
        let agents_dir = Self::state_dir().join("agents");
        let Ok(entries) = std::fs::read_dir(&agents_dir) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join("sessions"))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        dirs
    }
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

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        for root in Self::session_dirs() {
            let candidate = root.join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                debug!(
                    provider = "openclaw",
                    path = %candidate.display(),
                    session_id,
                    "owns session"
                );
                return Some(candidate);
            }
            // Walk subdirectories.
            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == session_id)
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "jsonl")
                {
                    debug!(
                        provider = "openclaw",
                        path = %entry.path().display(),
                        session_id,
                        "owns session (subdirectory)"
                    );
                    return Some(entry.path().to_path_buf());
                }
            }
        }
        None
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
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let session_id = if session.session_id.is_empty() {
            format!("casr-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"))
        } else {
            session.session_id.clone()
        };

        let target_dir = Self::home_dir();
        let target_path = target_dir.join(format!("{session_id}.jsonl"));

        debug!(
            session_id,
            path = %target_path.display(),
            messages = session.messages.len(),
            "writing OpenClaw session"
        );

        let file_content = render_session(&session_id, session);
        let outcome = crate::pipeline::atomic_write(
            &target_path,
            file_content.as_bytes(),
            opts.force,
            self.slug(),
        )?;

        info!(
            session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "OpenClaw session written"
        );

        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id: session_id.clone(),
            resume_command: self.resume_command(&session_id),
            backups: outcome.displaced().into_iter().collect(),
            warnings: Vec::new(),
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("openclaw --resume {session_id}")
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Serialise a session as an OpenClaw transcript.
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
/// Fields casr cannot know — `usage`, `stopReason`, `api` — are left absent
/// rather than filled with zeros. A zero token count is a claim, and a false
/// one; absence is what casr actually knows.
fn render_session(session_id: &str, session: &CanonicalSession) -> String {
    let mut lines: Vec<String> = Vec::new();

    let workspace = session
        .workspace
        .as_ref()
        .and_then(|w| w.to_str())
        .unwrap_or("/tmp");
    let header = serde_json::json!({
        "type": "session",
        "version": SESSION_VERSION,
        "id": session_id,
        "timestamp": session.started_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        "cwd": workspace,
    });
    lines.push(serde_json::to_string(&header).unwrap_or_default());

    let mut parent: Option<String> = None;

    for (i, msg) in session.messages.iter().enumerate() {
        let ts_ms = msg
            .timestamp
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let ts_str = chrono::DateTime::from_timestamp_millis(ts_ms)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

        let role_str = match &msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "toolResult",
            MessageRole::Other(r) => r.as_str(),
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

        let text: Vec<&str> = session.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(text, vec!["one", "reply one", "three"]);
        assert!(
            !session.messages.iter().any(|m| m.content.contains("ABANDONED")),
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
        let text: Vec<&str> = session.messages.iter().map(|m| m.content.as_str()).collect();
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
        let text: Vec<&str> = session.messages.iter().map(|m| m.content.as_str()).collect();
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

        assert!(!session.messages.iter().any(|m| m.content.contains("ANCIENT")));
        assert!(
            session.messages[0].content.contains("we discussed things"),
            "the compaction summary is model-visible and must survive"
        );
        let text: Vec<&str> = session.messages.iter().skip(1).map(|m| m.content.as_str()).collect();
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
        assert_eq!(session.messages[0].content, "[Thinking] weighing it up\ndone");
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
        // Empty on purpose — see the `toolResult` arm of `entry_message`.
        assert!(session.messages[0].tool_results.is_empty());
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
        assert!(session.messages[1].content.contains("Hi there!"));
        assert!(session.messages[1].content.contains("[Tool: exec]"));
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

    fn write_and_read_back(session: &CanonicalSession) -> CanonicalSession {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(format!("{}.jsonl", session.session_id));
        std::fs::write(&target, render_session(&session.session_id, session)).unwrap();
        OpenClaw.read_session(&target).unwrap()
    }

    /// The writer's twin of the reader's tree defect: a transcript with no
    /// `parentId` is only readable through OpenClaw's legacy path.
    #[test]
    fn writer_links_every_entry_into_the_tree() {
        let session = sample_session();
        let rendered = render_session("roundtrip-test", &session);
        let lines: Vec<&str> = rendered.lines().collect();

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 3, "version is the current one, as a number");

        let first: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(first["parentId"].is_null(), "the first entry is the root");
        let second: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(second["parentId"], first["id"]);
        let third: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(third["parentId"], second["id"]);
    }

    /// `AssistantMessage.content` is declared as a block array, never a string.
    #[test]
    fn writer_emits_assistant_content_as_blocks() {
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
    fn writer_emits_tool_results_as_tool_result_messages() {
        let session = sample_session();
        let rendered = render_session("roundtrip-test", &session);
        let tool: serde_json::Value =
            serde_json::from_str(rendered.lines().nth(3).unwrap()).unwrap();
        assert_eq!(tool["message"]["role"], "toolResult");
        assert_eq!(tool["message"]["toolCallId"], "tc1");
        assert_eq!(tool["message"]["toolName"], "read_file");
    }

    #[test]
    fn writer_roundtrip() {
        let original = sample_session();
        let readback = write_and_read_back(&original);
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
    fn writer_resume_command() {
        assert_eq!(
            OpenClaw.resume_command("my-session"),
            "openclaw --resume my-session"
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
    }
}
