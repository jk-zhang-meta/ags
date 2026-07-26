//! Codex → structured IR.
//!
//! The flat reader in [`super::codex`] keeps its own path; this is the
//! high-fidelity one. It is a separate module because the two have genuinely
//! different jobs — one produces text for providers that only need text, the
//! other preserves the structure Codex actually records — and because
//! `codex.rs` is already large enough without a second reader inside it.
//!
//! # Mapping
//!
//! Derived from a census of 590 real rollouts (Codex 0.145.0), not from the
//! format documentation, because the two disagree. Line counts below are from
//! that corpus and are what justify each decision.
//!
//! | Native line | IR | Visibility | Why |
//! |---|---|---|---|
//! | `response_item` (317k) | per payload, below | `Model` | this *is* the model's context |
//! | `event_msg` (925k) | per payload, below | `Ui`/`Telemetry` | rendering and accounting, not context |
//! | `turn_context` (15k) | [`Body::TurnConfig`] | `Ui` | model/effort/sandbox/approval per turn |
//! | `world_state` (8.7k) | [`Body::EnvSnapshot`] | `Ui` | AGENTS.md, skills, plugins |
//! | `compacted` (4.8k) | [`Body::Compaction`] + the replacement as events | `Model` | rewrites the model's history; 4,325 of its replacement entries are sealed [`Body::SealedContext`] rather than messages |
//! | `inter_agent_communication_metadata` (5.7k) | [`Body::Control`] | `Ui` | subagent plumbing |
//! | `session_meta` (948) | [`Origin`]/[`Workspace`] | — | header, not an event |
//!
//! `response_item` payloads:
//!
//! | Payload | IR | Note |
//! |---|---|---|
//! | `reasoning` (103k) | [`Body::Reasoning`] + [`Capsule`] | `summary` is empty in **all** 30930 sampled items; the content is in `encrypted_content` |
//! | `custom_tool_call{,_output}` (61k each) | tool events, [`ToolProtocol::Freeform`] | upstream rewrites these to `function_call`, which is not reversible |
//! | `message` (37k) | [`Body::Message`] | |
//! | `function_call{,_output}` (24k each) | tool events, [`ToolProtocol::JsonArgs`] | |
//! | `agent_message` (5.7k) | [`Body::Message`] as assistant | |
//!
//! `event_msg` is deliberately **not** promoted to `Model`. It largely
//! duplicates `response_item` content for rendering; treating it as context
//! would double every assistant message. `token_count` alone is 673k of the
//! 925k lines and is pure accounting.
//!
//! Two `event_msg` payloads are exceptions to "rendering only", because they
//! edit the model's history rather than describe it: `thread_rolled_back` (714)
//! becomes [`Body::Rollback`] and `turn_aborted` (2,304) becomes
//! [`Body::Abort`]. They keep `Visibility::Ui` — that is genuinely what Codex
//! recorded — and [`crate::replay::resolve`] reads both before its visibility
//! gate. Typing them here rather than leaving them as [`Body::Control`] strings
//! is what keeps Codex's wire vocabulary out of the provider-agnostic resolver.
//!
//! Note for anyone porting other converters: `agent_reasoning` does not appear
//! in this corpus at all. A reader that sources reasoning from that event finds
//! nothing on current Codex and, if it does not treat unknown shapes as
//! [`Body::Unknown`], reports success while silently producing none.

use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ir::{
    Block, Body, Branch, Capsule, CapsuleBinding, CapsuleKind, Event, SessionIr, SourceRef,
    Role, ToolInput, ToolOutcome, Visibility,
};
use crate::model::parse_timestamp;

