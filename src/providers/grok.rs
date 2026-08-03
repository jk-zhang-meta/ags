//! Grok Build provider — reads xAI's official `grok` CLI sessions under
//! `$GROK_HOME/sessions/` (default `~/.grok/sessions/`).
//!
//! Grok Build is xAI's official terminal coding agent (installed from
//! `x.ai/cli` to `~/.grok/bin/grok`). It stores each session in its own
//! directory, grouped by percent-encoded working directory:
//!
//! ```text
//! $GROK_HOME/sessions/<percent-encoded-cwd>/<session-uuid>/
//!   summary.json       ← metadata: id, cwd, title, timestamps, model, counts
//!   updates.jsonl      ← ACP session-update stream (authoritative log)
//!   chat_history.jsonl ← raw chat messages sent to the model
//!   plan.json, events.jsonl, rewind_points.jsonl, …  (ignored)
//! $GROK_HOME/sessions/<percent-encoded-cwd>/prompt_history.jsonl  (ignored)
//! $GROK_HOME/sessions/session_search.sqlite                       (ignored)
//! ```
//!
//! The CLI's bundled docs (`~/.grok/docs/user-guide/17-sessions.md`) state
//! that `updates.jsonl` "is the authoritative conversation log that drives
//! `/resume` and session restore", so it is the read source here;
//! `summary.json` supplies metadata.
//!
//! ## `updates.jsonl` line envelope (empirical, grok 0.2.103)
//!
//! ```json
//! {"timestamp": 1784388059,
//!  "method": "session/update",             // or "_x.ai/session/update"
//!  "params": {
//!    "sessionId": "<uuid>",
//!    "update": {"sessionUpdate": "<kind>", …},
//!    "_meta": {"eventId": "…", "agentTimestampMs": 1784388056266}}}
//! ```
//!
//! Some lines carry `_meta` inside `update` instead (observed:
//! `user_message_chunk` had `update._meta.modelId` / `promptIndex`), so both
//! locations are consulted. `sessionUpdate` kinds are the Agent Client
//! Protocol standard set (`user_message_chunk`, `agent_message_chunk`,
//! `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`) plus x.ai
//! extensions observed empirically (`hook_execution`, `retry_state`). Unknown
//! kinds are skipped tolerantly. Chunk kinds are streaming fragments;
//! consecutive same-kind chunks are coalesced into a single message.
//!
//! **Honesty note:** the `user_message_chunk` shape plus the extension kinds
//! were captured from a real 0.2.103 session; `agent_message_chunk`,
//! `agent_thought_chunk`, `tool_call`, and `tool_call_update` shapes follow
//! the public ACP schema (agentclientprotocol.com), which the CLI's own docs
//! name as its update format — parse defensively either way.
//!
//! ## Write support
//!
//! Writes the two authoritative files documented by Grok (`summary.json` and
//! `updates.jsonl`) and asks the official CLI to export the new session. The
//! export is the vendor-side discovery/readability check; derived indexes are
//! intentionally left to Grok.
//!
//! ## Resume
//!
//! ```bash
//! grok --resume <session-id>
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use tracing::{debug, info, trace};

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, WriteOptions, WrittenSession, read_dir_reporting, store_evidence,
};

/// Provider slug used in canonical metadata.
const SLUG: &str = "grok";
const GROK_BIN_ENV: &str = "GROK_BIN";
const GROK_MODEL: &str = "grok-build";
const GROK_WRITE_REFUSAL: &str = "Grok Build writes require the official `grok` CLI; \
set GROK_BIN or put grok in PATH. The native session format is not safely writable without \
the vendor loader.";

/// Grok Build provider implementation.
pub struct Grok;

impl Grok {
    fn binary_path() -> anyhow::Result<PathBuf> {
        if let Ok(value) = std::env::var(GROK_BIN_ENV) {
            let path = PathBuf::from(value.trim());
            if !path.as_os_str().is_empty() && path.is_file() {
                return Ok(path);
            }
            anyhow::bail!("{GROK_BIN_ENV} does not point to an executable grok binary");
        }
        if let Ok(path) = which::which("grok") {
            return Ok(path);
        }
        if let Some(home) = Self::home_dir() {
            let path = home.join("bin").join("grok");
            if path.is_file() {
                return Ok(path);
            }
        }
        anyhow::bail!("{GROK_WRITE_REFUSAL}");
    }

    /// Root directory for Grok Build data. Respects the `GROK_HOME` env
    /// override (documented by the CLI), otherwise defaults to `~/.grok`.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("GROK_HOME") {
            let trimmed = home.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
        dirs::home_dir().map(|h| h.join(".grok"))
    }

    /// The sessions root: `<home>/sessions`.
    fn sessions_root() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("sessions"))
    }

    /// Enumerate `(session_id, updates.jsonl path)` for every session
    /// directory under the configured sessions root.
    fn list_all_sessions() -> SessionListing {
        let Some(root) = Self::sessions_root() else {
            return SessionListing::default();
        };
        list_sessions_in(&root)
    }
}

/// Enumerate `(session_id, updates.jsonl path)` under a sessions root.
///
/// Layout: `<root>/<percent-encoded-cwd>/<session-uuid>/updates.jsonl`.
/// Group-level files (`prompt_history.jsonl`) and root-level files
/// (`session_search.sqlite`) are ignored because only directories two levels
/// deep containing an `updates.jsonl` qualify.
fn list_sessions_in(root: &Path) -> SessionListing {
    let mut listing = SessionListing::default();
    for group in read_dir_reporting(root, &mut listing.unreadable) {
        let group_path = group.path();
        if !group_path.is_dir() {
            continue; // e.g. session_search.sqlite
        }
        for session in read_dir_reporting(&group_path, &mut listing.unreadable) {
            let session_dir = session.path();
            if !session_dir.is_dir() {
                continue; // e.g. prompt_history.jsonl
            }
            let updates = session_dir.join("updates.jsonl");
            if !updates.is_file() {
                continue;
            }
            let Some(id) = session_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            listing.sessions.push((id.to_string(), updates));
        }
    }
    listing
}

/// Decode a percent-encoded cwd group directory name back to a path.
///
/// Prefers a `.cwd` file inside the group directory (written by the CLI when
/// the encoded name would exceed 255 bytes), then percent-decoding the name.
fn decode_group_cwd(group_dir: &Path) -> Option<PathBuf> {
    let cwd_file = group_dir.join(".cwd");
    if cwd_file.is_file()
        && let Ok(content) = std::fs::read_to_string(&cwd_file)
    {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let name = group_dir.file_name()?.to_str()?;
    let decoded = urlencoding::decode(name).ok()?;
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded.into_owned()))
}

