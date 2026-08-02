//! Structured IR → Codex rollout JSONL.
//!
//! The inverse of [`super::codex_ir`], and deliberately shaped as one: every
//! record this module emits is a record that reader parses back into the event
//! it came from. That is the only definition of "correct" a writer can be held
//! to, and `tests/roundtrip_ir_test.rs` holds it to exactly that.
//!
//! # What gets written
//!
//! [`SessionIr::model_visible`] and nothing else. The capture also contains
//! chrome, telemetry, superseded history and rolled-back turns; replaying those
//! is how a converted session ends up larger than the conversation it came
//! from. On the local corpus the resolver takes 492,429 captured Codex model
//! events down to 94,478, so a writer emitting anything near the captured count
//! has bypassed it.
//!
//! # Measured envelope
//!
//! Counted over 592 local rollouts on 2026-07-26. Every one of the 319,659
//! `response_item` lines is exactly `{payload, timestamp, type}` — all the
//! structure is in `payload` — and
//! `internal_chat_message_metadata_passthrough` holds exactly one key,
//! `turn_id`, on all 319,184 payloads that carry it. `session_meta.payload`
//! carries twelve keys on 100% of 950 headers (`base_instructions` and
//! `context_window` on 99.79%).
//!
//! # What this writer cannot reproduce
//!
//! Codex mints an item id for every reasoning (`rs_…`), tool call (`fc_…` /
//! `ctc_…`) and half its messages (`msg_…`), and the IR models none of them.
//! Fabricating one would be worse than omitting it: an invented `rs_…` beside a
//! real `encrypted_content` blob asserts an identity the provider never issued,
//! and the Responses API is entitled to reject the pair. The ids are left out
//! and the gap is reported rather than papered over.

use std::collections::HashSet;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use crate::budget::ContextBudget;
use crate::ir::{
    Block, Body, Capsule, CapsuleFit, CapsuleKind, Event, Fidelity, Loss, LossKind, Role,
    SessionIr, ToolInput, ToolOutcome,
};

/// The vendor whose sealed blobs a Codex session may replay.
///
/// Keyed on the capsule's format rather than on the endpoint that served the
/// source session — see [`Capsule::fits`]; 158 of the 950 local headers name a
/// gateway as `model_provider` while relaying OpenAI's blobs.
const TARGET_VENDOR: &str = "openai";

/// [`crate::ir::Origin::agent`] of a session this writer can replay natively.
const SAME_AGENT: &str = "codex";

/// Placed where a sealed compaction could not be carried across.
///
/// Silence is the worst option available: the target then answers confidently
/// as though it has the full history. See [`Fidelity::HistoryIncomplete`].
const SEALED_CONTEXT_MARKER: &str = "[converted by casr] The earlier part of this conversation was \
compacted by the source agent, which returned it sealed in a format only that \
agent's provider can read. It could not be transferred and is missing from this \
session. Do not assume the history above is complete — ask before relying on \
anything from before this point.";

/// A rendered rollout, with the grade the rendering earned.
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
    /// First user text in the replay, for the thread-index title.
    pub first_user_text: String,
}