/// Parse a Codex rollout into the structured IR.
pub fn read(path: &Path) -> anyhow::Result<SessionIr> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open Codex rollout {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut ir = SessionIr::new("codex", String::new());
    let mut builder = Builder::default();

    for (index, line) in reader.lines().enumerate() {
        let line = line
            .with_context(|| format!("failed to read line {} of {}", index + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        ir.capture.lines_read += 1;
        let source = SourceRef {
            line: index as u64 + 1,
            sha256: format!("{:x}", Sha256::digest(line.as_bytes())),
        };

        // A line that will not parse as JSON is still a line that existed.
        // Recording it as Unknown keeps the count honest; skipping it would
        // make a corrupt rollout look like a short one.
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                builder.push_unknown(
                    &mut ir,
                    source,
                    None,
                    Value::String(format!("unparseable JSON: {error}")),
                );
                continue;
            }
        };

        let ts = value.get("timestamp").and_then(parse_timestamp);
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => apply_session_meta(&mut ir, &value),
            Some("turn_context") => builder.push_turn_context(&mut ir, source, ts, &value),
            Some("response_item") => builder.push_response_item(&mut ir, source, ts, &value),
            Some("event_msg") => builder.push_event_msg(&mut ir, source, ts, &value),
            Some("compacted") => builder.push_compacted(&mut ir, source, ts, &value),
            Some("world_state") => builder.push_simple(
                &mut ir,
                source,
                ts,
                Visibility::Ui,
                Body::EnvSnapshot {
                    data: payload(&value),
                },
            ),
            Some("inter_agent_communication_metadata") => builder.push_simple(
                &mut ir,
                source,
                ts,
                Visibility::Ui,
                Body::Control {
                    control_kind: "inter_agent_communication".to_string(),
                    data: payload(&value),
                },
            ),
            other => builder.push_unknown(&mut ir, source, other.map(str::to_string), value),
        }
    }

    if ir.origin.native_session_id.is_empty() {
        anyhow::bail!(
            "{} has no session_meta line; refusing to guess the session id",
            path.display()
        );
    }
    Ok(ir)
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Per-file parse state: id allocation, DAG chaining, and compaction scope.
#[derive(Default)]
struct Builder {
    next_index: u64,
    /// Id of the previous model-visible event, used as the DAG parent.
    last_model: Option<String>,
    /// Model-visible ids not yet superseded by a compaction.
    live_model: Vec<String>,
    turn: Option<String>,
}

impl Builder {
    fn allocate_id(&mut self, source: &SourceRef) -> String {
        self.next_index += 1;
        // Line-derived rather than random so a given rollout always yields the
        // same ids, which keeps IR diffs readable and tests deterministic. The
        // counter disambiguates the lines that expand into several events.
        format!("cx-{:06}-{}", source.line, self.next_index)
    }

    fn emit(&mut self, ir: &mut SessionIr, event: Event) {
        if event.visibility == Visibility::Model {
            self.last_model = Some(event.id.clone());
            self.live_model.push(event.id.clone());
        }
        ir.capture.record(&event);
        ir.events.push(event);
    }

    fn build(
        &mut self,
        source: SourceRef,
        ts: Option<i64>,
        visibility: Visibility,
        body: Body,
        capsules: Vec<Capsule>,
    ) -> Event {
        let id = self.allocate_id(&source);
        Event {
            id,
            parent: if visibility == Visibility::Model {
                self.last_model.clone()
            } else {
                None
            },
            branch: Branch::Main,
            turn: self.turn.clone(),
            ts,
            visibility,
            body,
            capsules,
            source,
        }
    }

    fn push_simple(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        ts: Option<i64>,
        visibility: Visibility,
        body: Body,
    ) {
        let event = self.build(source, ts, visibility, body, Vec::new());
        self.emit(ir, event);
    }

    fn push_unknown(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        native_type: Option<String>,
        raw: Value,
    ) {
        let body = Body::Unknown { native_type, raw };
        let event = self.build(source, None, Visibility::Unclassified, body, Vec::new());
        self.emit(ir, event);
    }