fn group_dir_name(cwd: &Path) -> (String, bool) {
    let encoded = urlencoding::encode(&cwd.to_string_lossy()).into_owned();
    if encoded.len() <= 255 {
        return (encoded, false);
    }

    // Grok keeps a readable final-component slug and disambiguates it with
    // BLAKE3 over the original path bytes. `.cwd` below makes the mapping
    // lossless for readers and for paths whose final component is empty.
    let slug = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "-_.".contains(ch) {
                ch
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    let digest = blake3::hash(cwd.to_string_lossy().as_bytes());
    (format!("{slug}-{}", &digest.to_hex()[..16]), true)
}

fn timestamp_string(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn epoch_seconds(timestamp: Option<i64>) -> i64 {
    timestamp
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
        .div_euclid(1000)
}

fn ensure_group_cwd(group_dir: &Path, workspace: &Path) -> anyhow::Result<()> {
    let marker = group_dir.join(".cwd");
    let workspace_text = workspace.to_string_lossy();
    match std::fs::read_to_string(&marker) {
        Ok(existing) if existing.trim() == workspace_text => return Ok(()),
        Ok(_) => anyhow::bail!(
            "Grok workspace marker {} belongs to a different path",
            marker.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read Grok workspace marker {}", marker.display())
            });
        }
    }

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(&marker).with_context(|| {
                format!("failed to read Grok workspace marker {}", marker.display())
            })?;
            if existing.trim() == workspace_text {
                return Ok(());
            }
            anyhow::bail!(
                "Grok workspace marker {} belongs to a different path",
                marker.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to create Grok workspace marker {}",
                    marker.display()
                )
            });
        }
    };
    file.write_all(workspace_text.as_bytes())
        .with_context(|| format!("failed to write Grok workspace marker {}", marker.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync Grok workspace marker {}", marker.display()))
}

/// The kind of streaming message currently being accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    User,
    Thought,
    Agent,
}

/// Extract the best-precision timestamp (epoch millis) from an update line.
///
/// Prefers `_meta.agentTimestampMs` (params-level, then update-level), then
/// the outer envelope `timestamp` (epoch seconds).
fn line_timestamp(line: &serde_json::Value, update: &serde_json::Value) -> Option<i64> {
    line.pointer("/params/_meta/agentTimestampMs")
        .and_then(parse_timestamp)
        .or_else(|| {
            update
                .pointer("/_meta/agentTimestampMs")
                .and_then(parse_timestamp)
        })
        .or_else(|| line.get("timestamp").and_then(parse_timestamp))
}