/// Render `ir` as a Codex rollout under `session_id`, inside `budget`.
///
/// `None` when the replay is empty: there is no structured session to write,
/// and the caller is better served by the flat path than by a header with no
/// conversation under it.
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
        grade: Fidelity::ContextComplete,
        warnings: Vec::new(),
        // The budget's losses go into the same list the writer's own go into, so
        // the fold in `summarise` grades both. Nothing accumulates a grade
        // separately — that is the bug the fold replaced.
        losses: budgeted.losses.clone(),
        dropped_reasoning: 0,
        reasoning_bytes: 0,
        dropped_history: 0,
        history_bytes: 0,
        dropped_structured: 0,
        reshaped_reasoning: 0,
        untyped_images: 0,
        dropped_documents: 0,
        foreign_seals: 0,
        foreign_seal_bytes: 0,
        history_gap_tool_calls: 0,
        freeform_calls: HashSet::new(),
    };

    // Which calls arrived freeform decides whether their output is written as
    // `custom_tool_call_output` or `function_call_output`. Codex pairs the two
    // by shape as well as by `call_id`, so this is a pre-pass over the whole
    // replay rather than a running guess.
    for event in &visible {
        if let Body::ToolCall { call_id, input, .. } = &event.body
            && matches!(input, ToolInput::Freeform { .. })
        {
            writer.freeform_calls.insert(call_id.clone());
        }
    }

    // A sealed compaction can only be written back inside a `compacted`
    // envelope — as a bare `response_item` the reader files it under
    // [`Body::Unknown`], losing the very thing it is there to carry. So when
    // the replay still holds sealed context, the events belonging to the last
    // compaction's context go back into `replacement_history` and the rest
    // follow as ordinary lines.
    let checkpoint: HashSet<&str> = ir
        .events
        .iter()
        .rev()
        .find_map(|event| match &event.body {
            Body::Compaction { context, .. } => Some(context),
            // Enumerated rather than wildcarded: a `Body` variant added later
            // that also names a context would be silently skipped by a `_` arm,
            // and the failure would be an empty checkpoint — which reads as
            // "this session never compacted".
            Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::SealedContext { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => None,
        })
        .map(|context| context.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let sealed_present = visible
        .iter()
        .any(|event| matches!(event.body, Body::SealedContext { .. }));
    let (head, tail): (Vec<&Event>, Vec<&Event>) = if sealed_present {
        visible.iter().copied().partition(|event| {
            checkpoint.contains(event.id.as_str())
                || matches!(event.body, Body::SealedContext { .. })
        })
    } else {
        (Vec::new(), visible.to_vec())
    };

    let now_iso = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut lines: Vec<String> = Vec::with_capacity(visible.len() + 2);
    lines.push(session_meta(ir, session_id, &now_iso).to_string());
    if let Some(context) = turn_context(ir) {
        lines.push(
            json!({
                "type": "turn_context",
                "timestamp": now_iso,
                "payload": context,
            })
            .to_string(),
        );
    }

    if !head.is_empty() {
        let history: Vec<Value> = head
            .iter()
            .flat_map(|event| writer.replacement_items(event))
            .collect();
        lines.push(
            json!({
                "type": "compacted",
                "timestamp": stamp(head[0].ts, &now_iso),
                "payload": { "replacement_history": history },
            })
            .to_string(),
        );
        // Codex pairs the two exactly: across the 469 corpus rollouts that hold
        // either, the count of `compacted` envelopes equals the count of
        // `event_msg.context_compacted` in every single file. So the boundary is
        // restated on the history channel here, not invented — the file being
        // written genuinely records a compaction at this point.
        lines.push(
            json!({
                "type": "event_msg",
                "timestamp": stamp(head[0].ts, &now_iso),
                "payload": { "type": "context_compacted" },
            })
            .to_string(),
        );
    }

    for event in &tail {
        for payload in writer.payloads(event) {
            // The wire shape is checked here because here is the last place it
            // exists: one line later it is a string, and once it is read back
            // it is an `Event` again and the shape is gone. See
            // [`wire_contract_violation`] for why the IR-level oracle cannot
            // see this class at all.
            debug_assert!(
                wire_contract_violation(&payload).is_none(),
                "Codex writer emitted a payload Codex cannot read: {}",
                wire_contract_violation(&payload).unwrap_or_default(),
            );
            lines.push(
                json!({
                    "type": "response_item",
                    "timestamp": stamp(event.ts, &now_iso),
                    "payload": payload,
                })
                .to_string(),
            );
        }
        // After the `response_item`, not before: the `user_message` follows its
        // conversation record on all 1,441 corpus occurrences. See
        // [`history_events`] for why only a few kinds are derived.
        for payload in history_events(event) {
            lines.push(
                json!({
                    "type": "event_msg",
                    "timestamp": stamp(event.ts, &now_iso),
                    "payload": payload,
                })
                .to_string(),
            );
        }
    }

    let first_user_text = visible
        .iter()
        .find_map(|event| match &event.body {
            Body::Message {
                role: Role::User,
                blocks,
            } => Some(
                blocks
                    .iter()
                    .filter_map(Block::as_text)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            // Enumerated rather than wildcarded, for the reason the `payloads`
            // match states at length: a `_` arm over `Body` is how a new variant
            // stays invisible.
            Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::Compaction { .. }
            | Body::SealedContext { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => None,
        })
        .unwrap_or_default();

    writer.summarise();
    Some(Rendered {
        lines,
        fidelity: writer.grade,
        warnings: writer.warnings,
        losses: writer.losses,
        first_user_text,
    })
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

struct Writer {
    same_agent: bool,
    grade: Fidelity,
    losses: Vec<Loss>,
    warnings: Vec<String>,
    dropped_reasoning: usize,
    reasoning_bytes: usize,
    dropped_history: usize,
    history_bytes: usize,
    dropped_structured: usize,
    reshaped_reasoning: usize,
    untyped_images: usize,
    dropped_documents: usize,
    foreign_seals: usize,
    foreign_seal_bytes: usize,
    /// Tool calls written to the conversation channel. Counted because none of
    /// them can be written to the *history* channel — see [`history_events`] —
    /// so a session with tool calls resumes with a history view that shows the
    /// messages and not the work.
    history_gap_tool_calls: usize,
    freeform_calls: HashSet<String>,
}

impl Writer {
    /// Whether a capsule may be written into this target, grading any drop.
    ///
    /// `carries_history` is the caller's, not the capsule's, because the two
    /// losses look identical on disk and are not comparable: a dropped
    /// [`Body::SealedContext`] blob deletes the conversation, a dropped
    /// reasoning blob deletes a train of thought the provider would have
    /// stripped from the next request anyway.
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

    /// Turn the running counts into the loss list, then derive the grade from
    /// it.
    ///
    /// Derived, not accumulated. There used to be a `degrade()` alongside the
    /// counters, and the two disagreed: `dropped_structured` bumped its counter
    /// and pushed a `ConversationOnly` loss but never called `degrade`, so 126
    /// of 177 claude→codex corpus sessions reported a grade one rung better
    /// than their own loss list said. Three other sites had the mirror bug —
    /// `degrade` with no counter, so the grade was right and the loss list was
    /// short. Two sources of truth for one fact, each covering for the other's
    /// gaps.
    ///
    /// Now the losses *are* the grade. A site that forgets to record a loss
    /// reports a grade that is too good, which is the same failure as before —
    /// but there is only one place to forget, and the structural comparator
    /// independently re-derives the grade from the written file and warns when
    /// the two disagree.
    fn summarise(&mut self) {
        let mut push = |kind, events, capsules, bytes, grade, note| {
            if events > 0 {
                self.losses.push(Loss {
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
            LossKind::Metadata,
            self.dropped_structured,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} tool result(s) carried a structured companion the Codex format has nowhere \
                 to put; the output the model saw was written, the companion was not.",
                self.dropped_structured
            ),
        );
        push(
            LossKind::Reasoning,
            self.reshaped_reasoning,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} reasoning block(s) held plaintext, which Codex has no field for, so the \
                 words were folded into `summary`. They survive; the shape does not.",
                self.reshaped_reasoning
            ),
        );
        push(
            LossKind::Media,
            self.untyped_images,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} image(s) arrived with a declared media type; Codex records an image as a \
                 bare `image_url`, so the type is gone.",
                self.untyped_images
            ),
        );
        push(
            LossKind::Media,
            self.dropped_documents,
            0,
            0,
            Fidelity::ConversationOnly,
            format!(
                "{} document block(s) have no Codex counterpart and were written as \
                 unrecognised blocks rather than flattened into prose.",
                self.dropped_documents
            ),
        );
        push(
            LossKind::Reasoning,
            self.foreign_seals,
            self.foreign_seals,
            self.foreign_seal_bytes,
            Fidelity::ContextNoReasoning,
            format!(
                "{} unrecognised block(s) totalling {} bytes carried a sealed \
                 `encrypted_content` field from another agent. Written back they would read \
                 as OpenAI capsules the issuing vendor never minted, so they were dropped \
                 rather than re-labelled.",
                self.foreign_seals, self.foreign_seal_bytes
            ),
        );

        self.warnings
            .extend(self.losses.iter().map(|loss| loss.note.clone()));

        // Not a `Loss`: nothing the model sees is missing, so this must not move
        // the grade or the launch decision. It is still the difference between a
        // resumed session that looks right and one whose transcript has a hole
        // in it, and the user is better told than left to find out.
        if self.history_gap_tool_calls > 0 {
            self.warnings.push(format!(
                "Codex's history view is rebuilt from its `event_msg` channel, and only \
                 messages and compaction boundaries could be derived for it. The {} tool \
                 call(s) in this session are in the model's context but will not appear in \
                 the transcript Codex displays: the records that would show them assert \
                 timings and a sub-agent/MCP provenance the source session does not record, \
                 and inventing those would put a history in front of you that did not happen.",
                self.history_gap_tool_calls
            ));
        }
        self.grade = self
            .losses
            .iter()
            .fold(Fidelity::ContextComplete, |worst, loss| {
                worst.worse_of(loss.grade)
            });
    }

    /// Zero or more `response_item` payloads for one event.
    ///
    /// Zero when the event is entirely provider-bound material this target
    /// cannot replay: a dropped reasoning capsule leaves nothing behind, and an
    /// empty husk in its place costs context window while telling the model its
    /// own reasoning was truncated.
    fn payloads(&mut self, event: &Event) -> Vec<Value> {
        match &event.body {
            Body::Message { role, blocks } => {
                let mut content = self.content(blocks, matches!(role, Role::Assistant));
                for capsule in &event.capsules {
                    if self.keeps(capsule, false) {
                        content.push(json!({
                            "type": "encrypted_content",
                            "encrypted_content": capsule.sealed,
                        }));
                    }
                }
                vec![with_turn(
                    event,
                    json!({
                        "type": "message",
                        "role": role_string(role),
                        "content": content,
                    }),
                )]
            }

            Body::Reasoning { text, summary } => {
                let sealed: Option<String> = event
                    .capsules
                    .iter()
                    .find(|capsule| self.keeps(capsule, false))
                    .map(|capsule| capsule.sealed.clone());
                // Codex has no plaintext reasoning field, so a source that
                // recorded one — no current agent does; 0 of 5,979 Claude
                // `thinking` blocks carry text — is folded into `summary`. The
                // words survive, the shape does not.
                let mut summaries: Vec<&str> = summary.iter().map(String::as_str).collect();
                if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                    summaries.push(text);
                    self.reshaped_reasoning += 1;
                }
                if sealed.is_none() && summaries.is_empty() {
                    return Vec::new();
                }
                let mut payload = Map::new();
                payload.insert("type".into(), json!("reasoning"));
                payload.insert(
                    "summary".into(),
                    Value::Array(
                        summaries
                            .iter()
                            .map(|text| json!({"type": "summary_text", "text": text}))
                            .collect(),
                    ),
                );
                if let Some(sealed) = sealed {
                    payload.insert("encrypted_content".into(), json!(sealed));
                }
                vec![with_turn(event, Value::Object(payload))]
            }

            Body::ToolCall {
                call_id,
                name,
                namespace,
                input,
            } => {
                self.history_gap_tool_calls += 1;
                let mut payload = Map::new();
                payload.insert("call_id".into(), json!(call_id));
                // History, not a live call: the original tool name is preserved
                // rather than mapped onto a Codex equivalent, because an
                // invented mapping claims an equivalence that does not hold.
                payload.insert("name".into(), json!(name));
                match input {
                    ToolInput::Freeform { text } => {
                        payload.insert("type".into(), json!("custom_tool_call"));
                        payload.insert("input".into(), json!(text));
                        // "completed" on all 61,250 corpus occurrences.
                        payload.insert("status".into(), json!("completed"));
                    }
                    ToolInput::Json { value, original } => {
                        payload.insert("type".into(), json!("function_call"));
                        // `arguments` is a *string* holding JSON, never a JSON
                        // object. Codex deserialises `function_call.arguments`
                        // as `String`; an object there fails the record's
                        // decode, and because the paired output survives on its
                        // own it is then reported as an `Orphan function call
                        // output` and dropped too. So a source that kept no
                        // original argument text — Claude Code records
                        // `tool_use.input` as an object, so that is every
                        // Claude Code tool call — used to lose the call *and*
                        // its result on resume, silently, with casr's own
                        // read-back clean because [`ToolInput::from_json_field`]
                        // accepts the object it wrote.
                        payload.insert(
                            "arguments".into(),
                            match original {
                                Some(text) => json!(text),
                                None => json!(
                                    serde_json::to_string(value)
                                        .unwrap_or_else(|_| "{}".to_string())
                                ),
                            },
                        );
                    }
                }
                if let Some(namespace) = namespace {
                    payload.insert("namespace".into(), json!(namespace));
                }
                vec![with_turn(event, Value::Object(payload))]
            }

            Body::ToolResult {
                call_id,
                outcome,
                output,
                structured,
            } => {
                // The provider-side tool catalogue is not text, and flattening
                // it would destroy the schemas, so `tool_search_output` goes
                // back as itself.
                if let Some(structured) = structured
                    && structured.get("type").and_then(Value::as_str) == Some("tool_search_output")
                {
                    return vec![with_turn(event, structured.clone())];
                }
                let mut payload = Map::new();
                payload.insert(
                    "type".into(),
                    json!(if self.freeform_calls.contains(call_id) {
                        "custom_tool_call_output"
                    } else {
                        "function_call_output"
                    }),
                );
                payload.insert("call_id".into(), json!(call_id));
                if let Some(value) = self.tool_output(output, structured.as_ref()) {
                    payload.insert("output".into(), value);
                }
                // Codex writes no success marker on tool output at all — not in
                // any of the 85,674 corpus outputs — so `Unknown` is written as
                // the absence the source actually recorded.
                match outcome {
                    ToolOutcome::Succeeded => {
                        payload.insert("success".into(), json!(true));
                    }
                    ToolOutcome::Failed => {
                        payload.insert("success".into(), json!(false));
                    }
                    ToolOutcome::Unknown => {}
                }
                vec![with_turn(event, Value::Object(payload))]
            }

            // Only reachable when the blob is foreign or absent: `render` routes
            // a replayable sealed compaction into `replacement_history` instead.
            Body::SealedContext { .. } => {
                let kept = event
                    .capsules
                    .iter()
                    .any(|capsule| self.keeps(capsule, true));
                if !kept && event.capsules.is_empty() {
                    self.dropped_history += 1;
                }
                vec![with_turn(event, sealed_context_marker())]
            }

            Body::Unknown { raw, .. } => {
                // Model-visible and not understood: the bytes are the only
                // honest answer, and the reader files them back where it found
                // them.
                if raw.is_object() {
                    vec![raw.clone()]
                } else {
                    Vec::new()
                }
            }

            // Compaction markers are boundaries rather than content and are
            // never in the replay; rollback and abort are directives the
            // resolver has already applied; the rest is chrome it filtered out.
            // Reaching any of them means the resolver changed underneath us.
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
            | Body::Control { .. } => Vec::new(),
        }
    }

    /// The `replacement_history` entries for one event.
    ///
    /// The corpus has exactly two shapes here — 168,550 messages and 4,325
    /// sealed compactions — so anything else falls through to the ordinary
    /// response-item rendering rather than inventing a third.
    ///
    /// A `Vec` rather than an `Option` because the fallthrough is
    /// [`Writer::payloads`], which is already allowed to return more than one
    /// item per event. Taking `.next()` off it made the truncation of any such
    /// body silent and invisible; returning what it returns makes the
    /// truncation unrepresentable instead of merely unlikely.
    fn replacement_items(&mut self, event: &Event) -> Vec<Value> {
        match &event.body {
            Body::SealedContext { native_id, meta } => {
                let Some(sealed) = event
                    .capsules
                    .iter()
                    .find(|capsule| self.keeps(capsule, true))
                    .map(|capsule| capsule.sealed.clone())
                else {
                    if event.capsules.is_empty() {
                        self.dropped_history += 1;
                    }
                    return vec![sealed_context_marker()];
                };
                let mut item = Map::new();
                item.insert("type".into(), json!("compaction"));
                item.insert("encrypted_content".into(), json!(sealed));
                if let Some(id) = native_id {
                    item.insert("id".into(), json!(id));
                }
                if !meta.is_null() {
                    item.insert(
                        "internal_chat_message_metadata_passthrough".into(),
                        meta.clone(),
                    );
                }
                vec![Value::Object(item)]
            }
            // `with_turn`, not a bare object: every one of the 176,027 corpus
            // entries carries its own `turn_id`, and the reader now reads it
            // (see `super::codex_ir::Builder::replacement_event`). Writing the
            // entry without it would put the turn back on the floor one hop
            // later.
            Body::Message { role, blocks } => vec![with_turn(
                event,
                json!({
                    "type": "message",
                    "role": role_string(role),
                    "content": self.content(blocks, matches!(role, Role::Assistant)),
                }),
            )],
            // Enumerated rather than wildcarded, same rule as `payloads`.
            Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::Compaction { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => self.payloads(event),
        }
    }

    /// Invert [`super::codex_ir`]'s reading of `payload.output`.
    ///
    /// That reader derives *both* the `output` blocks and `structured` from the
    /// one field, so replaying `structured` reproduces both — but only when the
    /// source was Codex. A foreign structured companion (Claude's
    /// `toolUseResult`) is not the tool output and must not be written as it;
    /// what the model actually saw is.
    fn tool_output(&mut self, blocks: &[Block], structured: Option<&Value>) -> Option<Value> {
        if self.same_agent
            && let Some(structured) = structured
        {
            return Some(structured.clone());
        }
        if structured.is_some() {
            self.dropped_structured += 1;
        }
        match blocks {
            [] => None,
            [Block::Text { text }] => Some(json!(text)),
            many => Some(Value::Array(self.content(many, false))),
        }
    }

    fn content(&mut self, blocks: &[Block], assistant: bool) -> Vec<Value> {
        let text_type = if assistant {
            "output_text"
        } else {
            "input_text"
        };
        let mut content = Vec::with_capacity(blocks.len());
        for block in blocks {
            if self.is_foreign_seal(block) {
                continue;
            }
            content.push(match block {
                Block::Text { text } => json!({"type": text_type, "text": text}),
                Block::Image { url, media_type } => {
                    // Codex records an image as a bare `image_url` and nothing
                    // else, so a source that typed its images loses the type.
                    if media_type.is_some() {
                        self.untyped_images += 1;
                    }
                    json!({"type": "input_image", "image_url": url})
                }
                Block::Document { data } => {
                    // No Codex counterpart. The bytes survive as an unrecognised
                    // block rather than being flattened into prose.
                    self.dropped_documents += 1;
                    data.clone()
                }
                Block::Redacted { reason } => json!({
                    "type": text_type,
                    "text": match reason {
                        Some(reason) => format!("[redacted: {reason}]"),
                        None => "[redacted]".to_string(),
                    },
                }),
                Block::Unknown { raw, .. } => raw.clone(),
            });
        }
        content
    }

    /// Would writing this block mint a Codex-native *sealed* item out of bytes
    /// no vendor gate ever saw?
    ///
    /// [`super::codex_ir`] re-labels any content item typed `encrypted_content`
    /// as [`CapsuleKind::OpenaiReasoningEncryptedContent`] — by the field it
    /// arrived in, not by the vendor that minted it. So an unrecognised block of
    /// that shape written back verbatim reads out the other side as an OpenAI
    /// capsule, and the Responses API is handed bytes only Anthropic can verify.
    /// [`Event::capsules`] is the one door sealed material is allowed through,
    /// and [`Writer::keeps`] is the gate on it; a block is not a capsule and
    /// must not become one.
    ///
    /// Same-agent is exempt, and has to be: a Codex rollout's own
    /// `encrypted_content` really is OpenAI's, and refusing it would delete
    /// content on the one path that conserves everything. The same exemption
    /// `tool_output` already makes, for the same reason.
    ///
    /// Absent from the local corpus — 0 of 176,027 `replacement_history`
    /// entries and 0 of 85,674 tool outputs carry one — which is why it went
    /// unnoticed. This is the same shape as the `redacted_thinking` defect
    /// already fixed in [`CapsuleKind::AnthropicRedactedThinking`]: sealed
    /// material filed as an unknown block survives ungated.
    fn is_foreign_seal(&mut self, block: &Block) -> bool {
        let Block::Unknown { raw, .. } = block else {
            return false;
        };
        if self.same_agent || raw.get("type").and_then(Value::as_str) != Some("encrypted_content") {
            return false;
        }
        self.foreign_seals += 1;
        self.foreign_seal_bytes += raw
            .get("encrypted_content")
            .and_then(Value::as_str)
            .map_or(0, str::len);
        true
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn session_meta(ir: &SessionIr, session_id: &str, now_iso: &str) -> Value {
    let same_agent = ir.origin.agent == SAME_AGENT;
    let cwd = ir
        .workspace
        .cwd
        .as_deref()
        .unwrap_or(std::path::Path::new("/tmp"))
        .to_string_lossy()
        .to_string();
    let mut payload = Map::new();
    // Codex indexes threads by `id`; recent builds also read `session_id`.
    payload.insert("id".into(), json!(session_id));
    payload.insert("session_id".into(), json!(session_id));
    payload.insert("timestamp".into(), json!(now_iso));
    payload.insert("cwd".into(), json!(cwd));
    payload.insert("originator".into(), json!("casr"));
    payload.insert(
        "cli_version".into(),
        json!(match (same_agent, ir.origin.agent_version.as_deref()) {
            (true, Some(version)) => version.to_string(),
            _ => env!("CARGO_PKG_VERSION").to_string(),
        }),
    );
    payload.insert("source".into(), json!("cli"));
    payload.insert("thread_source".into(), json!("user"));
    // The endpoint that served the *source* session is only meaningful when the
    // target speaks to the same one, so a cross-agent write names the vendor it
    // is writing for rather than carrying the other agent's gateway across.
    payload.insert(
        "model_provider".into(),
        json!(match (same_agent, ir.origin.provider.as_deref()) {
            (true, Some(provider)) => provider.to_string(),
            _ => TARGET_VENDOR.to_string(),
        }),
    );
    // `history_mode` is `legacy` on all 950 local headers. The other two are the
    // harness's to supply, so they are present and empty rather than invented.
    payload.insert("base_instructions".into(), Value::Null);
    payload.insert("history_mode".into(), json!("legacy"));
    payload.insert("context_window".into(), Value::Null);
    if ir.workspace.git_branch.is_some() || ir.workspace.git_commit.is_some() {
        payload.insert(
            "git".into(),
            json!({
                "branch": ir.workspace.git_branch,
                "commit_hash": ir.workspace.git_commit,
            }),
        );
    }
    json!({
        "type": "session_meta",
        "timestamp": now_iso,
        "payload": Value::Object(payload),
    })
}

/// The one `turn_context` line, when there is anything to put in it.
///
/// Not decoration: [`super::codex_ir`] sources [`crate::ir::Origin::model`] and
/// [`crate::ir::Workspace::roots`] from here and nowhere else, so a rollout
/// written without it reads back with both fields empty.
fn turn_context(ir: &SessionIr) -> Option<Value> {
    let roots = &ir.workspace.roots;
    if ir.origin.model.is_none() && roots.is_empty() {
        return None;
    }
    let mut payload = Map::new();
    if let Some(model) = &ir.origin.model {
        payload.insert("model".into(), json!(model));
    }
    if !roots.is_empty() {
        payload.insert(
            "workspace_roots".into(),
            Value::Array(
                roots
                    .iter()
                    .map(|root| json!(root.to_string_lossy()))
                    .collect(),
            ),
        );
    }
    Some(Value::Object(payload))
}

// ---------------------------------------------------------------------------
// History channel (`event_msg`)
// ---------------------------------------------------------------------------

/// The `event_msg` records an event genuinely evidences, or nothing.
///
/// # Why a second channel exists at all
///
/// A Codex rollout carries the conversation twice, for two different readers.
/// `response_item` lines are the model's context — that is what
/// [`Writer::payloads`] builds and what the Responses API is sent. `event_msg`
/// lines are the *history* channel: the app-server's `thread/read` reconstructs
/// turns and items exclusively from them, and the picker and transcript view are
/// built on that. Writing only `response_item` therefore produces a session that
/// resumes with its full context and displays as blank. Measured against
/// `codex-cli 0.145.0`: `thread/read` on a real rollout returns 4 turns and 22
/// items; on a rollout with no `event_msg` it returns 0 and 0.
///
/// # Why so few kinds
///
/// Every record here is a restatement of something the IR positively knows
/// happened. The rest of what Codex writes to this channel is not derivable
/// without inventing it, and an invented history is worse than a blank one — it
/// reads as evidence:
///
/// - `context_compacted` cannot come from [`Body::Compaction`]: that is a
///   history *directive*, consumed by [`crate::replay::resolve`] to rewrite the
///   history rather than replayed as part of it, so it never reaches
///   `model_visible`. It is derived instead from the two places a compaction
///   boundary is genuinely written into this file — the `compacted` envelope in
///   [`render`], and [`Body::SealedContext`] here when the blob could not be
///   carried and the marker stands in its place.
/// - `token_count` would require token counts casr does not have.
/// - `task_started` / `task_complete` carry `started_at`, `duration_ms` and a
///   `model_context_window` that only the original run knew.
/// - `turn_aborted` carries the same wall-clock fields.
/// - `sub_agent_activity` and `mcp_tool_call_end` assert a *provenance* — that a
///   call went to a sub-agent or an MCP server — that no cross-agent source
///   records. Claiming it would be a claim about the original session.
///
/// Against the 22-item real rollout used as the baseline: 18 items are restated
/// one for one (2 user messages, 16 agent messages). Its 2 compaction boundaries
/// are representable but *coarser* — the replay carries one resolved head, so
/// one envelope and one boundary are written where the original session
/// compacted twice. The last 2 — `sub_agent_activity` and `mcp_tool_call_end` —
/// are not derivable at any fidelity. What is left out is reported to the user
/// by [`Writer::summarise`] rather than left to be discovered as a blank screen.
fn history_events(event: &Event) -> Vec<Value> {
    match &event.body {
        Body::Message { role, blocks } => {
            let text = blocks
                .iter()
                .filter_map(Block::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                return Vec::new();
            }
            match role {
                // `images`, `local_images` and `text_elements` are `[]` on all
                // 1,441 corpus occurrences, and empty is the honest value here:
                // a converted session carries no Codex-side attachments.
                Role::User => vec![json!({
                    "type": "user_message",
                    "message": text,
                    "images": [],
                    "local_images": [],
                    "text_elements": [],
                })],
                // `memory_citation` is null on all 8,056 corpus occurrences.
                // `phase` is "commentary" or "final_answer" and is *not*
                // derivable — the distinction is a property of the original run,
                // so the field is omitted rather than guessed. Codex reads the
                // record without it; measured via `thread/read`.
                Role::Assistant => vec![json!({
                    "type": "agent_message",
                    "message": text,
                    "memory_citation": Value::Null,
                })],
                // Codex has no history-channel record for these, and the
                // conversation channel already carries them. `Other` is a role
                // this version does not recognise: filing it as either a user or
                // an agent message would put words in a named speaker's mouth,
                // so it is left out with the rest.
                Role::System | Role::Developer | Role::Tool | Role::Other(_) => Vec::new(),
            }
        }
        // Payload is `{"type": "context_compacted"}` and nothing else on all 388
        // corpus occurrences, so there is nothing here to invent. Keyed on the
        // sealed context rather than on `Body::Compaction` because only the
        // former survives into the replay — see this function's docs.
        Body::SealedContext { .. } => vec![json!({ "type": "context_compacted" })],
        // Enumerated rather than wildcarded, for the reason the `payloads` match
        // states at length.
        Body::Compaction { .. }
        | Body::Reasoning { .. }
        | Body::ToolCall { .. }
        | Body::ToolResult { .. }
        | Body::TurnConfig { .. }
        | Body::EnvSnapshot { .. }
        | Body::Attachment { .. }
        | Body::Rollback { .. }
        | Body::Abort { .. }
        | Body::Control { .. }
        | Body::Unknown { .. } => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Wire contract
// ---------------------------------------------------------------------------

/// The JSON type a `response_item` payload field has in a real Codex rollout.
///
/// # Why this exists rather than a comment
///
/// The structured track verifies itself by reading its own output back
/// ([`crate::pipeline::verify_structured_write`]), and that oracle is blind to
/// exactly one class of bug: a field whose *JSON type* is wrong but whose value
/// the reader is lenient enough to accept anyway. The comparison in
/// [`crate::compare`] reduces an event to its [`Body`], and
/// [`crate::ir::ToolInput::from_json_field`] maps both `"{\"a\":1}"` and
/// `{"a":1}` onto the same `ToolInput::Json` — so a writer that emitted the
/// object form round-tripped perfectly and produced a file in which Codex
/// dropped every tool call and every tool result. Nothing in the IR could see
/// it, because by then the wire shape had already been parsed away.
///
/// So the invariant is asserted where the wire shape still exists: on the
/// serialised payload, at the seam that writes it. The table is not a belief
/// about the format — every row below is the observed type of that field across
/// a 66,376-payload sample of the local Codex corpus, and every row is
/// unanimous except the two `output` fields, which really are string-or-array.
///
/// # Why the reader is *not* tightened to match
///
/// Leniency in `from_json_field` is load-bearing in two independent places, so
/// "arguments are always a string" is a per-record-type rule, not a global one:
///
/// - Claude Code records `tool_use.input` as a JSON **object**; that is the
///   only shape it ever writes, and [`crate::providers::claude_code_ir`] reads
///   it through the same helper.
/// - Codex's own `tool_search_call.arguments` and `web_search_call.action` are
///   JSON **objects** in the corpus.
///
/// A strict `from_json_field` would therefore break reading both providers to
/// protect one writer. The invariant belongs here instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireType {
    /// A JSON string, and specifically not an object that merely holds JSON.
    Str,
    Array,
    Object,
    Bool,
    /// A JSON string or a JSON array — the two shapes tool output really takes.
    StrOrArray,
}

impl WireType {
    fn accepts(self, value: &Value) -> bool {
        match self {
            WireType::Str => value.is_string(),
            WireType::Array => value.is_array(),
            WireType::Object => value.is_object(),
            WireType::Bool => value.is_boolean(),
            WireType::StrOrArray => value.is_string() || value.is_array(),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            WireType::Str => "string",
            WireType::Array => "array",
            WireType::Object => "object",
            WireType::Bool => "boolean",
            WireType::StrOrArray => "string or array",
        }
    }
}

/// Fields whose JSON type Codex depends on, per `response_item` payload type.
///
/// Only the payload types this writer emits are listed; an unlisted type is not
/// waved through silently but reported by [`wire_contract_violation`], because
/// a payload type added later with no row here is the same invisible-by-default
/// hazard this table exists to remove.
fn wire_contract(kind: &str) -> Option<&'static [(&'static str, WireType)]> {
    match kind {
        "message" => Some(&[("role", WireType::Str), ("content", WireType::Array)]),
        "function_call" => Some(&[
            ("call_id", WireType::Str),
            ("name", WireType::Str),
            // 4,093 of 4,093 corpus occurrences. The one that mattered.
            ("arguments", WireType::Str),
            ("namespace", WireType::Str),
        ]),
        "function_call_output" => Some(&[
            ("call_id", WireType::Str),
            ("output", WireType::StrOrArray),
            // Codex writes no success marker on tool output in any of the
            // 93,152 corpus outputs; this writer adds one when the source
            // recorded an outcome. Codex ignores fields it does not know but is
            // strict about the type of ones it does, so it is typed here rather
            // than left unconstrained.
            ("success", WireType::Bool),
        ]),
        "custom_tool_call" => Some(&[
            ("call_id", WireType::Str),
            ("name", WireType::Str),
            ("input", WireType::Str),
            ("status", WireType::Str),
        ]),
        "custom_tool_call_output" => Some(&[
            ("call_id", WireType::Str),
            ("output", WireType::StrOrArray),
            ("success", WireType::Bool),
        ]),
        // Echoed back verbatim from the source's structured companion, because
        // the provider-side tool catalogue is schemas rather than text.
        "tool_search_output" => Some(&[
            ("call_id", WireType::Str),
            ("execution", WireType::Str),
            ("status", WireType::Str),
            ("tools", WireType::Array),
        ]),
        "reasoning" => Some(&[
            ("summary", WireType::Array),
            ("encrypted_content", WireType::Str),
        ]),
        _ => None,
    }
}

/// Check one serialised `response_item` payload against the wire contract.
///
/// Returns the violation as prose, or `None` when the payload is shaped the way
/// Codex reads it. Fields absent from the payload are not checked — the table
/// constrains the type of what is written, not which fields must be present.
pub fn wire_contract_violation(payload: &Value) -> Option<String> {
    let map = payload.as_object()?;
    let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(fields) = wire_contract(kind) else {
        return Some(format!(
            "response_item payload type '{kind}' has no wire contract in this version, so the \
             shape Codex will read it as is unverified"
        ));
    };
    // `internal_chat_message_metadata_passthrough` is written by `with_turn` for
    // every payload type rather than per type, so it is checked here once.
    if let Some(turn) = map.get("internal_chat_message_metadata_passthrough")
        && !WireType::Object.accepts(turn)
    {
        return Some(format!(
            "{kind}.internal_chat_message_metadata_passthrough must be an object on the wire, \
             not {}",
            json_kind(turn)
        ));
    }
    for (field, expected) in fields {
        let Some(value) = map.get(*field) else {
            continue;
        };
        if !expected.accepts(value) {
            return Some(format!(
                "{kind}.{field} must be a {} on the wire, not {} — Codex fails the record's \
                 decode and drops it (a dropped `function_call` also orphans its \
                 `function_call_output`, so the tool result goes with it)",
                expected.describe(),
                json_kind(value),
            ));
        }
    }
    None
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Attach the turn id the way Codex does.
///
/// `internal_chat_message_metadata_passthrough` holds exactly one key across
/// all 319,184 corpus payloads that carry it, and it is where `turn_id` lives.
fn with_turn(event: &Event, mut payload: Value) -> Value {
    if let (Some(turn), Some(map)) = (event.turn.as_ref(), payload.as_object_mut()) {
        map.insert(
            "internal_chat_message_metadata_passthrough".into(),
            json!({ "turn_id": turn }),
        );
    }
    payload
}

fn sealed_context_marker() -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": SEALED_CONTEXT_MARKER}],
    })
}