    fn push_turn_context(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        ts: Option<i64>,
        value: &Value,
    ) {
        let payload = payload(value);
        self.turn = payload
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if ir.origin.model.is_none() {
            ir.origin.model = payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if ir.workspace.roots.is_empty()
            && let Some(roots) = payload.get("workspace_roots").and_then(Value::as_array)
        {
            ir.workspace.roots = roots
                .iter()
                .filter_map(Value::as_str)
                .map(Into::into)
                .collect();
        }

        let body = Body::TurnConfig {
            model: string_field(&payload, "model"),
            effort: string_field(&payload, "effort"),
            sandbox: payload.get("sandbox_policy").cloned(),
            approval: payload.get("approval_policy").cloned(),
            personality: payload.get("personality").cloned(),
            instructions: string_field(&payload, "user_instructions"),
        };
        self.push_simple(ir, source, ts, Visibility::Ui, body);
    }

    fn push_response_item(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        ts: Option<i64>,
        value: &Value,
    ) {
        let payload = payload(value);
        if let Some(turn) = payload
            .get("internal_chat_message_metadata_passthrough")
            .and_then(|meta| meta.get("turn_id"))
            .and_then(Value::as_str)
        {
            self.turn = Some(turn.to_string());
        }

        let Some(kind) = payload.get("type").and_then(Value::as_str) else {
            self.push_unknown(ir, source, Some("response_item".into()), value.clone());
            return;
        };

        match kind {
            "message" | "agent_message" => {
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .map(Role::from_native)
                    // `agent_message` carries no role; it is always the agent.
                    .unwrap_or(Role::Assistant);
                let bound = capsule_binding(ir);
                let (blocks, capsules) = content_parts(payload.get("content"), Some(&bound));
                let body = Body::Message { role, blocks };
                let event = self.build(source, ts, Visibility::Model, body, capsules);
                self.emit(ir, event);
            }

            "reasoning" => {
                // `summary` is empty in every sampled item; the substance is in
                // `encrypted_content`, which only OpenAI can read. Carry it
                // verbatim and let the writer decide whether it may be replayed.
                let capsules = payload
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|sealed| !sealed.is_empty())
                    .map(|sealed| {
                        vec![Capsule {
                            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
                            bound: CapsuleBinding {
                                provider: ir
                                    .origin
                                    .provider
                                    .clone()
                                    .unwrap_or_else(|| "openai".to_string()),
                                model: ir.origin.model.clone(),
                            },
                            sealed: sealed.to_string(),
                        }]
                    })
                    .unwrap_or_default();

                let body = Body::Reasoning {
                    text: None,
                    summary: payload
                        .get("summary")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.get("text").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                let event = self.build(source, ts, Visibility::Model, body, capsules);
                self.emit(ir, event);
            }

            "function_call" | "custom_tool_call" => {
                // `function_call` puts JSON in `arguments`; `custom_tool_call`
                // puts an opaque string in `input`. Both arrive as strings on
                // the wire, so the distinction lives in the type rather than
                // in a guess about the payload's shape.
                let input = if kind == "custom_tool_call" {
                    ToolInput::Freeform {
                        text: string_field(&payload, "input").unwrap_or_default(),
                    }
                } else {
                    ToolInput::from_json_field(
                        payload.get("arguments").unwrap_or(&Value::Null),
                    )
                };
                let body = Body::ToolCall {
                    call_id: string_field(&payload, "call_id").unwrap_or_default(),
                    name: string_field(&payload, "name").unwrap_or_else(|| "unknown".to_string()),
                    namespace: string_field(&payload, "namespace"),
                    input,
                };
                self.push_simple(ir, source, ts, Visibility::Model, body);
            }

            "function_call_output" | "custom_tool_call_output" => {
                let output = payload.get("output");
                let body = Body::ToolResult {
                    call_id: string_field(&payload, "call_id").unwrap_or_default(),
                    outcome: tool_outcome(&payload),
                    output: blocks_from_content(output),
                    structured: output.filter(|value| !value.is_string()).cloned(),
                };
                self.push_simple(ir, source, ts, Visibility::Model, body);
            }

            // Provider-side built-in tools. These are ordinary tool activity
            // in the model's context, but they carry no `call_id` of their own
            // in every case — `web_search_call` identifies itself with `id`
            // alone — so the pairing key falls back to `id`.
            "web_search_call" | "tool_search_call" => {
                let name = if kind == "web_search_call" {
                    "web_search"
                } else {
                    "tool_search"
                };
                let input = ToolInput::from_json_field(
                    payload
                        .get("arguments")
                        .or_else(|| payload.get("action"))
                        .unwrap_or(&Value::Null),
                );
                let body = Body::ToolCall {
                    call_id: string_field(&payload, "call_id")
                        .or_else(|| string_field(&payload, "id"))
                        .unwrap_or_default(),
                    name: name.to_string(),
                    namespace: Some("builtin".to_string()),
                    input,
                };
                self.push_simple(ir, source, ts, Visibility::Model, body);
            }

            "tool_search_output" => {
                let body = Body::ToolResult {
                    call_id: string_field(&payload, "call_id")
                        .or_else(|| string_field(&payload, "id"))
                        .unwrap_or_default(),
                    outcome: tool_outcome(&payload),
                    output: Vec::new(),
                    // The payload is a tool catalogue, not text. Flattening it
                    // would destroy the schemas; keep it whole.
                    structured: Some(payload.clone()),
                };
                self.push_simple(ir, source, ts, Visibility::Model, body);
            }

            other => {
                // Anything a future release adds. Model-visible but not
                // understood: keep the bytes and let the count show up in the
                // capture report so the gap is a to-do, not a silent loss.
                let body = Body::Unknown {
                    native_type: Some(format!("response_item.{other}")),
                    raw: payload.clone(),
                };
                let event = self.build(source, ts, Visibility::Model, body, Vec::new());
                self.emit(ir, event);
            }
        }
    }

    fn push_event_msg(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        ts: Option<i64>,
        value: &Value,
    ) {
        let payload = payload(value);
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if let Some(turn) = payload.get("turn_id").and_then(Value::as_str) {
            self.turn = Some(turn.to_string());
        }

        // `token_count` is 673k of the 925k event_msg lines in the corpus and
        // is pure accounting. Everything else here is rendering: it duplicates
        // `response_item` content rather than adding to it.
        let visibility = if kind == "token_count" {
            Visibility::Telemetry
        } else {
            Visibility::Ui
        };

        // Two of these are not rendering at all: they edit the model's history,
        // and the resolver has to act on them. Mapping them here — where
        // `thread_rolled_back` and `turn_aborted` are already understood — is
        // what keeps `crate::replay` free of Codex's wire vocabulary. The
        // visibility is deliberately left as Codex recorded it: these really
        // are `event_msg` and really are chrome to render, and the resolver
        // reads them before its visibility gate rather than pretending
        // otherwise here.
        let body = match kind.as_str() {
            "thread_rolled_back" => Body::Rollback {
                // 1 in all 714 corpus occurrences. Absent means 1 rather than
                // "everything": a rollback that cannot say how far it goes must
                // not be allowed to eat the session.
                turns: payload
                    .get("num_turns")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, u32::MAX as u64) as u32,
            },
            "turn_aborted" => Body::Abort {},
            _ => Body::Control {
                control_kind: kind,
                data: payload,
            },
        };
        self.push_simple(ir, source, ts, visibility, body);
    }

