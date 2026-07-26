//! Structured IR → Claude Code transcript JSONL.
//!
//! The inverse of [`super::claude_code_ir`]. Every record this module emits is
//! a record that reader parses back into the events it came from, which is the
//! only definition of "correct" a writer can be held to;
//! `tests/roundtrip_ir_test.rs` holds it to exactly that.
//!
//! # What gets written
//!
//! [`SessionIr::model_visible`] and nothing else — the resolver has already
//! applied compaction, rollback and fork pruning, and re-deriving any of it
//! here would be a second answer to a question that already has one. On the
//! local corpus it takes 41,384 captured Claude model events down to 20,297.
//!
//! # Measured envelope
//!
//! Counted over 52 local transcripts on 2026-07-26: across 9,801 `user` and
//! 19,349 `assistant` records, twelve fields are on 100% of both —
//!
//! ```text
//! cwd  entrypoint  gitBranch  isSidechain  message  parentUuid
//! sessionId  timestamp  type  userType  uuid  version
//! ```
//!
//! — and `gitBranch`, `entrypoint` and `userType` are the three a naive writer
//! drops. Everything else is conditional and is written only when the IR
//! actually has it: `promptId` (89.8%), `toolUseResult` (86.3%),
//! `sourceToolAssistantUUID` (86.3%, and on tool-result records exclusively).
//!
//! `slug` (61–65%), `effort` (93.6% on assistant), `requestId`,
//! `isCompactSummary` (0.4%) and `isMeta` (1.4%) are *not* written: the IR does
//! not model them, and a fabricated value is worse than an absent one.
//!
//! # The chain, not the DAG
//!
//! `parentUuid` links each record to its predecessor. The resolver returns a
//! linear ordered context, so the writer emits a linear chain — first record
//! `parentUuid: null`, each subsequent one pointing at the last. The abandoned
//! branches it pruned were abandoned on purpose; reconstructing the original
//! DAG would put them back.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use crate::budget::ContextBudget;
use crate::ir::{
    Block, Body, Branch, Capsule, CapsuleFit, CapsuleKind, Event, Fidelity, Loss, LossKind, Role,
    SessionIr, ToolInput, ToolOutcome,
};

/// The vendor whose sealed blobs a Claude Code session may replay.
const TARGET_VENDOR: &str = "anthropic";

/// [`crate::ir::Origin::agent`] of a session this writer can replay natively.
const SAME_AGENT: &str = "claude-code";

/// `entrypoint` on 29,043 of 29,220 corpus records (the rest are `sdk-cli`).
const ENTRYPOINT: &str = "cli";

/// `userType` on 100% of corpus records.
const USER_TYPE: &str = "external";

/// Placed where a sealed compaction could not be carried across.
///
/// Codex's compacted history is 4,325 items and 87.6 MB across the corpus, and
/// for three quarters of rollouts it *is* the entire earlier conversation.
/// Omitting it silently is the worst option available: the target then answers
/// confidently as though it has the full history.
const SEALED_CONTEXT_MARKER: &str = "[converted by casr] The earlier part of this conversation was \
compacted by the source agent, which returned it sealed in a format only that \
agent's provider can read. It could not be transferred and is missing from this \
session. Do not assume the history above is complete — ask before relying on \
anything from before this point.";

/// A rendered transcript, with the grade the rendering earned.
pub struct Rendered {
    /// JSONL lines, without trailing newlines.
    pub lines: Vec<String>,
    /// The worst grade any part of this conversion earned.
    pub fidelity: Fidelity,
    /// Non-fatal notes about what could not be carried. Rendered from
    /// [`Rendered::losses`], never assembled separately.
    pub warnings: Vec<String>,
    /// What the grade is made of.
    pub losses: Vec<Loss>,
}