fn role_string(role: &Role) -> String {
    match role {
        Role::User => "user".to_string(),
        Role::Assistant => "assistant".to_string(),
        Role::System => "system".to_string(),
        Role::Developer => "developer".to_string(),
        Role::Tool => "tool".to_string(),
        Role::Other(other) => other.clone(),
    }
}

fn stamp(ts: Option<i64>, fallback: &str) -> String {
    ts.and_then(DateTime::from_timestamp_millis)
        .map(|when| when.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Branch, CapsuleBinding, SourceRef, Visibility};

    fn event(id: &str, body: Body) -> Event {
        Event {
            id: id.to_string(),
            parent: None,
            branch: Branch::Main,
            turn: Some("t1".to_string()),
            ts: None,
            visibility: Visibility::Model,
            body,
            capsules: Vec::new(),
            source: SourceRef {
                line: 1,
                sha256: String::new(),
            },
        }
    }

    fn ir(events: Vec<Event>) -> SessionIr {
        let mut ir = SessionIr::new(SAME_AGENT, "s1");
        ir.origin.provider = Some("openai".to_string());
        ir.events = events;
        ir
    }

    fn rendered(ir: &SessionIr) -> Rendered {
        render(ir, "sid", Utc::now(), &ContextBudget::UNLIMITED).expect("non-empty replay")
    }

    fn payload_types(out: &Rendered) -> Vec<String> {
        out.lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|value| {
                value["payload"]["type"]
                    .as_str()
                    .map(str::to_string)
                    .filter(|kind| kind.contains("tool_call"))
            })
            .collect()
    }

    /// The IR the budget tests below render, deliberately small and with one
    /// oversized tool observation.
    fn budget_source() -> SessionIr {
        ir(vec![
            event(
                "u",
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "fix the build".into(),
                    }],
                },
            ),
            event(
                "c",
                Body::ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    namespace: None,
                    input: ToolInput::Freeform {
                        text: "cargo build".into(),
                    },
                },
            ),
            event(
                "o",
                Body::ToolResult {
                    call_id: "c1".into(),
                    outcome: ToolOutcome::Unknown,
                    output: vec![Block::Text {
                        text: "E".repeat(50),
                    }],
                    structured: None,
                },
            ),
            event(
                "a",
                Body::Message {
                    role: Role::Assistant,
                    blocks: vec![Block::Text {
                        text: "fixed".into(),
                    }],
                },
            ),
        ])
    }

    /// A fixed clock, so the rendering is a pure function of the IR.
    fn fixed_now() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid epoch millis")
    }

    /// Flags absent must mean *these bytes*, not merely equivalent ones.
    ///
    /// The codex→codex round trip is verified event-for-event, so a budget that
    /// reshaped the output even when switched off would break a guarantee this
    /// crate sells. Spelled out as bytes rather than as properties because a
    /// property test cannot fail on a field that quietly moved.
    #[test]
    fn an_absent_budget_writes_the_bytes_it_wrote_before_there_was_a_budget() {
        let out = render(
            &budget_source(),
            "sid",
            fixed_now(),
            &ContextBudget::UNLIMITED,
        )
        .expect("non-empty replay");

        let version = env!("CARGO_PKG_VERSION");
        let expected = [
            format!(
                r#"{{"payload":{{"base_instructions":null,"cli_version":"{version}","context_window":null,"cwd":"/tmp","history_mode":"legacy","id":"sid","model_provider":"openai","originator":"casr","session_id":"sid","source":"cli","thread_source":"user","timestamp":"2023-11-14T22:13:20.000Z"}},"timestamp":"2023-11-14T22:13:20.000Z","type":"session_meta"}}"#
            ),
            r#"{"payload":{"content":[{"text":"fix the build","type":"input_text"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"},"role":"user","type":"message"},"timestamp":"2023-11-14T22:13:20.000Z","type":"response_item"}"#.to_string(),
            // The history channel, immediately after the conversation record it
            // restates. `event_msg` carries no turn-id passthrough — Codex does
            // not write one on any of the 9,497 corpus occurrences.
            r#"{"payload":{"images":[],"local_images":[],"message":"fix the build","text_elements":[],"type":"user_message"},"timestamp":"2023-11-14T22:13:20.000Z","type":"event_msg"}"#.to_string(),
            r#"{"payload":{"call_id":"c1","input":"cargo build","internal_chat_message_metadata_passthrough":{"turn_id":"t1"},"name":"shell","status":"completed","type":"custom_tool_call"},"timestamp":"2023-11-14T22:13:20.000Z","type":"response_item"}"#.to_string(),
            format!(
                r#"{{"payload":{{"call_id":"c1","internal_chat_message_metadata_passthrough":{{"turn_id":"t1"}},"output":"{}","type":"custom_tool_call_output"}},"timestamp":"2023-11-14T22:13:20.000Z","type":"response_item"}}"#,
                "E".repeat(50)
            ),
            r#"{"payload":{"content":[{"text":"fixed","type":"output_text"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"},"role":"assistant","type":"message"},"timestamp":"2023-11-14T22:13:20.000Z","type":"response_item"}"#.to_string(),
            r#"{"payload":{"memory_citation":null,"message":"fixed","type":"agent_message"},"timestamp":"2023-11-14T22:13:20.000Z","type":"event_msg"}"#.to_string(),
        ];
        assert_eq!(out.lines, expected);
        assert!(out.losses.is_empty(), "{:?}", out.losses);
        assert_eq!(out.fidelity, Fidelity::ContextComplete);
    }

    /// The same IR, a cap that binds: the tail survives, the head is reported.
    #[test]
    fn a_cap_keeps_the_tail_and_reports_what_it_dropped() {
        let out = render(
            &budget_source(),
            "sid",
            fixed_now(),
            &ContextBudget {
                // Each of these four events costs 30-60 tokens; 60 buys the last.
                max_context_tokens: 60,
                max_tool_output: 0,
                keep_reasoning: true,
            },
        )
        .expect("non-empty replay");

        // `response_item` only: the history channel restates the same turn on a
        // second line, and this test is about how much conversation survived the
        // cap, not about how many lines carry it.
        let texts: Vec<String> = out
            .lines
            .iter()
            .filter(|line| line.contains(r#""type":"response_item""#))
            .map(|line| line.to_string())
            .collect();
        assert_eq!(texts.len(), 1, "only the newest turn fits: {texts:?}");
        assert!(
            texts[0].contains("fixed"),
            "the tail is what a resume needs"
        );
        assert!(
            !out.lines
                .iter()
                .any(|line| line.contains("custom_tool_call")),
            "the call and its output went together; an orphan is rejected at replay"
        );

        let kinds: Vec<LossKind> = out.losses.iter().map(|loss| loss.kind).collect();
        assert!(kinds.contains(&LossKind::Conversation), "{kinds:?}");
        assert!(kinds.contains(&LossKind::ToolProtocol), "{kinds:?}");
        assert_eq!(
            out.fidelity,
            Fidelity::HistoryIncomplete,
            "the grade is folded from the losses, budget losses included"
        );
        assert!(
            out.warnings
                .iter()
                .any(|note| note.contains("context budget")),
            "a trim the user cannot see is the flat track's silence again: {:?}",
            out.warnings
        );
    }

    #[test]
    fn an_oversized_observation_is_elided_in_place_not_dropped() {
        let out = render(
            &budget_source(),
            "sid",
            fixed_now(),
            &ContextBudget {
                max_context_tokens: 0,
                max_tool_output: 20,
                keep_reasoning: true,
            },
        )
        .expect("non-empty replay");

        let output = out
            .lines
            .iter()
            .find(|line| line.contains("custom_tool_call_output"))
            .expect("the tool result kept its event, its call_id and its outcome");
        assert!(output.contains("elided"), "{output}");
        assert_eq!(out.losses.len(), 1);
        assert_eq!(out.losses[0].kind, LossKind::ToolProtocol);
        assert_eq!(
            out.fidelity,
            Fidelity::ConversationOnly,
            "an elision that announces itself is a degraded observation, not a \
             hole in the conversation"
        );
    }

    #[test]
    fn empty_replay_writes_nothing() {
        assert!(
            render(
                &ir(Vec::new()),
                "sid",
                Utc::now(),
                &ContextBudget::UNLIMITED
            )
            .is_none()
        );
    }

    #[test]
    fn every_response_item_carries_the_turn_id() {
        let out = rendered(&ir(vec![event(
            "a",
            Body::Message {
                role: Role::User,
                blocks: vec![Block::Text { text: "hi".into() }],
            },
        )]));
        // Every `response_item`, which is what this asserts — the last line is
        // now the history-channel restatement, and Codex writes no turn-id
        // passthrough on `event_msg` in any of the 9,497 corpus occurrences.
        let items: Vec<Value> = out
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["type"] == json!("response_item"))
            .collect();
        assert!(!items.is_empty(), "the replay produced no conversation");
        for item in &items {
            assert_eq!(
                item["payload"]["internal_chat_message_metadata_passthrough"]["turn_id"],
                json!("t1")
            );
        }
        assert_eq!(out.fidelity, Fidelity::ContextComplete);
    }

    #[test]
    fn freeform_calls_keep_their_protocol_and_pair_their_output() {
        let out = rendered(&ir(vec![
            event(
                "call",
                Body::ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    namespace: None,
                    input: ToolInput::Freeform {
                        text: "ls -la".into(),
                    },
                },
            ),
            event(
                "out",
                Body::ToolResult {
                    call_id: "c1".into(),
                    outcome: ToolOutcome::Unknown,
                    output: vec![Block::Text { text: "ok".into() }],
                    structured: None,
                },
            ),
        ]));
        assert_eq!(
            payload_types(&out),
            ["custom_tool_call", "custom_tool_call_output"],
            "collapsing a freeform call into a function_call is a one-way downgrade"
        );
    }

    #[test]
    fn a_foreign_reasoning_capsule_is_dropped_not_stubbed() {
        let mut reasoning = event(
            "r",
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        reasoning.capsules.push(Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".into(),
                model: None,
            },
            sealed: "AAAA".into(),
        });
        let mut source = ir(vec![
            reasoning,
            event(
                "m",
                Body::Message {
                    role: Role::Assistant,
                    blocks: vec![Block::Text {
                        text: "done".into(),
                    }],
                },
            ),
        ]);
        source.origin.agent = "claude-code".into();

        let out = rendered(&source);
        assert_eq!(out.fidelity, Fidelity::ContextNoReasoning);
        assert!(
            !out.lines.iter().any(|line| line.contains("reasoning")),
            "an empty reasoning husk costs context and tells the model it was truncated"
        );
    }

    #[test]
    fn sealed_context_rides_back_inside_a_compacted_envelope() {
        let mut sealed = event(
            "cmp",
            Body::SealedContext {
                native_id: Some("cmp_1".into()),
                meta: Value::Null,
            },
        );
        sealed.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiCompactedContext,
            bound: CapsuleBinding {
                provider: "sub2api".into(),
                model: None,
            },
            sealed: "CCCC".into(),
        });
        let source = ir(vec![
            sealed,
            event(
                "c",
                Body::Compaction {
                    context: vec!["cmp".into()],
                    supersedes: Vec::new(),
                    note: None,
                    window_from: None,
                    window_to: None,
                },
            ),
            event(
                "after",
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "next".into(),
                    }],
                },
            ),
        ]);

        let out = rendered(&source);
        let compacted: Value = out
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == json!("compacted"))
            .expect("a sealed compaction must go back inside `compacted`");
        assert_eq!(
            compacted["payload"]["replacement_history"][0]["encrypted_content"],
            json!("CCCC"),
            "the blob must be byte-identical, not re-encoded"
        );
        assert_eq!(
            out.fidelity,
            Fidelity::ContextComplete,
            "a gateway endpoint does not make an OpenAI blob unreplayable"
        );
    }

    #[test]
    fn a_lost_sealed_compaction_leaves_a_marker_and_says_so() {
        let mut sealed = event(
            "cmp",
            Body::SealedContext {
                native_id: None,
                meta: Value::Null,
            },
        );
        sealed.capsules.push(Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".into(),
                model: None,
            },
            sealed: "AAAA".into(),
        });
        let mut source = ir(vec![sealed]);
        source.origin.agent = "claude-code".into();

        let out = rendered(&source);
        assert_eq!(out.fidelity, Fidelity::HistoryIncomplete);
        assert!(
            out.lines
                .iter()
                .any(|line| line.contains("[converted by casr]")),
            "omitting the hole silently is the worst option available"
        );
    }
}