    fn push_compacted(
        &mut self,
        ir: &mut SessionIr,
        source: SourceRef,
        ts: Option<i64>,
        value: &Value,
    ) {
        let payload = payload(value);

        // A compaction is a *state assignment* — `crate::replay::resolve` does
        // `live := context` and never diffs — so a record that does not carry
        // an array `replacement_history` is not an instruction to empty the
        // context. It is a record this reader does not understand, and applying
        // it would delete the entire conversation on the strength of a field
        // that was not there. All 4,866 corpus `compacted` records carry the
        // array, so this is the unmeasured case, and it fails loudly and
        // non-destructively rather than quietly and catastrophically.
        let Some(history) = payload.get("replacement_history").and_then(Value::as_array) else {
            let line = source.line;
            self.push_unknown(ir, source, Some("compacted".into()), value.clone());
            ir.capture.note(format!(
                "line {line} is a `compacted` record with no array `replacement_history`; \
                 the live context was kept rather than superseded by nothing"
            ));
            return;
        };

        // Codex does not name the events a compaction supersedes: the semantics
        // are "everything model-visible up to here is replaced by
        // replacement_history". So the scope is every live model event, and
        // after this they are no longer live.
        let supersedes = std::mem::take(&mut self.live_model);

        // Emit the substituted history as ordinary events *before* the marker,
        // so they are counted, become the new live set, and are superseded in
        // turn by whatever compaction comes next. Because `live_model` was just
        // emptied, `emit` refills it with exactly these ids — which is also the
        // post-compaction context the resolver assigns.
        let mut context = Vec::new();
        let bound = capsule_binding(ir);
        for item in history {
            let event = self.replacement_event(&source, ts, item, &bound);
            context.push(event.id.clone());
            self.emit(ir, event);
        }

        let body = Body::Compaction {
            context,
            supersedes,
            note: string_field(&payload, "message").filter(|note| !note.is_empty()),
            window_from: string_field(&payload, "previous_window_id"),
            window_to: string_field(&payload, "window_id"),
        };
        let event = self.build(source, ts, Visibility::Model, body, Vec::new());
        self.emit(ir, event);
    }