/// Render `ir` as a Claude Code transcript under `session_id`, inside `budget`.
///
/// `None` when the replay is empty; the caller falls back to the flat path
/// rather than writing a transcript with no conversation in it.
///
/// `budget` is applied to [`SessionIr::model_visible`] and to nothing else — the
/// resolved live context, not the captured event list, because trimming the
/// capture would "save" history the agent had already compacted away. Pass
/// [`ContextBudget::UNLIMITED`] for the pre-budget behaviour, which is
/// byte-identical rather than merely equivalent. Whatever the budget removes
/// arrives here as a [`Loss`] and is folded into the grade with the writer's
/// own; see [`crate::budget`].
pub fn render(
    ir: &SessionIr,
    session_id: &str,
    now: DateTime<Utc>,
    budget: &ContextBudget,
) -> Option<Rendered> {
    let budgeted = budget.apply(ir.model_visible());
    let visible = budgeted.as_events();
    if visible.is_empty() {
        return None;
    }

    let mut writer = Writer {
        same_agent: ir.origin.agent == SAME_AGENT,
        dropped_reasoning: 0,
        reasoning_bytes: 0,
        dropped_history: 0,
        history_bytes: 0,
        downgraded_calls: 0,
        recast_roles: 0,
        dropped_empty: 0,
    };

    let mut records: Vec<Record> = Vec::new();
    for event in &visible {
        let Some(part) = writer.part(event) else {
            continue;
        };
        match records.last_mut() {
            Some(last) if last.accepts(event, &part) => last.push(part),
            _ => records.push(Record::new(event, part)),
        }
    }
    if records.is_empty() {
        return None;
    }

    // Uuids are assigned before serialisation because a tool result names the
    // record that called it (`sourceToolAssistantUUID`, on 8,455 of 8,455
    // tool-result records), and that record is always an earlier one.
    let uuids: Vec<String> = records
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let mut caller_of: HashMap<String, String> = HashMap::new();
    for (record, uuid) in records.iter().zip(&uuids) {
        for call_id in &record.provides {
            caller_of.insert(call_id.clone(), uuid.clone());
        }
    }

    let now_iso = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let cwd = ir
        .workspace
        .cwd
        .as_deref()
        .unwrap_or(std::path::Path::new("/tmp"))
        .to_string_lossy()
        .to_string();
    let version = match (writer.same_agent, ir.origin.agent_version.as_deref()) {
        (true, Some(version)) => version.to_string(),
        _ => "casr".to_string(),
    };
    // The model names the *record's* author. Carrying the source agent's model
    // across would label a Codex turn as though Claude had produced it.
    let model = match (writer.same_agent, ir.origin.model.as_deref()) {
        (true, Some(model)) => model.to_string(),
        _ => "unknown".to_string(),
    };

    let mut lines = Vec::with_capacity(records.len());
    let mut parent: Option<&str> = None;
    for (record, uuid) in records.iter().zip(&uuids) {
        lines.push(
            record
                .render(Envelope {
                    uuid,
                    parent,
                    session_id,
                    cwd: &cwd,
                    version: &version,
                    model: &model,
                    git_branch: ir.workspace.git_branch.as_deref().unwrap_or(""),
                    now_iso: &now_iso,
                    caller_of: &caller_of,
                })
                .to_string(),
        );
        parent = Some(uuid);
    }

    // Derived from the losses, not accumulated beside them — see
    // `codex_ir_write::Writer::summarise` for the bug that motivated it. Two
    // sources of truth for one fact meant each covered for the other's gaps.
    // The budget's losses join the same list, so the fold grades them too.
    let mut losses = budgeted.losses.clone();
    losses.extend(writer.losses());
    let fidelity = losses
        .iter()
        .fold(Fidelity::ContextComplete, |worst, loss| {
            worst.worse_of(loss.grade)
        });
    Some(Rendered {
        lines,
        fidelity,
        warnings: losses.iter().map(|loss| loss.note.clone()).collect(),
        losses,
    })
}

// ---------------------------------------------------------------------------
// Parts and records
// ---------------------------------------------------------------------------

/// Which of the two native record types an event belongs in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    User,
    Assistant,
}