/// Flatten an ACP `ToolCallContent` array (from `tool_call` /
/// `tool_call_update` `content`) into a display string.
///
/// Variants per the ACP schema: `{"type":"content","content":<ContentBlock>}`,
/// `{"type":"diff","path":…,…}`, `{"type":"terminal",…}`.
fn flatten_tool_content(value: &serde_json::Value) -> String {
    let Some(items) = value.as_array() else {
        return flatten_content(value);
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("content") => {
                if let Some(block) = item.get("content") {
                    let text = flatten_content(block);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            Some("diff") => {
                let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("file");
                parts.push(format!("[Diff: {path}]"));
            }
            Some("terminal") => parts.push("[Terminal output]".to_string()),
            _ => {
                let text = flatten_content(item);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        }
    }
    parts.join("\n")
}

/// Render a tool result string from a `tool_call` / `tool_call_update` event:
/// prefer `content`, fall back to `rawOutput`.
fn tool_output_text(update: &serde_json::Value) -> Option<String> {
    if let Some(content) = update.get("content") {
        let text = flatten_tool_content(content);
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    match update.get("rawOutput") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            let text = flatten_content(other);
            if !text.trim().is_empty() {
                Some(text)
            } else {
                serde_json::to_string(other).ok()
            }
        }
    }
}

/// Streaming parser state that folds ACP session-update events into
/// canonical messages, coalescing consecutive same-kind chunks.
#[derive(Default)]
struct MessageBuilder {
    messages: Vec<CanonicalMessage>,
    /// Kind of the message currently being extended (None after a break).
    last_kind: Option<ChunkKind>,
    /// `toolCallId` → index into `messages` holding that tool call.
    tool_call_owner: std::collections::HashMap<String, usize>,
    /// Private writer marker → canonical message index. Native Grok sessions
    /// do not carry this marker and continue using streaming coalescing.
    imported_messages: std::collections::HashMap<usize, usize>,
}

impl MessageBuilder {
    fn push_new(&mut self, kind: ChunkKind, msg: CanonicalMessage) {
        self.messages.push(msg);
        self.last_kind = Some(kind);
    }

    /// Append a streaming chunk, coalescing with the previous message when it
    /// has the same kind.
    fn push_chunk(
        &mut self,
        kind: ChunkKind,
        text: &str,
        ts: Option<i64>,
        author: Option<String>,
        imported_index: Option<usize>,
    ) {
        if let Some(imported_index) = imported_index
            && let Some(&message_index) = self.imported_messages.get(&imported_index)
        {
            let message = &mut self.messages[message_index];
            message.content.push_str(text);
            if message.timestamp.is_none() {
                message.timestamp = ts;
            }
            if message.author.is_none() {
                message.author = author;
            }
            self.last_kind = Some(kind);
            return;
        }
        if imported_index.is_none()
            && self.last_kind == Some(kind)
            && let Some(last) = self.messages.last_mut()
        {
            last.content.push_str(text);
            if last.timestamp.is_none() {
                last.timestamp = ts;
            }
            if last.author.is_none() {
                last.author = author;
            }
            return;
        }
        let role = match kind {
            ChunkKind::User => MessageRole::User,
            ChunkKind::Thought | ChunkKind::Agent => MessageRole::Assistant,
        };
        let author = match kind {
            ChunkKind::Thought => author.or_else(|| Some("reasoning".to_string())),
            _ => author,
        };
        let extra = serde_json::json!({ "sessionUpdate": chunk_kind_label(kind) });
        self.push_new(
            kind,
            CanonicalMessage {
                idx: 0,
                role,
                content: text.to_string(),
                timestamp: ts,
                author,
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                extra,
            },
        );
        if let Some(imported_index) = imported_index {
            self.imported_messages
                .insert(imported_index, self.messages.len() - 1);
        }
    }

    /// Record a `tool_call` event. Attaches to the in-progress assistant
    /// message when one is open, otherwise starts a new assistant message.
    fn push_tool_call(
        &mut self,
        update: &serde_json::Value,
        ts: Option<i64>,
        imported_index: Option<usize>,
    ) {
        let call = ToolCall {
            id: update
                .get("toolCallId")
                .and_then(|v| v.as_str())
                .map(String::from),
            name: update
                .get("title")
                .and_then(|v| v.as_str())
                .or_else(|| update.get("kind").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string(),
            arguments: update
                .get("rawInput")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        };
        let call_id = call.id.clone();

        let target_idx = imported_index
            .and_then(|index| self.imported_messages.get(&index).copied())
            .or_else(|| {
                if imported_index.is_none()
                    && self.last_kind == Some(ChunkKind::Agent)
                    && !self.messages.is_empty()
                {
                    Some(self.messages.len() - 1)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                self.push_new(
                    ChunkKind::Agent,
                    CanonicalMessage {
                        idx: 0,
                        role: MessageRole::Assistant,
                        content: String::new(),
                        timestamp: ts,
                        author: None,
                        tool_calls: Vec::new(),
                        tool_results: Vec::new(),
                        extra: serde_json::json!({ "sessionUpdate": "tool_call" }),
                    },
                );
                self.messages.len() - 1
            });
        if let Some(imported_index) = imported_index {
            self.imported_messages
                .entry(imported_index)
                .or_insert(target_idx);
        }

        self.messages[target_idx].tool_calls.push(call);
        if let Some(id) = call_id {
            self.tool_call_owner.insert(id, target_idx);
        }

        // A completed tool_call event may already carry output.
        self.apply_tool_outcome(target_idx, update);
    }

    /// Record a `tool_call_update` event: merge into the owning message's tool
    /// call, and surface output as a tool result when present. Updates for
    /// unknown call ids (e.g. a truncated log) create a fresh tool call so the
    /// activity is not silently dropped.
    fn push_tool_call_update(
        &mut self,
        update: &serde_json::Value,
        ts: Option<i64>,
        imported_index: Option<usize>,
    ) {
        let call_id = update.get("toolCallId").and_then(|v| v.as_str());
        let owner = call_id.and_then(|id| self.tool_call_owner.get(id).copied());
        match owner {
            Some(idx) => {
                // Merge late-arriving fields into the recorded call.
                if let Some(id) = call_id
                    && let Some(call) = self.messages[idx]
                        .tool_calls
                        .iter_mut()
                        .find(|c| c.id.as_deref() == Some(id))
                {
                    if let Some(title) = update.get("title").and_then(|v| v.as_str())
                        && !title.is_empty()
                    {
                        call.name = title.to_string();
                    }
                    if let Some(raw_input) = update.get("rawInput")
                        && !raw_input.is_null()
                    {
                        call.arguments = raw_input.clone();
                    }
                }
                self.apply_tool_outcome(idx, update);
            }
            None => self.push_tool_call(update, ts, imported_index),
        }
    }

    /// If the event carries output (or a failed status), attach a tool result
    /// to the message at `idx`.
    fn apply_tool_outcome(&mut self, idx: usize, update: &serde_json::Value) {
        let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let is_error = status == "failed";
        let output = tool_output_text(update);
        if output.is_none() && !is_error {
            return;
        }
        let call_id = update
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .map(String::from);
        let msg = &mut self.messages[idx];
        // Replace an earlier partial result for the same call (streamed
        // tool_call_update events supersede one another).
        msg.tool_results
            .retain(|r| r.call_id.is_none() || r.call_id != call_id);
        msg.tool_results.push(ToolResult {
            call_id,
            content: output.unwrap_or_default(),
            is_error,
        });
    }

    /// Finish: drop messages that carry no content and no tool activity.
    fn finish(mut self) -> Vec<CanonicalMessage> {
        self.messages.retain(|m| {
            !m.content.trim().is_empty() || !m.tool_calls.is_empty() || !m.tool_results.is_empty()
        });
        reindex_messages(&mut self.messages);
        self.messages
    }
}

fn chunk_kind_label(kind: ChunkKind) -> &'static str {
    match kind {
        ChunkKind::User => "user_message_chunk",
        ChunkKind::Thought => "agent_thought_chunk",
        ChunkKind::Agent => "agent_message_chunk",
    }
}

/// Resolve the path casr was handed to the session's `updates.jsonl`.
///
/// Accepts the `updates.jsonl` itself, a sibling session file
/// (`summary.json`, `chat_history.jsonl`, …), or the session directory.
fn resolve_updates_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.join("updates.jsonl");
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some("updates.jsonl") => path.to_path_buf(),
        Some(_) => path
            .parent()
            .map(|p| p.join("updates.jsonl"))
            .unwrap_or_else(|| path.to_path_buf()),
        None => path.to_path_buf(),
    }
}

fn update_envelope(session_id: &str, update: serde_json::Value, timestamp: Option<i64>) -> String {
    let event_id = uuid::Uuid::new_v4().to_string();
    serde_json::json!({
        "timestamp": epoch_seconds(timestamp),
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
            "_meta": {
                "eventId": event_id,
                "agentTimestampMs": timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
            }
        }
    })
    .to_string()
}

fn tool_content(text: &str) -> serde_json::Value {
    serde_json::json!([{
        "type": "content",
        "content": {"type": "text", "text": text}
    }])
}