    /// One entry of `replacement_history`.
    ///
    /// Two shapes, and the second is the one that matters. 168,467 entries in
    /// the corpus are ordinary `message`s; 4,325 are `type: "compaction"` with
    /// no `role` and no `content`, carrying an `encrypted_content` blob that
    /// totals 87.6 MB. Treating the second shape as a message yields an empty
    /// assistant turn where the entire pre-compaction conversation should be —
    /// which is what this reader used to do.
    ///
    /// Each entry also carries its **own** turn, and it is not the builder's.
    /// All 176,027 corpus entries have
    /// `internal_chat_message_metadata_passthrough.turn_id`, and one `compacted`
    /// record routinely spans dozens of distinct ones — 763 of them span 17.
    /// [`Event::turn`] is what [`crate::replay::roll_back`] counts as "the last
    /// N typed turns", so stamping the builder's current turn on every entry
    /// collapses the whole compacted history into a single turn and lets one
    /// rollback undo all of it. The same collapse was already fixed once on the
    /// Claude side, in [`super::claude_code_ir::is_restatement`].
    fn replacement_event(
        &mut self,
        source: &SourceRef,
        ts: Option<i64>,
        item: &Value,
        bound: &CapsuleBinding,
    ) -> Event {
        let turn = item
            .get("internal_chat_message_metadata_passthrough")
            .and_then(|meta| meta.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.turn.clone());

        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            let capsules = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|sealed| !sealed.is_empty())
                .map(|sealed| {
                    vec![Capsule {
                        kind: CapsuleKind::OpenaiCompactedContext,
                        bound: bound.clone(),
                        sealed: sealed.to_string(),
                    }]
                })
                .unwrap_or_default();
            let body = Body::SealedContext {
                native_id: string_field(item, "id"),
                meta: item
                    .get("internal_chat_message_metadata_passthrough")
                    .cloned()
                    .unwrap_or(Value::Null),
            };
            let mut event = self.build(source.clone(), ts, Visibility::Model, body, capsules);
            event.turn = turn;
            return event;
        }

        let role = item
            .get("role")
            .and_then(Value::as_str)
            .map(Role::from_native)
            .unwrap_or(Role::Assistant);
        let body = Body::Message {
            role,
            blocks: blocks_from_content(item.get("content")),
        };
        let mut event = self.build(source.clone(), ts, Visibility::Model, body, Vec::new());
        event.turn = turn;
        event
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn apply_session_meta(ir: &mut SessionIr, value: &Value) {
    let payload = payload(value);
    if let Some(id) = payload.get("id").and_then(Value::as_str) {
        ir.origin.native_session_id = id.to_string();
    }
    if let Some(version) = payload.get("cli_version").and_then(Value::as_str) {
        ir.origin.agent_version = Some(version.to_string());
    }
    if let Some(provider) = payload.get("model_provider").and_then(Value::as_str) {
        ir.origin.provider = Some(provider.to_string());
    }
    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
        ir.workspace.cwd = Some(cwd.into());
    }
    if let Some(git) = payload.get("git") {
        ir.workspace.git_branch = git
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_string);
        ir.workspace.git_commit = git
            .get("commit_hash")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    ir.origin.captured_at = payload.get("timestamp").and_then(parse_timestamp);
}