/// One event, rendered as the content block(s) it contributes to a record.
struct Part {
    side: Side,
    blocks: Vec<Value>,
    /// True when this part is the record's coalesced text. At most one per
    /// record: the reader folds all text blocks into a single event, so a
    /// second one here would come back merged and the round trip would lose an
    /// event.
    is_text: bool,
    /// `tool_use.id`s this part introduces.
    provides: Vec<String>,
    /// The `tool_use.id` this part answers, and the structured companion that
    /// belongs beside it at record level.
    answers: Option<(String, Option<Value>)>,
}

/// One native `user` or `assistant` record under construction.
struct Record {
    side: Side,
    line: u64,
    branch: Branch,
    turn: Option<String>,
    ts: Option<i64>,
    blocks: Vec<Value>,
    has_text: bool,
    provides: Vec<String>,
    answers: Option<String>,
    structured: Option<Value>,
    /// The record is a single plain user utterance, which Claude writes as a
    /// bare string rather than a one-element array.
    plain_text: Option<String>,
}

impl Record {
    fn new(event: &Event, part: Part) -> Self {
        let mut record = Record {
            side: part.side,
            line: event.source.line,
            branch: event.branch.clone(),
            turn: event.turn.clone(),
            ts: event.ts,
            blocks: Vec::new(),
            has_text: false,
            provides: Vec::new(),
            answers: None,
            structured: None,
            plain_text: None,
        };
        record.push(part);
        record
    }

    /// Whether `event` belongs in this record rather than starting a new one.
    ///
    /// Only events the reader would have split out of *this* native record are
    /// folded back into it, which is what makes the merge invertible: same
    /// source line, same side, same branch, and never a second coalesced text
    /// block.
    fn accepts(&self, event: &Event, part: &Part) -> bool {
        self.side == part.side
            && self.line == event.source.line
            && self.branch == event.branch
            && self.turn == event.turn
            && !(part.is_text && self.has_text)
    }

    fn push(&mut self, part: Part) {
        self.has_text |= part.is_text;
        self.provides.extend(part.provides);
        if let Some((call_id, structured)) = part.answers {
            self.answers.get_or_insert(call_id);
            if self.structured.is_none() {
                self.structured = structured;
            }
        }
        // A lone text block on a user record is written as a bare string, the
        // way Claude writes an ordinary prompt. Never for empty text: the
        // reader discards a blank string outright, so the event would not
        // survive its own round trip.
        self.plain_text = match (self.blocks.is_empty(), part.is_text, self.side) {
            (true, true, Side::User) if part.blocks.len() == 1 => part.blocks[0]
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
            _ => None,
        };
        self.blocks.extend(part.blocks);
    }

    fn render(&self, env: Envelope<'_>) -> Value {
        let content = match &self.plain_text {
            Some(text) => json!(text),
            None => Value::Array(self.blocks.clone()),
        };
        let mut record = Map::new();
        record.insert(
            "parentUuid".into(),
            match env.parent {
                Some(parent) => json!(parent),
                None => Value::Null,
            },
        );
        let sidechain = matches!(self.branch, Branch::Sub(_));
        record.insert("isSidechain".into(), json!(sidechain));
        if let Branch::Sub(agent) = &self.branch {
            record.insert("agentId".into(), json!(agent));
        }
        record.insert("userType".into(), json!(USER_TYPE));
        record.insert("entrypoint".into(), json!(ENTRYPOINT));
        record.insert("cwd".into(), json!(env.cwd));
        record.insert("sessionId".into(), json!(env.session_id));
        record.insert("version".into(), json!(env.version));
        record.insert("gitBranch".into(), json!(env.git_branch));
        record.insert(
            "type".into(),
            json!(match self.side {
                Side::User => "user",
                Side::Assistant => "assistant",
            }),
        );
        record.insert("message".into(), self.message(&content, env.model));
        record.insert("uuid".into(), json!(env.uuid));
        record.insert(
            "timestamp".into(),
            json!(
                self.ts
                    .and_then(DateTime::from_timestamp_millis)
                    .map(|when| when.to_rfc3339_opts(SecondsFormat::Millis, true))
                    .unwrap_or_else(|| env.now_iso.to_string())
            ),
        );
        if let Some(turn) = &self.turn {
            record.insert("promptId".into(), json!(turn));
        }
        if let Some(structured) = &self.structured {
            record.insert("toolUseResult".into(), structured.clone());
        }
        if let Some(caller) = self
            .answers
            .as_deref()
            .and_then(|call_id| env.caller_of.get(call_id))
        {
            record.insert("sourceToolAssistantUUID".into(), json!(caller));
        }
        Value::Object(record)
    }