fn build_updates(session: &CanonicalSession, target_id: &str) -> anyhow::Result<Vec<String>> {
    if session.messages.is_empty() {
        anyhow::bail!("cannot write an empty Grok session");
    }

    let model = GROK_MODEL;
    let mut updates = Vec::new();
    let mut prompt_index = 0usize;
    let mut emitted_calls = std::collections::HashSet::new();

    for message in &session.messages {
        let timestamp = message.timestamp.or(session.started_at);
        let is_user = matches!(
            message.role,
            MessageRole::User | MessageRole::System | MessageRole::Other(_)
        );
        let is_reasoning = message.author.as_deref() == Some("reasoning");
        let kind = if is_user {
            "user_message_chunk"
        } else if is_reasoning {
            "agent_thought_chunk"
        } else {
            "agent_message_chunk"
        };
        let content_is_structural_result = matches!(message.role, MessageRole::Tool)
            && message
                .tool_results
                .iter()
                .any(|result| result.content == message.content);
        if !message.content.is_empty() && !content_is_structural_result {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "modelId".into(),
                serde_json::Value::String(model.to_string()),
            );
            meta.insert(
                "agsMessageIndex".into(),
                serde_json::Value::Number((message.idx as u64).into()),
            );
            if is_user {
                meta.insert(
                    "promptIndex".into(),
                    serde_json::Value::Number((prompt_index as u64).into()),
                );
            } else {
                meta.insert(
                    "promptIndex".into(),
                    serde_json::Value::Number(prompt_index.saturating_sub(1).into()),
                );
                meta.insert(
                    "messageIndex".into(),
                    serde_json::Value::Number((message.idx as u64).into()),
                );
            }
            updates.push(update_envelope(
                target_id,
                serde_json::json!({
                    "sessionUpdate": kind,
                    "content": {"type": "text", "text": message.content},
                    "_meta": meta
                }),
                timestamp,
            ));
        }
        if is_user {
            prompt_index = prompt_index.saturating_add(1);
        }

        for (call_index, call) in message.tool_calls.iter().enumerate() {
            let call_id = call
                .id
                .clone()
                .unwrap_or_else(|| format!("ags-{target_id}-{}-{call_index}", message.idx));
            emitted_calls.insert(call_id.clone());
            updates.push(update_envelope(
                target_id,
                serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": call.name,
                    "kind": "function",
                    "status": "in_progress",
                    "rawInput": call.arguments,
                    "_meta": {"agsMessageIndex": message.idx}
                }),
                timestamp,
            ));
        }

        for (result_index, result) in message.tool_results.iter().enumerate() {
            let call_id = result
                .call_id
                .clone()
                .or_else(|| {
                    message
                        .tool_calls
                        .get(result_index)
                        .and_then(|call| call.id.clone())
                })
                .unwrap_or_else(|| format!("ags-{target_id}-{}-{result_index}", message.idx));
            if emitted_calls.insert(call_id.clone()) {
                updates.push(update_envelope(
                    target_id,
                    serde_json::json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id.clone(),
                        "title": "Imported tool result",
                        "kind": "function",
                        "status": "in_progress",
                        "rawInput": serde_json::Value::Null,
                        "_meta": {"agsMessageIndex": message.idx}
                    }),
                    timestamp,
                ));
            }
            updates.push(update_envelope(
                target_id,
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": if result.is_error { "failed" } else { "completed" },
                    "content": tool_content(&result.content),
                    "rawOutput": result.content,
                    "_meta": {"agsMessageIndex": message.idx}
                }),
                timestamp,
            ));
        }
    }
    Ok(updates)
}

fn build_summary(
    session: &CanonicalSession,
    target_id: &str,
    workspace: &Path,
    update_count: usize,
) -> serde_json::Value {
    let title = session
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            session
                .messages
                .iter()
                .find(|message| matches!(message.role, MessageRole::User))
                .map(|message| message.content.as_str())
        })
        .map(|title| truncate_title(title, 100))
        .unwrap_or_else(|| "Untitled session".to_string());
    let created = session.started_at.or_else(|| {
        session
            .messages
            .iter()
            .filter_map(|message| message.timestamp)
            .min()
    });
    let updated = session
        .ended_at
        .or_else(|| {
            session
                .messages
                .iter()
                .filter_map(|message| message.timestamp)
                .max()
        })
        .or(created);
    let mut summary = serde_json::json!({
        "info": {"id": target_id, "cwd": workspace.to_string_lossy()},
        "session_summary": title,
        "generated_title": title,
        "created_at": timestamp_string(created),
        "updated_at": timestamp_string(updated),
        "last_active_at": timestamp_string(updated),
        "num_messages": update_count,
        "num_chat_messages": session.messages.len(),
        "current_model_id": GROK_MODEL,
        "chat_format_version": 1,
        "agent_name": "grok-build",
        "sandbox_profile": "off"
    });
    if let Some(parent) = session
        .metadata
        .get("parent_session_id")
        .and_then(|value| value.as_str())
    {
        summary["parent_session_id"] = serde_json::Value::String(parent.to_string());
    }
    summary
}

impl Provider for Grok {
    fn name(&self) -> &str {
        "Grok Build"
    }

    fn slug(&self) -> &str {
        SLUG
    }

    fn cli_alias(&self) -> &str {
        "grk"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if let Ok(value) = std::env::var(GROK_BIN_ENV) {
            let path = PathBuf::from(value.trim());
            if path.is_file() {
                evidence.push(format!("{GROK_BIN_ENV} points to {}", path.display()));
                installed = true;
            }
        }
        if which::which("grok").is_ok() {
            evidence.push("grok binary found in PATH".to_string());
            installed = true;
        }

        if let Some(home) = Self::home_dir() {
            // The official installer places the binary at ~/.grok/bin/grok.
            let bin = home.join("bin").join("grok");
            if bin.is_file() {
                evidence.push(format!("{} exists", bin.display()));
                installed = true;
            }
            let sessions = home.join("sessions");
            if sessions.is_dir() {
                evidence.push(format!("sessions directory found: {}", sessions.display()));
                installed = true;
            }
        }

        // The installer creates `~/.grok/bin/grok`; `~/.grok/sessions` is what
        // `list` reads and it appears only after the first session.
        if installed && let Some(sessions) = Self::sessions_root() {
            evidence.push(store_evidence(&sessions));
        }

        trace!(provider = SLUG, ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let Some(root) = Self::sessions_root() else {
            return vec![];
        };
        if root.is_dir() { vec![root] } else { vec![] }
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        Some(Self::list_all_sessions())
    }

    /// Every Grok transcript is `<group>/<session-uuid>/updates.jsonl`. The
    /// filename is fixed, which is what keeps `prompt_history.jsonl` (one level
    /// up) and `session_search.sqlite` (two levels up) out of the listing.
    fn is_session_path(&self, path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()) == Some("updates.jsonl")
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let root = Self::sessions_root()?;
        if !root.is_dir() {
            return None;
        }

        // Fast path: direct `<group>/<session_id>/updates.jsonl` probe.
        for group in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let group_path = group.path();
            if !group_path.is_dir() {
                continue;
            }
            let candidate = group_path.join(session_id).join("updates.jsonl");
            if candidate.is_file() {
                debug!(path = %candidate.display(), session_id, "found Grok session");
                return Some(candidate);
            }
        }