/// The capsule binding for this session: which endpoint served it, and which
/// model. Informational — replay compatibility is decided by
/// [`CapsuleKind::vendor`], not by this.
fn capsule_binding(ir: &SessionIr) -> CapsuleBinding {
    CapsuleBinding {
        provider: ir
            .origin
            .provider
            .clone()
            .unwrap_or_else(|| "openai".to_string()),
        model: ir.origin.model.clone(),
    }
}

fn payload(value: &Value) -> Value {
    value.get("payload").cloned().unwrap_or(Value::Null)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Whether a tool output records a failure.
///
/// Upstream hardcodes success here, which turns every failed command in a
/// converted history into an apparently successful one. Codex has no single
/// error flag, so check the shapes it actually uses.
fn tool_outcome(payload: &Value) -> ToolOutcome {
    if payload.get("success").and_then(Value::as_bool) == Some(false)
        || payload
            .get("output")
            .and_then(|output| output.get("success"))
            .and_then(Value::as_bool)
            == Some(false)
    {
        return ToolOutcome::Failed;
    }
    if let Some(status) = payload.get("status").and_then(Value::as_str)
        && (status == "failed" || status == "error")
    {
        return ToolOutcome::Failed;
    }
    // 3,760 outputs in the corpus encode their result as a JSON string with a
    // `timed_out` flag and nothing else. It is the only failure signal Codex
    // actually writes today.
    if let Some(text) = payload.get("output").and_then(Value::as_str)
        && let Ok(inner) = serde_json::from_str::<Value>(text)
        && inner.get("timed_out").and_then(Value::as_bool) == Some(true)
    {
        return ToolOutcome::Failed;
    }
    if payload.get("success").and_then(Value::as_bool) == Some(true) {
        return ToolOutcome::Succeeded;
    }
    // Codex writes no success marker on tool output at all: not `success`,
    // not `status`, not an exit code, in any of the 85,584 outputs sampled.
    // Calling that a success is a claim the rollout does not make.
    ToolOutcome::Unknown
}

/// Convert a Codex content value into IR blocks.
fn blocks_from_content(content: Option<&Value>) -> Vec<Block> {
    content_parts(content, None).0
}

/// Split a `content` array into replayable blocks and sealed capsules.
///
/// `agent_message` content carries `encrypted_content` blocks — 4,562 of them
/// in the corpus — which are sealed material, not text. They belong in
/// [`Event::capsules`] beside reasoning, not in the block list and certainly
/// not on the floor, which is where the previous `filter_map` put them.
fn content_parts(content: Option<&Value>, bound: Option<&CapsuleBinding>) -> (Vec<Block>, Vec<Capsule>) {
    let Some(content) = content else {
        return (Vec::new(), Vec::new());
    };
    match content {
        Value::String(text) => (vec![Block::Text { text: text.clone() }], Vec::new()),
        Value::Null => (Vec::new(), Vec::new()),
        Value::Array(items) => {
            let mut blocks = Vec::new();
            let mut capsules = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("encrypted_content") {
                    if let (Some(bound), Some(sealed)) = (
                        bound,
                        item.get("encrypted_content")
                            .and_then(Value::as_str)
                            .filter(|sealed| !sealed.is_empty()),
                    ) {
                        capsules.push(Capsule {
                            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
                            bound: bound.clone(),
                            sealed: sealed.to_string(),
                        });
                        continue;
                    }
                }
                blocks.push(block_from_item(item));
            }
            (blocks, capsules)
        }
        other => (
            vec![Block::Text {
                text: other.to_string(),
            }],
            Vec::new(),
        ),
    }
}