    /// The inner `message` object.
    ///
    /// Assistant records carry the full Anthropic response envelope (`id`,
    /// `type`, `model`, `stop_reason`, `usage`). Without it `claude --resume`
    /// hangs on load and reports "Failed to resume session" — the same reason
    /// the flat writer synthesises them.
    fn message(&self, content: &Value, model: &str) -> Value {
        match self.side {
            Side::User => json!({ "role": "user", "content": content }),
            Side::Assistant => json!({
                "id": format!("msg_casr_{}", uuid::Uuid::new_v4().simple()),
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": "end_turn",
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }),
        }
    }
}

/// The per-session values every record repeats.
struct Envelope<'a> {
    uuid: &'a str,
    parent: Option<&'a str>,
    session_id: &'a str,
    cwd: &'a str,
    version: &'a str,
    model: &'a str,
    git_branch: &'a str,
    now_iso: &'a str,
    caller_of: &'a HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

struct Writer {
    same_agent: bool,
    dropped_reasoning: usize,
    reasoning_bytes: usize,
    dropped_history: usize,
    history_bytes: usize,
    downgraded_calls: usize,
    recast_roles: usize,
    dropped_empty: usize,
}

impl Writer {
    /// Whether a capsule may be written into this target, grading any drop.
    ///
    /// `carries_history` is the caller's, not the capsule's, because the two
    /// losses look identical on disk and are not comparable: a dropped
    /// [`Body::SealedContext`] blob deletes the conversation, a dropped
    /// reasoning blob deletes a train of thought Anthropic strips from the next
    /// request anyway.
    fn keeps(&mut self, capsule: &Capsule, carries_history: bool) -> bool {
        if capsule.fits(TARGET_VENDOR) == CapsuleFit::SameVendor {
            return true;
        }
        if carries_history || capsule.kind == CapsuleKind::OpenaiCompactedContext {
            self.dropped_history += 1;
            self.history_bytes += capsule.sealed.len();
        } else {
            self.dropped_reasoning += 1;
            self.reasoning_bytes += capsule.sealed.len();
        }
        false
    }