        // Case-insensitive fallback (UUIDs are conventionally lowercase).
        let lc = session_id.to_ascii_lowercase();
        for (id, path) in Self::list_all_sessions().sessions {
            if id.to_ascii_lowercase() == lc {
                debug!(path = %path.display(), session_id, "found Grok session (case-insensitive)");
                return Some(path);
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Grok session");

        let updates_path = resolve_updates_path(path);
        let session_dir = updates_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("cannot determine Grok session directory"))?;

        // ------------------------------------------------------------------
        // summary.json → metadata
        // ------------------------------------------------------------------
        let summary: Option<serde_json::Value> =
            std::fs::read_to_string(session_dir.join("summary.json"))
                .ok()
                .and_then(|c| serde_json::from_str(&c).ok());

        let session_id = summary
            .as_ref()
            .and_then(|s| s.pointer("/info/id"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                session_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "unknown".to_string());

        let workspace = summary
            .as_ref()
            .and_then(|s| s.pointer("/info/cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .or_else(|| session_dir.parent().and_then(decode_group_cwd));

        let model_name = summary
            .as_ref()
            .and_then(|s| s.get("current_model_id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        // ------------------------------------------------------------------
        // updates.jsonl → messages
        // ------------------------------------------------------------------
        let mut builder = MessageBuilder::default();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        if updates_path.is_file() {
            let content = std::fs::read_to_string(&updates_path)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", updates_path.display()))?;

            for (i, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(val): Result<serde_json::Value, _> = serde_json::from_str(line) else {
                    trace!(line = i, "skipping malformed Grok updates line");
                    continue;
                };

                // Envelope: {"params":{"update":{…}}}; tolerate a bare
                // {"update":{…}} or a naked SessionUpdate object.
                let update = val
                    .pointer("/params/update")
                    .or_else(|| val.get("update"))
                    .unwrap_or(&val);
                let Some(kind) = update.get("sessionUpdate").and_then(|v| v.as_str()) else {
                    continue;
                };

                let ts = line_timestamp(&val, update);
                // The old spelling is still read because it is on disk: this
                // marker is written into the converted transcript, so every
                // Grok session converted before the rename carries it.
                let imported_index = update
                    .pointer("/_meta/agsMessageIndex")
                    .or_else(|| update.pointer("/_meta/agsxMessageIndex"))
                    .and_then(|value| value.as_u64())
                    .map(|value| value as usize);
                if let Some(t) = ts {
                    started_at = Some(started_at.map_or(t, |s: i64| s.min(t)));
                    ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
                }

                match kind {
                    "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
                        let chunk_kind = match kind {
                            "user_message_chunk" => ChunkKind::User,
                            "agent_thought_chunk" => ChunkKind::Thought,
                            _ => ChunkKind::Agent,
                        };
                        let text = update
                            .get("content")
                            .map(flatten_content)
                            .unwrap_or_default();
                        // Model id may ride in update-level _meta.
                        let author = if chunk_kind == ChunkKind::Agent {
                            update
                                .pointer("/_meta/modelId")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .or_else(|| model_name.clone())
                        } else {
                            None
                        };
                        if text.is_empty() {
                            continue;
                        }
                        builder.push_chunk(chunk_kind, &text, ts, author, imported_index);
                    }
                    "tool_call" => builder.push_tool_call(update, ts, imported_index),
                    "tool_call_update" => builder.push_tool_call_update(update, ts, imported_index),
                    // Known non-conversation kinds (plan/TODO state, hook runs,
                    // retry telemetry) and any future/unknown kinds are skipped.
                    _ => {
                        trace!(kind, line = i, "skipping non-conversation Grok update");
                    }
                }
            }
        } else {
            debug!(
                updates = %updates_path.display(),
                "no updates.jsonl found; Grok session has no readable transcript"
            );
        }

        let messages = builder.finish();

        // ------------------------------------------------------------------
        // Metadata assembly
        // ------------------------------------------------------------------
        let title = summary
            .as_ref()
            .and_then(|s| s.get("generated_title"))
            .and_then(|v| v.as_str())
            .filter(|t| !t.trim().is_empty())
            .map(String::from)
            .or_else(|| {
                summary
                    .as_ref()
                    .and_then(|s| s.get("session_summary"))
                    .and_then(|v| v.as_str())
                    .filter(|t| !t.trim().is_empty())
                    .map(String::from)
            })
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.content, 100))
            });

        // Summary timestamps take precedence over message-derived bounds.
        if let Some(s) = summary.as_ref() {
            if let Some(t) = s.get("created_at").and_then(parse_timestamp) {
                started_at = Some(t);
            }
            if let Some(t) = s
                .get("last_active_at")
                .and_then(parse_timestamp)
                .or_else(|| s.get("updated_at").and_then(parse_timestamp))
            {
                ended_at = Some(t);
            }
        }

        let mut metadata = serde_json::Map::new();
        metadata.insert("source".into(), serde_json::Value::String(SLUG.into()));
        metadata.insert(
            "sessionId".into(),
            serde_json::Value::String(session_id.clone()),
        );
        if let Some(s) = summary.as_ref() {
            if let Some(t) = s.get("generated_title").and_then(|v| v.as_str())
                && !t.trim().is_empty()
            {
                metadata.insert(
                    crate::model::NATIVE_NAME_META_KEY.into(),
                    serde_json::Value::String(t.to_string()),
                );
            }
            // An allow-list, and the only thing `summary.json` contributes to
            // the metadata bag.
            //
            // This used to be followed by `metadata.insert("summary", s)` — the
            // whole file — under the comment "preserve the full summary for
            // round-trip fidelity". Both halves of that were wrong.
            //
            // The canonical metadata bag is read back in exactly three places:
            // `native_name_from_metadata`, the
            // `InfoResponse` in `main.rs` (which *prints* it), and the
            // per-provider writers, of which only Gemini (`projectHash`), Pi
            // (`provider`) and Kiro (`session_state`/`history`) consult a key
            // at all. Grok's writer below deliberately creates fresh metadata
            // from canonical fields rather than copying this source summary.
            //
            // And the file is not inert. `summary.json` is 33 fields in grok
            // 0.2.103 — recovered from the session-persistence serializer in
            // the shipped `@xai-official/grok-linux-x64` binary (brotli
            // `bin/grok.br`; the field names are emitted in declaration order
            // and cross-check against the `persistence.rs` and `acp_agent.rs`
            // string pools and against the CLI's own bundled
            // `docs/user-guide/17-sessions.md`). One of them is `git_remotes`,
            // an array of the workspace's git remote URLs. An HTTPS remote
            // routinely embeds a credential —
            // `https://x-access-token:ghp_…@github.com/…` — and grok's own
            // secret scrubber (`xai-grok-secrets`, which knows `ghp_`,
            // `glpat-`, `xai-`, `AKIA`, JWTs…) is wired to its sampler and
            // telemetry paths, not to session persistence. So the value lands
            // in `summary.json` unredacted, and the wholesale copy put it into
            // `casr info --json` — a command users pipe to a file and paste
            // into issues. `grok_home`, `info.cwd`, `prompt_display_cwd`,
            // `source_workspace_dir` and `git_root_dir` rode along with it.
            //
            // 0.2.112 already adds fields 0.2.103 does not have, so the set is
            // not stable even across patch releases. That is the argument for
            // naming what we take rather than taking the file: `generated_title`
            // above and these three are ours to justify, the rest are xAI's to
            // change.
            for key in ["agent_name", "sandbox_profile", "parent_session_id"] {
                if let Some(v) = s.get(key)
                    && !v.is_null()
                {
                    metadata.insert(key.into(), v.clone());
                }
            }
        }
        if let Some(m) = model_name.as_ref() {
            metadata.insert("model".into(), serde_json::Value::String(m.clone()));
        }

        info!(session_id, messages = messages.len(), "Grok session parsed");

        Ok(CanonicalSession {
            session_id,
            provider_slug: SLUG.to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name,
        })
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let binary = Self::binary_path().map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let workspace = session
            .workspace
            .clone()
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .context("could not determine a Grok workspace")?;
        let mut warnings = Vec::new();
        if session
            .workspace
            .as_ref()
            .is_some_and(|path| !path.is_dir())
        {
            warnings.push(format!(
                "The source workspace {} does not exist; Grok will resume in {}.",
                session.workspace.as_ref().unwrap().display(),
                workspace.display()
            ));
        }

        let root = Self::sessions_root().context("could not determine Grok session store")?;
        let (group_name, shortened) = group_dir_name(&workspace);
        let group_dir = root.join(group_name);
        let target_id = uuid::Uuid::new_v4().to_string();
        let session_dir = group_dir.join(&target_id);
        if session_dir.exists() {
            anyhow::bail!("Grok generated session ID already exists: {target_id}");
        }
        std::fs::create_dir_all(&session_dir).with_context(|| {
            format!(
                "failed to create Grok session directory {}",
                session_dir.display()
            )
        })?;
        let cleanup = |error: anyhow::Error| -> anyhow::Result<WrittenSession> {
            let _ = std::fs::remove_dir_all(&session_dir);
            Err(error)
        };

        if shortened && let Err(error) = ensure_group_cwd(&group_dir, &workspace) {
            return cleanup(error);
        }

        let updates = match build_updates(session, &target_id) {
            Ok(updates) => updates,
            Err(error) => return cleanup(error),
        };
        let summary = build_summary(session, &target_id, &workspace, updates.len());
        let summary_bytes = match serde_json::to_vec_pretty(&summary) {
            Ok(bytes) => bytes,
            Err(error) => {
                return cleanup(anyhow::anyhow!("failed to serialize Grok summary: {error}"));
            }
        };
        let updates_bytes = format!("{}\n", updates.join("\n")).into_bytes();

        let summary_path = session_dir.join("summary.json");
        let updates_path = session_dir.join("updates.jsonl");
        let summary_outcome = match crate::pipeline::atomic_write(
            &summary_path,
            &summary_bytes,
            opts.force,
            self.slug(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => return cleanup(anyhow::anyhow!("{error}")),
        };
        let updates_outcome = match crate::pipeline::atomic_write(
            &updates_path,
            &updates_bytes,
            opts.force,
            self.slug(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = crate::pipeline::restore_backup(&summary_outcome, self.slug());
                return cleanup(anyhow::anyhow!("{error}"));
            }
        };

        // The CLI is the vendor-side parser and discovery oracle. `export`
        // reads the newly written authoritative log without invoking a model.
        let export = Command::new(&binary)
            .arg("export")
            .arg(&target_id)
            .current_dir(&workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("failed to run {}", binary.display()));
        match export {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let _ = crate::pipeline::restore_backup(&updates_outcome, self.slug());
                let _ = crate::pipeline::restore_backup(&summary_outcome, self.slug());
                return cleanup(anyhow::anyhow!(
                    "Grok rejected generated session {target_id} during official export (exit {})",
                    status
                ));
            }
            Err(error) => {
                let _ = crate::pipeline::restore_backup(&updates_outcome, self.slug());
                let _ = crate::pipeline::restore_backup(&summary_outcome, self.slug());
                return cleanup(error);
            }
        }

        info!(
            target_session_id = %target_id,
            path = %updates_path.display(),
            messages = session.messages.len(),
            "Grok session written and accepted by official export"
        );
        Ok(WrittenSession {
            paths: vec![updates_path, summary_path],
            session_id: target_id.clone(),
            resume_command: self.resume_command(&target_id),
            backups: summary_outcome
                .displaced()
                .into_iter()
                .chain(updates_outcome.displaced())
                .collect(),
            warnings,
        })
    }