fn block_from_item(item: &Value) -> Block {
    let native_type = item.get("type").and_then(Value::as_str);
    if native_type == Some("input_image")
        && let Some(url) = item.get("image_url").and_then(Value::as_str)
    {
        return Block::Image {
            url: url.to_string(),
            media_type: None,
        };
    }
    // input_text / output_text / summary_text and anything else carrying
    // `text` all project to the same thing.
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Block::Text {
            text: text.to_string(),
        };
    }
    Block::Unknown {
        native_type: native_type.map(str::to_string),
        raw: item.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CapsuleFit, ToolProtocol};
    use std::io::Write;

    fn rollout(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        file.flush().expect("flush");
        file
    }

    const META: &str = r#"{"timestamp":"2026-07-25T10:00:00.000Z","type":"session_meta","payload":{"id":"019f9906-b8f7-7cb2-85d5-386512c066d4","cli_version":"0.145.0","model_provider":"openai","cwd":"/work","timestamp":"2026-07-25T10:00:00.000Z"}}"#;

    #[test]
    fn requires_session_meta() {
        let file = rollout(&[r#"{"type":"event_msg","payload":{"type":"token_count"}}"#]);
        let error = read(file.path()).expect_err("must refuse a rollout with no session_meta");
        assert!(error.to_string().contains("session_meta"));
    }

    #[test]
    fn reads_header_into_origin_and_workspace() {
        let file = rollout(&[META]);
        let ir = read(file.path()).expect("parse");
        assert_eq!(ir.origin.native_session_id, "019f9906-b8f7-7cb2-85d5-386512c066d4");
        assert_eq!(ir.origin.agent_version.as_deref(), Some("0.145.0"));
        assert_eq!(ir.origin.provider.as_deref(), Some("openai"));
        assert_eq!(ir.workspace.cwd.as_deref(), Some(Path::new("/work")));
    }

    #[test]
    fn reasoning_capsule_is_carried_verbatim() {
        let sealed = "EpYCCokBCA8YtestpayloadAAAA";
        let file = rollout(&[
            META,
            &format!(
                r#"{{"timestamp":"2026-07-25T10:00:01.000Z","type":"response_item","payload":{{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"{sealed}"}}}}"#
            ),
        ]);
        let ir = read(file.path()).expect("parse");
        let reasoning = ir
            .events
            .iter()
            .find(|event| matches!(event.body, Body::Reasoning { .. }))
            .expect("reasoning event");

        assert_eq!(reasoning.capsules.len(), 1, "the sealed blob must survive");
        assert_eq!(
            reasoning.capsules[0].sealed, sealed,
            "the blob must be byte-identical, not re-encoded"
        );
        assert_eq!(reasoning.capsules[0].bound.provider, "openai");
        assert_eq!(reasoning.capsules[0].fits("openai"), CapsuleFit::SameVendor);
        assert_eq!(reasoning.capsules[0].fits("anthropic"), CapsuleFit::ForeignVendor);
        assert_eq!(ir.capture.capsules, 1);
    }

    #[test]
    fn custom_tool_call_keeps_its_protocol() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:02.000Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"shell","input":"ls -la","status":"completed"}}"#,
            r#"{"timestamp":"2026-07-25T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","call_id":"c2","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
        ]);
        let ir = read(file.path()).expect("parse");
        let protocols: Vec<ToolProtocol> = ir
            .events
            .iter()
            .filter_map(|event| {
                let Body::ToolCall { input, .. } = &event.body else {
                    return None;
                };
                Some(input.protocol())
            })
            .collect();
        assert_eq!(
            protocols,
            [ToolProtocol::Freeform, ToolProtocol::JsonArgs],
            "collapsing these into one protocol is a one-way downgrade"
        );
    }

    #[test]
    fn failed_tool_output_is_not_reported_as_success() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","status":"failed","output":"boom"}}"#,
        ]);
        let ir = read(file.path()).expect("parse");
        let outcome = ir
            .events
            .iter()
            .find_map(|event| {
                let Body::ToolResult { outcome, .. } = &event.body else {
                    return None;
                };
                Some(*outcome)
            })
            .expect("tool result");
        assert_eq!(
            outcome,
            ToolOutcome::Failed,
            "a failed tool call must not become a successful one"
        );
    }

    #[test]
    fn event_msg_never_becomes_model_context() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"rendered twice"}}"#,
            r#"{"timestamp":"2026-07-25T10:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
        ]);
        let ir = read(file.path()).expect("parse");
        assert!(
            ir.model_visible().is_empty(),
            "event_msg duplicates response_item for rendering; promoting it doubles the conversation"
        );
        let telemetry = ir
            .events
            .iter()
            .filter(|event| event.visibility == Visibility::Telemetry)
            .count();
        assert_eq!(telemetry, 1, "token_count is accounting, not history");
    }

    /// The two `event_msg` payloads that edit history get typed here.
    ///
    /// The visibility must stay `Ui` — that is what Codex actually recorded,
    /// and promoting it to `Model` to make the resolver notice would put two
    /// rendering artifacts into the target's context. The resolver reads both
    /// before its visibility gate instead; see `casr::replay::resolve`.
    #[test]
    fn history_directives_are_typed_rather_than_left_as_control_strings() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:07.000Z","type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}"#,
            r#"{"timestamp":"2026-07-25T10:00:08.000Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t9","reason":"interrupted"}}"#,
            r#"{"timestamp":"2026-07-25T10:00:09.000Z","type":"event_msg","payload":{"type":"agent_message","message":"rendered"}}"#,
        ]);
        let ir = read(file.path()).expect("parse");
        let bodies: Vec<(&str, Visibility)> = ir
            .events
            .iter()
            .map(|event| (event.body.kind(), event.visibility))
            .collect();
        assert_eq!(
            bodies,
            [
                ("rollback", Visibility::Ui),
                ("abort", Visibility::Ui),
                ("control", Visibility::Ui),
            ],
            "a rollback filed under `control` is a rollback the provider-agnostic \
             resolver cannot see"
        );

        assert!(matches!(ir.events[0].body, Body::Rollback { turns: 1 }));
        assert!(matches!(&ir.events[1].body, Body::Abort {}));
        // The abort carries no turn of its own; the event does, set from the
        // same `turn_id`, which is why the body field was removed.
        assert_eq!(ir.events[1].turn.as_deref(), Some("t9"));
    }

    /// `num_turns` is present on all 714 corpus rollbacks, so this is the
    /// unmeasured case — and it must resolve to one turn rather than to the
    /// whole session.
    #[test]
    fn a_rollback_with_no_count_rolls_back_one_turn() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:07.000Z","type":"event_msg","payload":{"type":"thread_rolled_back"}}"#,
        ]);
        let ir = read(file.path()).expect("parse");
        assert!(matches!(ir.events[0].body, Body::Rollback { turns: 1 }));
    }

    #[test]
    fn compaction_supersedes_prior_model_events() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:07.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#,
            r#"{"timestamp":"2026-07-25T10:00:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second"}]}}"#,
            r#"{"timestamp":"2026-07-25T10:00:09.000Z","type":"compacted","payload":{"window_id":"w2","previous_window_id":"w1","replacement_history":[{"role":"user","type":"message","content":[{"type":"input_text","text":"condensed"}]}]}}"#,
            r#"{"timestamp":"2026-07-25T10:00:10.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"after"}]}}"#,
        ]);
        let ir = read(file.path()).expect("parse");

        let texts: Vec<String> = ir
            .model_visible()
            .iter()
            .filter_map(|event| {
                let Body::Message { blocks, .. } = &event.body else {
                    return None;
                };
                Some(blocks.iter().filter_map(Block::as_text).collect::<String>())
            })
            .collect();
        assert_eq!(
            texts,
            ["condensed", "after"],
            "pre-compaction history must not be replayed to the target"
        );
    }

    #[test]
    fn unknown_line_types_are_recorded_not_dropped() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:11.000Z","type":"brand_new_event_type","payload":{"a":1}}"#,
            r#"not json at all"#,
        ]);
        let ir = read(file.path()).expect("parse");
        assert_eq!(
            ir.capture.unknown, 2,
            "format drift must be counted, not silently skipped"
        );
        assert_eq!(ir.capture.lines_read, 3);
    }

    #[test]
    fn ids_are_deterministic_across_reads() {
        let file = rollout(&[
            META,
            r#"{"timestamp":"2026-07-25T10:00:12.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        ]);
        let first: Vec<String> = read(file.path()).unwrap().events.iter().map(|e| e.id.clone()).collect();
        let second: Vec<String> = read(file.path()).unwrap().events.iter().map(|e| e.id.clone()).collect();
        assert_eq!(first, second);
    }
}