    /// The structured loss list. Warnings and the grade are both derived from
    /// it, so a loss cannot be reported in one channel and missing from another.
    fn losses(&self) -> Vec<Loss> {
        let mut losses = Vec::new();
        let mut push = |kind, events, capsules, bytes, grade, note| {
            if events > 0 {
                losses.push(Loss {
                    kind,
                    events,
                    capsules,
                    bytes,
                    grade,
                    note,
                });
            }
        };
        push(
            LossKind::SealedContext,
            self.dropped_history,
            self.dropped_history,
            self.history_bytes,
            Fidelity::HistoryIncomplete,
            format!(
                "{} compacted-history capsule(s) totalling {} bytes could not be carried \
                 across. The resumed session is missing history; a marker was written in \
                 their place.",
                self.dropped_history, self.history_bytes
            ),
        );
        push(
            LossKind::Reasoning,
            self.dropped_reasoning,
            self.dropped_reasoning,
            self.reasoning_bytes,
            Fidelity::ContextNoReasoning,
            format!(
                "{} reasoning capsule(s) totalling {} bytes were minted by another vendor and \
                 cannot be replayed here; they were dropped rather than replaced by a \
                 placeholder.",
                self.dropped_reasoning, self.reasoning_bytes
            ),
        );
        push(
            LossKind::ToolProtocol,
            self.downgraded_calls,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} freeform tool call(s) were written as JSON-argument calls: Claude Code has \
                 one calling convention and the distinction cannot survive.",
                self.downgraded_calls
            ),
        );
        push(
            LossKind::Metadata,
            self.recast_roles,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} system/developer/tool message(s) were written as user records; Claude Code \
                 has only `user` and `assistant`.",
                self.recast_roles
            ),
        );
        push(
            LossKind::Reasoning,
            self.dropped_empty,
            0,
            0,
            Fidelity::ContextNoReasoning,
            format!(
                "{} message(s) held nothing but sealed material from another vendor and were \
                 dropped with it.",
                self.dropped_empty
            ),
        );
        losses
    }

    /// The content this event contributes, or `None` when it contributes none.
    fn part(&mut self, event: &Event) -> Option<Part> {
        match &event.body {
            Body::Message { role, blocks } => {
                let side = match role {
                    Role::Assistant => Side::Assistant,
                    Role::User => Side::User,
                    // Claude Code has two record types. Everything else — 4,870
                    // Codex `developer` messages among them — becomes a user
                    // record, which is a genuine loss of the operator/harness
                    // distinction rather than a rename.
                    _ => {
                        self.recast_roles += 1;
                        Side::User
                    }
                };
                let blocks = self.content(blocks);
                if blocks.is_empty() {
                    // A Codex `agent_message` can be nothing but sealed
                    // material, and the seal is not ours to carry. What is left
                    // is a record with no content, which the reader discards —
                    // so it is dropped here, where it can be counted.
                    self.dropped_empty += 1;
                    return None;
                }
                Some(Part {
                    side,
                    blocks,
                    is_text: true,
                    provides: Vec::new(),
                    answers: None,
                })
            }

            Body::Reasoning { text, summary } => {
                // Across providers the capsule must be dropped, and with it the
                // block: an empty `signature` is not a thinking block Anthropic
                // will accept, and a placeholder costs context window while
                // telling the model its own reasoning was truncated.
                let sealed = event
                    .capsules
                    .iter()
                    .find(|capsule| self.keeps(capsule, false))
                    .map(|capsule| capsule.sealed.clone())?;
                let mut thinking = text.clone().unwrap_or_default();
                if !summary.is_empty() {
                    if !thinking.is_empty() {
                        thinking.push_str("\n\n");
                    }
                    thinking.push_str(&summary.join("\n\n"));
                }
                Some(Part {
                    side: Side::Assistant,
                    blocks: vec![json!({"type": "thinking", "thinking": thinking, "signature": sealed})],
                    is_text: false,
                    provides: Vec::new(),
                    answers: None,
                })
            }

            Body::ToolCall {
                call_id,
                name,
                namespace,
                input,
            } => {
                let mut block = Map::new();
                block.insert("type".into(), json!("tool_use"));
                block.insert("id".into(), json!(call_id));
                // Preserved, not mapped: these are history, and an invented
                // `shell` → `Bash` equivalence is a claim the data does not make.
                block.insert("name".into(), json!(name));
                block.insert("input".into(), self.tool_input(input));
                if let Some(namespace) = namespace {
                    block.insert("caller".into(), json!({ "type": namespace }));
                }
                Some(Part {
                    side: Side::Assistant,
                    blocks: vec![Value::Object(block)],
                    is_text: false,
                    provides: vec![call_id.clone()],
                    answers: None,
                })
            }

            Body::ToolResult {
                call_id,
                outcome,
                output,
                structured,
            } => {
                let mut block = Map::new();
                block.insert("type".into(), json!("tool_result"));
                block.insert("tool_use_id".into(), json!(call_id));
                block.insert("content".into(), Value::Array(self.content(output)));
                // Claude does write the flag, so `Unknown` is written as the
                // absence the source recorded rather than as a success.
                match outcome {
                    ToolOutcome::Succeeded => {
                        block.insert("is_error".into(), json!(false));
                    }
                    ToolOutcome::Failed => {
                        block.insert("is_error".into(), json!(true));
                    }
                    ToolOutcome::Unknown => {}
                }
                Some(Part {
                    side: Side::User,
                    blocks: vec![Value::Object(block)],
                    is_text: false,
                    provides: Vec::new(),
                    answers: Some((call_id.clone(), structured.clone())),
                })
            }

            Body::SealedContext { .. } => {
                let kept = event.capsules.iter().any(|capsule| self.keeps(capsule, true));
                if !kept && event.capsules.is_empty() {
                    self.dropped_history += 1;
                }
                // Claude has no sealed-context record, so even a hypothetical
                // Anthropic-minted blob has nowhere to go. What it leaves behind
                // is a visible hole rather than silence.
                Some(Part {
                    side: Side::User,
                    blocks: vec![json!({"type": "text", "text": SEALED_CONTEXT_MARKER})],
                    is_text: true,
                    provides: Vec::new(),
                    answers: None,
                })
            }

            // Chrome the resolver filtered out; compaction markers, which are
            // boundaries rather than content; and rollback/abort, which are
            // directives the resolver has already applied. Reaching any of them
            // means the resolver changed underneath us.
            //
            // Enumerated rather than wildcarded on purpose: a `Body` variant no
            // writer handles is a hole in every conversion this crate performs,
            // and a `_` arm is how it would stay invisible.
            Body::Compaction { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => None,
        }
    }

    /// `tool_use.input`, which the Anthropic API requires to be an object.
    ///
    /// A freeform call's input is not required to be JSON at all, so it is
    /// wrapped rather than parsed. The text survives verbatim; the fact that it
    /// was freeform does not, because Claude Code has nowhere to record a second
    /// calling convention.
    fn tool_input(&mut self, input: &ToolInput) -> Value {
        match input {
            ToolInput::Json { value, original } => match original {
                Some(text) => json!(text),
                None => value.clone(),
            },
            ToolInput::Freeform { text } => {
                self.downgraded_calls += 1;
                super::claude_code::coerce_tool_input(&json!(text))
            }
        }
    }

    fn content(&mut self, blocks: &[Block]) -> Vec<Value> {
        blocks
            .iter()
            .map(|block| match block {
                Block::Text { text } => json!({"type": "text", "text": text}),
                Block::Image { url, media_type } => image_block(url, media_type.as_deref()),
                Block::Document { data } => data.clone(),
                Block::Redacted { reason } => json!({
                    "type": "text",
                    "text": match reason {
                        Some(reason) => format!("[redacted: {reason}]"),
                        None => "[redacted]".to_string(),
                    },
                }),
                Block::Unknown { raw, .. } => raw.clone(),
            })
            .collect()
    }
}