    fn rollback_write(&self, written: &WrittenSession) -> anyhow::Result<()> {
        let updates = written
            .paths
            .iter()
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("updates.jsonl"))
            .context("Grok rollback has no updates.jsonl path")?;
        let session_dir = updates
            .parent()
            .context("Grok rollback cannot determine session directory")?;
        let id = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .context("Grok rollback session directory has no UTF-8 ID")?;
        let root = Self::sessions_root().context("Grok rollback cannot determine session store")?;
        let path_root = session_dir.parent().and_then(Path::parent);
        if id != written.session_id || path_root != Some(root.as_path()) {
            anyhow::bail!("Grok rollback path does not match generated session ID");
        }
        let binary = Self::binary_path().ok();
        if let Some(binary) = binary {
            let output = Command::new(binary)
                .args(["sessions", "delete", &written.session_id])
                .output();
            if let Ok(output) = output
                && output.status.success()
                && !session_dir.exists()
            {
                return Ok(());
            }
        }
        match std::fs::remove_dir_all(session_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove unverified Grok session {}",
                        session_dir.display()
                    )
                });
            }
        }
        Ok(())
    }

    fn write_refusal(&self) -> Option<&'static str> {
        Self::binary_path().is_err().then_some(GROK_WRITE_REFUSAL)
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("grok --resume {session_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    const SESSION_ID: &str = "019f75d0-ebe1-7db0-a59a-60af9ffa9e71";
    const ENCODED_CWD: &str = "%2Fdata%2Fprojects%2Fdemo";

    /// Wrap a `SessionUpdate` object in the empirical line envelope.
    fn envelope(update: serde_json::Value, ts_secs: i64) -> String {
        serde_json::to_string(&json!({
            "timestamp": ts_secs,
            "method": "session/update",
            "params": {
                "sessionId": SESSION_ID,
                "update": update,
                "_meta": {"eventId": format!("{SESSION_ID}-1"), "agentTimestampMs": ts_secs * 1000}
            }
        }))
        .unwrap()
    }

    /// Build `<root>/sessions/<encoded-cwd>/<uuid>/` with updates + summary.
    fn make_grok_tree(
        lines: &[String],
        summary: Option<serde_json::Value>,
    ) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let session_dir = tmp
            .path()
            .join("sessions")
            .join(ENCODED_CWD)
            .join(SESSION_ID);
        std::fs::create_dir_all(&session_dir).expect("mkdirs");
        let updates = session_dir.join("updates.jsonl");
        std::fs::write(&updates, lines.join("\n")).expect("write updates");
        if let Some(s) = summary {
            std::fs::write(
                session_dir.join("summary.json"),
                serde_json::to_string_pretty(&s).unwrap(),
            )
            .expect("write summary");
        }
        (tmp, updates)
    }

    fn default_summary() -> serde_json::Value {
        json!({
            "info": {"id": SESSION_ID, "cwd": "/data/projects/demo"},
            "session_summary": "Run echo hi please",
            "created_at": "2026-07-18T15:20:54.103795421Z",
            "updated_at": "2026-07-18T15:20:59.770364197Z",
            "num_messages": 4,
            "num_chat_messages": 5,
            "current_model_id": "grok-build",
            "chat_format_version": 1,
            "grok_home": "/home/user/.grok",
            "last_active_at": "2026-07-18T15:20:59.770364197Z",
            "generated_title": "Echo test session",
            "agent_name": "grok-build-plan",
            "sandbox_profile": "off"
        })
    }

    // -----------------------------------------------------------------------
    // Reader tests
    // -----------------------------------------------------------------------

    #[test]
    fn reader_parses_real_user_message_chunk_shape() {
        // Verbatim structure captured from a real grok 0.2.103 session
        // (update-level _meta with modelId/promptIndex).
        let lines = vec![envelope(
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "Run the shell command: echo hi"},
                "_meta": {"modelId": "grok-build", "promptIndex": 0}
            }),
            1_784_388_059,
        )];
        let (_guard, updates) = make_grok_tree(&lines, Some(default_summary()));
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.provider_slug, "grok");
        assert_eq!(session.session_id, SESSION_ID);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(
            session.messages[0].content,
            "Run the shell command: echo hi"
        );
        assert!(session.messages[0].timestamp.is_some());
    }

    #[test]
    fn reader_coalesces_consecutive_same_kind_chunks() {
        let lines = vec![
            envelope(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Run echo hi"}}),
                100,
            ),
            envelope(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": " please"}}),
                101,
            ),
            envelope(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Running"}}),
                102,
            ),
            envelope(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": " it now."}}),
                103,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Run echo hi please");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Running it now.");
    }

    #[test]
    fn reader_thought_chunks_become_reasoning_messages() {
        let lines = vec![
            envelope(
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "The user wants echo."}}),
                100,
            ),
            envelope(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "On it."}}),
                101,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::Assistant);
        assert_eq!(session.messages[0].author.as_deref(), Some("reasoning"));
        assert_eq!(session.messages[0].content, "The user wants echo.");
        assert_eq!(session.messages[1].content, "On it.");
        assert_ne!(session.messages[1].author.as_deref(), Some("reasoning"));
    }

    #[test]
    fn reader_merges_tool_call_and_update_into_one_message() {
        let lines = vec![
            envelope(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Running echo."}}),
                100,
            ),
            envelope(
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_1",
                    "title": "Run echo hi",
                    "kind": "execute",
                    "status": "pending",
                    "rawInput": {"command": "echo hi"}
                }),
                101,
            ),
            envelope(
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_1",
                    "status": "completed",
                    "content": [
                        {"type": "content", "content": {"type": "text", "text": "hi\n"}}
                    ],
                    "rawOutput": {"output": "hi\n"}
                }),
                102,
            ),
            envelope(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": " Done: hi."}}),
                103,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        // Tool activity does not break the assistant turn.
        assert_eq!(session.messages.len(), 1);
        let msg = &session.messages[0];
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content, "Running echo. Done: hi.");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(msg.tool_calls[0].name, "Run echo hi");
        assert_eq!(msg.tool_calls[0].arguments["command"], "echo hi");
        assert_eq!(msg.tool_results.len(), 1);
        assert_eq!(msg.tool_results[0].call_id.as_deref(), Some("call_1"));
        assert_eq!(msg.tool_results[0].content, "hi\n");
        assert!(!msg.tool_results[0].is_error);
    }

    #[test]
    fn reader_marks_failed_tool_calls_as_errors() {
        let lines = vec![
            envelope(
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_9",
                    "title": "Read file",
                    "kind": "read",
                    "status": "pending"
                }),
                100,
            ),
            envelope(
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_9",
                    "status": "failed",
                    "rawOutput": "no such file"
                }),
                101,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.messages.len(), 1);
        let msg = &session.messages[0];
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_results.len(), 1);
        assert!(msg.tool_results[0].is_error);
        assert_eq!(msg.tool_results[0].content, "no such file");
    }

    #[test]
    fn reader_skips_extension_and_unknown_kinds_tolerantly() {
        // hook_execution and retry_state lines below are structurally verbatim
        // from a real 0.2.103 session (content sanitized).
        let lines = vec![
            envelope(
                json!({
                    "sessionUpdate": "hook_execution",
                    "event_name": "user_prompt_submit",
                    "prompt_id": "a23b02fe-95e5-46e4-93c0-c51b0400b88a",
                    "runs": [{"name": "global/settings:user_prompt_submit[0].hooks[0]",
                              "status": {"status": "success", "elapsed_ms": 388}}]
                }),
                100,
            ),
            envelope(
                json!({
                    "sessionUpdate": "retry_state",
                    "type": "failed",
                    "error_type": "api",
                    "message": "API error (status 404 Not Found)"
                }),
                101,
            ),
            envelope(json!({"sessionUpdate": "plan", "entries": []}), 102),
            envelope(
                json!({"sessionUpdate": "some_future_kind", "payload": {"x": 1}}),
                103,
            ),
            "not valid json {{{".to_string(),
            envelope(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Real message"}}),
                104,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Real message");
    }

    #[test]
    fn reader_summary_metadata_wins() {
        let lines = vec![envelope(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hello"}}),
            1_784_388_059,
        )];
        let (_guard, updates) = make_grok_tree(&lines, Some(default_summary()));
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.title.as_deref(), Some("Echo test session"));
        assert_eq!(session.model_name.as_deref(), Some("grok-build"));
        assert_eq!(
            session.workspace,
            Some(PathBuf::from("/data/projects/demo"))
        );
        assert_eq!(session.metadata["source"], "grok");
        assert_eq!(session.metadata["agent_name"], "grok-build-plan");
        assert_eq!(
            crate::model::native_name_from_metadata(&session.metadata).as_deref(),
            Some("Echo test session")
        );
        // created_at 2026-07-18T15:20:54Z / last_active_at 15:20:59Z.
        assert!(session.started_at.is_some());
        assert!(session.ended_at.unwrap() > session.started_at.unwrap());
    }

    #[test]
    fn reader_title_falls_back_to_session_summary_then_user_message() {
        let lines = vec![envelope(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "first user words"}}),
            100,
        )];

        // No generated_title → session_summary.
        let mut summary = default_summary();
        summary["generated_title"] = json!("");
        let (_guard, updates) = make_grok_tree(&lines, Some(summary));
        let session = Grok.read_session(&updates).expect("read");
        assert_eq!(session.title.as_deref(), Some("Run echo hi please"));

        // No summary.json at all → first user message.
        let (_guard2, updates2) = make_grok_tree(&lines, None);
        let session2 = Grok.read_session(&updates2).expect("read");
        assert_eq!(session2.title.as_deref(), Some("first user words"));
        // Without summary, the session id comes from the directory name.
        assert_eq!(session2.session_id, SESSION_ID);
    }

    #[test]
    fn reader_workspace_falls_back_to_percent_decoded_group_dir() {
        let lines = vec![envelope(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hi"}}),
            100,
        )];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");
        assert_eq!(
            session.workspace,
            Some(PathBuf::from("/data/projects/demo"))
        );
    }

    #[test]
    fn reader_accepts_summary_json_or_session_dir_path() {
        let lines = vec![envelope(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hi"}}),
            100,
        )];
        let (_guard, updates) = make_grok_tree(&lines, Some(default_summary()));
        let session_dir = updates.parent().unwrap().to_path_buf();

        let via_summary = Grok
            .read_session(&session_dir.join("summary.json"))
            .expect("read via summary.json");
        assert_eq!(via_summary.messages.len(), 1);

        let via_dir = Grok.read_session(&session_dir).expect("read via dir");
        assert_eq!(via_dir.messages.len(), 1);
    }

    #[test]
    fn reader_empty_updates_file() {
        let (_guard, updates) = make_grok_tree(&[String::new()], Some(default_summary()));
        let session = Grok.read_session(&updates).expect("read");
        assert_eq!(session.messages.len(), 0);
        // Metadata still available from summary.json.
        assert_eq!(session.session_id, SESSION_ID);
        assert_eq!(session.title.as_deref(), Some("Echo test session"));
    }

    #[test]
    fn reader_preserves_imported_boundaries_between_same_role_messages() {
        let lines = vec![
            envelope(
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "first"},
                    "_meta": {"agsMessageIndex": 0}
                }),
                100,
            ),
            envelope(
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "second"},
                    "_meta": {"agsMessageIndex": 1}
                }),
                101,
            ),
        ];
        let (_guard, updates) = make_grok_tree(&lines, None);
        let session = Grok.read_session(&updates).expect("read");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "first");
        assert_eq!(session.messages[1].content, "second");
    }

    #[test]
    fn long_workspace_group_matches_grok_slug_and_blake3_rule() {
        let workspace = PathBuf::from(format!(
            "/root/.agent-work/ags-grok-boundary-probe.ww9QFb/long/{}/{}/{}",
            "a".repeat(80),
            "b".repeat(80),
            "c".repeat(80)
        ));
        let (group, shortened) = group_dir_name(&workspace);

        assert!(shortened);
        assert_eq!(
            group,
            "cccccccccccccccccccccccccccccccccccccccc-ebbcdbab40fb44f4"
        );
    }

    // -----------------------------------------------------------------------
    // Enumeration
    // -----------------------------------------------------------------------

    #[test]
    fn list_sessions_in_enumerates_session_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let group = root.join(ENCODED_CWD);
        let session_dir = group.join(SESSION_ID);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("updates.jsonl"), "").unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        // Non-session artifacts that must be ignored.
        std::fs::write(root.join("session_search.sqlite"), b"SQLite format 3\x00").unwrap();
        std::fs::write(group.join("prompt_history.jsonl"), "{}").unwrap();
        // A session dir without updates.jsonl is skipped.
        std::fs::create_dir_all(group.join("no-updates-session")).unwrap();

        let listed = list_sessions_in(&root).sessions;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, SESSION_ID);
        assert!(listed[0].1.ends_with("updates.jsonl"));
    }

    #[test]
    fn decode_group_cwd_percent_decodes_and_prefers_cwd_file() {
        let tmp = tempfile::tempdir().unwrap();
        let group = tmp.path().join(ENCODED_CWD);
        std::fs::create_dir_all(&group).unwrap();
        assert_eq!(
            decode_group_cwd(&group),
            Some(PathBuf::from("/data/projects/demo"))
        );

        // A `.cwd` file (long-path escape hatch) wins over the dir name.
        let hashed = tmp.path().join("some-slug-abc123");
        std::fs::create_dir_all(&hashed).unwrap();
        std::fs::write(hashed.join(".cwd"), "/very/long/original/path\n").unwrap();
        assert_eq!(
            decode_group_cwd(&hashed),
            Some(PathBuf::from("/very/long/original/path"))
        );
    }

    // -----------------------------------------------------------------------
    // Provider metadata / missing-CLI refusal / resume
    // -----------------------------------------------------------------------

    #[test]
    fn provider_identity() {
        let p = Grok;
        assert_eq!(p.name(), "Grok Build");
        assert_eq!(p.slug(), "grok");
        assert_eq!(p.cli_alias(), "grk");
        assert_eq!(p.write_refusal(), Some(GROK_WRITE_REFUSAL));
    }

    #[test]
    fn resume_command_uses_documented_flag() {
        assert_eq!(
            Grok.resume_command(SESSION_ID),
            format!("grok --resume {SESSION_ID}")
        );
    }

    #[test]
    fn write_session_is_refused_without_official_cli() {
        let session = CanonicalSession {
            session_id: "x".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages: vec![],
            metadata: serde_json::Value::Null,
            source_path: PathBuf::from("/tmp/x"),
            model_name: None,
        };
        let err = Grok
            .write_session(&session, &WriteOptions { force: false })
            .expect_err("grok must refuse writes without its official CLI");
        assert_eq!(err.to_string(), GROK_WRITE_REFUSAL);
    }
}