/// Rebuild an Anthropic image block from the URL the reader flattened it into.
fn image_block(url: &str, media_type: Option<&str>) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media, data)) = rest.split_once(";base64,")
    {
        return json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media, "data": data},
        });
    }
    let mut source = Map::new();
    source.insert("type".into(), json!("url"));
    source.insert("url".into(), json!(url));
    if let Some(media_type) = media_type {
        source.insert("media_type".into(), json!(media_type));
    }
    json!({"type": "image", "source": Value::Object(source)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CapsuleBinding, SourceRef, Visibility};

    fn event(id: &str, line: u64, body: Body) -> Event {
        Event {
            id: id.to_string(),
            parent: None,
            branch: Branch::Main,
            turn: Some("p1".to_string()),
            ts: None,
            visibility: Visibility::Model,
            body,
            capsules: Vec::new(),
            source: SourceRef {
                line,
                sha256: String::new(),
            },
        }
    }

    fn ir(events: Vec<Event>) -> SessionIr {
        let mut ir = SessionIr::new(SAME_AGENT, "s1");
        ir.origin.provider = Some(TARGET_VENDOR.to_string());
        ir.events = events;
        ir
    }

    fn records(out: &Rendered) -> Vec<Value> {
        out.lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("valid JSON"))
            .collect()
    }

    #[test]
    fn every_record_carries_the_twelve_universal_fields() {
        let out = render(
            &ir(vec![event(
                "u1",
                1,
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text { text: "hi".into() }],
                },
            )]),
            "sid",
            Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("non-empty replay");

        let record = &records(&out)[0];
        for field in [
            "cwd",
            "entrypoint",
            "gitBranch",
            "isSidechain",
            "message",
            "parentUuid",
            "sessionId",
            "timestamp",
            "type",
            "userType",
            "uuid",
            "version",
        ] {
            assert!(
                record.get(field).is_some(),
                "{field} is on 100% of corpus records and must be written"
            );
        }
        assert_eq!(record["message"]["content"], json!("hi"));
        assert_eq!(record["promptId"], json!("p1"));
    }

    #[test]
    fn one_native_record_is_rebuilt_from_the_events_it_split_into() {
        let mut thinking = event(
            "a1",
            7,
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        thinking.capsules.push(Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".into(),
                model: None,
            },
            sealed: "SIG".into(),
        });
        let out = render(
            &ir(vec![
                thinking,
                event(
                    "a1#1",
                    7,
                    Body::ToolCall {
                        call_id: "t1".into(),
                        name: "Bash".into(),
                        namespace: Some("direct".into()),
                        input: ToolInput::Json {
                            value: json!({"command": "ls"}),
                            original: None,
                        },
                    },
                ),
                event(
                    "a1#2",
                    7,
                    Body::Message {
                        role: Role::Assistant,
                        blocks: vec![Block::Text {
                            text: "running".into(),
                        }],
                    },
                ),
            ]),
            "sid",
            Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("non-empty replay");

        let records = records(&out);
        assert_eq!(records.len(), 1, "three events, one native record");
        let kinds: Vec<&str> = records[0]["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["thinking", "tool_use", "text"]);
        assert_eq!(out.fidelity, Fidelity::ContextComplete);
    }

    #[test]
    fn a_tool_result_names_the_record_that_called_it() {
        let out = render(
            &ir(vec![
                event(
                    "a1",
                    1,
                    Body::ToolCall {
                        call_id: "t1".into(),
                        name: "Bash".into(),
                        namespace: None,
                        input: ToolInput::Json {
                            value: json!({}),
                            original: None,
                        },
                    },
                ),
                event(
                    "u2",
                    2,
                    Body::ToolResult {
                        call_id: "t1".into(),
                        outcome: ToolOutcome::Succeeded,
                        output: vec![Block::Text { text: "ok".into() }],
                        structured: Some(json!({"stdout": "ok"})),
                    },
                ),
            ]),
            "sid",
            Utc::now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("non-empty replay");

        let records = records(&out);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1]["sourceToolAssistantUUID"], records[0]["uuid"],
            "the field is on 8,455 of 8,455 corpus tool-result records"
        );
        assert_eq!(records[1]["toolUseResult"], json!({"stdout": "ok"}));
        assert_eq!(records[1]["parentUuid"], records[0]["uuid"]);
    }

    #[test]
    fn a_foreign_reasoning_capsule_takes_its_block_with_it() {
        let mut thinking = event(
            "r",
            1,
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        thinking.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
            bound: CapsuleBinding {
                provider: "openai".into(),
                model: None,
            },
            sealed: "BBBB".into(),
        });
        let mut source = ir(vec![
            thinking,
            event(
                "m",
                2,
                Body::Message {
                    role: Role::Assistant,
                    blocks: vec![Block::Text {
                        text: "done".into(),
                    }],
                },
            ),
        ]);
        source.origin.agent = "codex".into();

        let out = render(&source, "sid", Utc::now(), &ContextBudget::UNLIMITED).expect("non-empty replay");
        assert_eq!(records(&out).len(), 1);
        assert_eq!(out.fidelity, Fidelity::ContextNoReasoning);
    }

    #[test]
    fn a_freeform_call_is_wrapped_rather_than_parsed() {
        let mut source = ir(vec![event(
            "c",
            1,
            Body::ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                namespace: None,
                input: ToolInput::Freeform {
                    text: "ls -la".into(),
                },
            },
        )]);
        source.origin.agent = "codex".into();

        let out = render(&source, "sid", Utc::now(), &ContextBudget::UNLIMITED).expect("non-empty replay");
        let block = &records(&out)[0]["message"]["content"][0];
        assert_eq!(block["name"], json!("shell"), "the tool name is history");
        assert_eq!(block["input"], json!({"value": "ls -la"}));
        assert_eq!(out.fidelity, Fidelity::ConversationOnly);
    }

    /// The budget's other half: the same trim, on the writer that coalesces
    /// events into records. Records, not events, is the unit here — so a trim
    /// that cut inside a record would show up as a record that lost a block.
    #[test]
    fn a_cap_keeps_the_tail_and_reports_what_it_dropped() {
        let source = ir(vec![
            event(
                "u1",
                1,
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "x".repeat(400),
                    }],
                },
            ),
            event(
                "u2",
                2,
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "the newest turn".into(),
                    }],
                },
            ),
        ]);

        let whole = render(&source, "sid", Utc::now(), &ContextBudget::UNLIMITED)
            .expect("non-empty replay");
        assert_eq!(records(&whole).len(), 2);
        assert!(whole.losses.is_empty());
        assert_eq!(whole.fidelity, Fidelity::ContextComplete);

        let trimmed = render(
            &source,
            "sid",
            Utc::now(),
            &ContextBudget {
                max_context_tokens: 20,
                max_tool_output: 0,
                keep_reasoning: true,
            },
        )
        .expect("non-empty replay");
        let records = records(&trimmed);
        assert_eq!(records.len(), 1, "the oldest record went, the tail stayed");
        assert_eq!(records[0]["message"]["content"], json!("the newest turn"));
        assert_eq!(
            records[0]["parentUuid"],
            Value::Null,
            "the kept tail is a chain of its own, not a chain with a dangling head"
        );
        assert_eq!(trimmed.losses.len(), 1);
        assert_eq!(trimmed.losses[0].kind, LossKind::Conversation);
        assert_eq!(trimmed.fidelity, Fidelity::HistoryIncomplete);
    }

    #[test]
    fn an_oversized_observation_is_elided_in_place_not_dropped() {
        let source = ir(vec![
            event(
                "a1",
                1,
                Body::ToolCall {
                    call_id: "t1".into(),
                    name: "Bash".into(),
                    namespace: None,
                    input: ToolInput::Json {
                        value: json!({}),
                        original: None,
                    },
                },
            ),
            event(
                "u2",
                2,
                Body::ToolResult {
                    call_id: "t1".into(),
                    outcome: ToolOutcome::Succeeded,
                    output: vec![Block::Text {
                        text: "E".repeat(5_000),
                    }],
                    structured: None,
                },
            ),
        ]);
        let out = render(
            &source,
            "sid",
            Utc::now(),
            &ContextBudget {
                max_context_tokens: 0,
                max_tool_output: 100,
                keep_reasoning: true,
            },
        )
        .expect("non-empty replay");

        let records = records(&out);
        assert_eq!(records.len(), 2, "the pair is intact");
        let block = &records[1]["message"]["content"][0];
        assert_eq!(block["tool_use_id"], json!("t1"));
        let text = block["content"][0]["text"].as_str().expect("text block");
        assert!(text.contains("elided"), "{text}");
        assert!(text.len() < 400);
        assert_eq!(out.losses.len(), 1);
        assert_eq!(out.losses[0].kind, LossKind::ToolProtocol);
        assert_eq!(out.fidelity, Fidelity::ConversationOnly);
    }

    #[test]
    fn a_dropped_compaction_leaves_a_visible_hole() {
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
        let mut source = ir(vec![sealed]);
        source.origin.agent = "codex".into();

        let out = render(&source, "sid", Utc::now(), &ContextBudget::UNLIMITED).expect("non-empty replay");
        assert_eq!(
            out.fidelity,
            Fidelity::HistoryIncomplete,
            "losing the conversation ranks worse than losing the train of thought"
        );
        assert!(out.lines[0].contains("[converted by casr]"));
    }
}
