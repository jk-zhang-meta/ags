//! Claude Code → structured IR.
//!
//! Counterpart to [`super::codex_ir`]; see that module for why the
//! high-fidelity readers live beside the flat ones rather than inside them.
//!
//! # Mapping
//!
//! Derived from a census of 170 real transcripts spanning Claude Code
//! 2.1.214–2.1.220. Counts below are from that corpus.
//!
//! | Native `type` | IR | Visibility |
//! |---|---|---|
//! | `assistant` (25k) | [`Body::Message`] / [`Body::Reasoning`] / [`Body::ToolCall`] | `Model` |
//! | `user` (13k) | [`Body::Message`] / [`Body::ToolResult`] | `Model` |
//! | `system` + `compact_boundary` | [`Body::Compaction`] | `Model` |
//! | `system` (other subtypes) | [`Body::Control`] | `Ui` |
//! | `attachment` (1.5k) | [`Body::Attachment`] | `Ui` |
//! | `mode`, `permission-mode`, `last-prompt`, `ai-title`, `queue-operation`, `file-history-*`, `pr-link`, `agent-name`, `started`, `result`, `fork-context-ref` | [`Body::Control`] | `Ui` |
//! | anything else | [`Body::Unknown`] | `Ui` |
//!
//! The list above is an allowlist, not a fallback chain. A `type` that is not
//! on it becomes [`Body::Unknown`] and shows up in
//! [`crate::ir::CaptureReport::unknown`], because the alternative — quietly
//! filing an unrecognised record under `Control` — is how a reader keeps
//! reporting success after the format moves underneath it.
//!
//! # Two things Claude Code gives us that Codex does not
//!
//! **A real DAG.** Every record carries `uuid` and `parentUuid`, so
//! [`Event::parent`] is read rather than inferred, and `isSidechain` plus
//! `agentId` give subagent branches their own [`Branch::Sub`]. A DAG needs a
//! head to be useful, and `last-prompt.leafUuid` is it: that goes onto
//! [`SessionIr::live_head`], so [`crate::replay::resolve`] can prune abandoned
//! branches without knowing what a `last-prompt` record is. Codex sets no head
//! and gets no prune, structurally rather than by an agent check.
//!
//! **Exact compaction scope.** `compact_boundary` records
//! `compactMetadata.preservedMessages.allUuids` — precisely which messages
//! survived. So the post-compaction context is the live model events that are
//! in the preserved set, and `supersedes` is the rest, instead of the
//! "everything before this point" approximation the Codex reader is forced
//! into.
//!
//! # The one thing Claude Code gives us twice
//!
//! Across a `/compact`, Claude re-appends the records it has to replay for the
//! post-boundary context to be well-formed — the unresolved `tool_use` and its
//! `tool_result`s — under **their original `uuid`s**, immediately before the
//! `compact_boundary`. On corpus transcript `aeeed6b0` that is lines 1048–1057
//! restating lines 16–694. One event per line then emits the same
//! [`Event::id`] twice, and the id is documented unique within the session:
//! `replay::resolve`'s `position` map, [`SessionIr::model_visible`]'s `by_id`
//! and `prune_forks`' record index all key on it, so *which copy survives is
//! arbitrary*.
//!
//! A re-emission carries nothing the IR does not already have. The preserved
//! set arrives separately and exactly, through the boundary's
//! `preservedMessages.allUuids`, which [`push_system`] already reads — so the
//! re-append is a restatement, and the second copy is dropped. Every emission
//! therefore goes through [`Sink::emit`], which makes a duplicate id
//! unrepresentable in this reader's output rather than a thing to remember not
//! to do. A record that reuses an id with *different* content is not a
//! restatement and is kept, under a minted `<id>#dup<n>`, and counted in
//! [`crate::ir::CaptureReport::id_collisions`]. See [`is_restatement`] for what
//! "different" means and why it is not byte-identity.
//!
//! # Reasoning
//!
//! `thinking` blocks carry an empty `thinking` string and a `signature` of
//! 344–100820 characters whose length tracks the amount of reasoning. The
//! signature is the reasoning, sealed. It becomes a [`Capsule`]; the empty
//! string is not worth carrying and is not carried.

use std::collections::{HashMap, HashSet};
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

/// Claude Code transcripts record no inference provider.
///
/// The `thinking.signature` blobs are Anthropic-issued whenever they exist at
/// all, so this is the binding that matters for capsule replay. A gateway that
/// maps Claude model families onto some other vendor produces no thinking
/// blocks, and therefore no capsules for this assumption to mislabel.
const ASSUMED_PROVIDER: &str = "anthropic";

/// Native record types that are understood and deliberately treated as UI
/// chrome. Anything outside this set becomes [`Body::Unknown`].
const UI_CONTROL_TYPES: &[&str] = &[
    "mode",
    "permission-mode",
    "last-prompt",
    "ai-title",
    "queue-operation",
    "file-history-snapshot",
    "file-history-delta",
    "pr-link",
    "agent-name",
    "started",
    "result",
    "fork-context-ref",
];

/// Parse a Claude Code transcript into the structured IR.
pub fn read(path: &Path) -> anyhow::Result<SessionIr> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open Claude transcript {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut ir = SessionIr::new("claude-code", String::new());
    ir.origin.provider = Some(ASSUMED_PROVIDER.to_string());
    let mut sink = Sink::new(ir);

    for (index, line) in reader.lines().enumerate() {
        let line = line
            .with_context(|| format!("failed to read line {} of {}", index + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        sink.ir.capture.lines_read += 1;
        let source = SourceRef {
            line: index as u64 + 1,
            sha256: format!("{:x}", Sha256::digest(line.as_bytes())),
        };

        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                sink.emit(Event {
                    id: format!("cc-{:06}", source.line),
                    parent: None,
                    branch: Branch::Main,
                    turn: None,
                    ts: None,
                    visibility: Visibility::Ui,
                    body: Body::Unknown {
                        native_type: None,
                        raw: Value::String(format!("unparseable JSON: {error}")),
                    },
                    capsules: Vec::new(),
                    source,
                });
                continue;
            }
        };

        apply_envelope(&mut sink.ir, &value);
        let ctx = RecordContext::from(&value, &source);

        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => push_assistant(&mut sink, &ctx, &value),
            Some("user") => push_user(&mut sink, &ctx, &value),
            Some("system") => push_system(&mut sink, &ctx, &value),
            Some("attachment") => {
                let body = Body::Attachment {
                    attachment_kind: value
                        .get("attachment")
                        .and_then(|a| a.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    data: value.get("attachment").cloned().unwrap_or(Value::Null),
                };
                sink.emit(ctx.event(0, Visibility::Ui, body, Vec::new()));
            }
            Some(other) if UI_CONTROL_TYPES.contains(&other) => {
                // `last-prompt` is chrome to render *and* the one thing in the
                // file that names the live branch. The record stays a control
                // event; the fact it carries is lifted onto the session, where
                // the resolver can read it without knowing what a `last-prompt`
                // is. Claude rewrites the record on every submit and the file
                // keeps them all, so the newest wins.
                if other == "last-prompt"
                    && let Some(leaf) = value.get("leafUuid").and_then(Value::as_str)
                {
                    sink.ir.live_head = Some(leaf.to_string());
                }
                let body = Body::Control {
                    control_kind: other.to_string(),
                    data: value.clone(),
                };
                sink.emit(ctx.event(0, Visibility::Ui, body, Vec::new()));
            }
            other => {
                let body = Body::Unknown {
                    native_type: other.map(str::to_string),
                    raw: value.clone(),
                };
                sink.emit(ctx.event(0, Visibility::Unclassified, body, Vec::new()));
            }
        }
    }

    let ir = sink.ir;
    if ir.origin.native_session_id.is_empty() {
        anyhow::bail!(
            "{} has no sessionId on any record; refusing to guess it",
            path.display()
        );
    }
    Ok(ir)
}

// ---------------------------------------------------------------------------
// Record context
// ---------------------------------------------------------------------------

/// The envelope fields shared by every record, resolved once per line.
struct RecordContext {
    uuid: String,
    parent: Option<String>,
    branch: Branch,
    turn: Option<String>,
    ts: Option<i64>,
    source: SourceRef,
}

impl RecordContext {
    fn from(value: &Value, source: &SourceRef) -> Self {
        let sidechain = value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let branch = if sidechain {
            Branch::Sub(
                value
                    .get("agentId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            )
        } else {
            Branch::Main
        };
        Self {
            uuid: value
                .get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                // `compact_boundary` and a few control records carry no uuid.
                .unwrap_or_else(|| format!("cc-{:06}", source.line)),
            parent: value
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    value
                        .get("logicalParentUuid")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
            branch,
            turn: value
                .get("promptId")
                .and_then(Value::as_str)
                .map(str::to_string),
            ts: value.get("timestamp").and_then(parse_timestamp),
            source: source.clone(),
        }
    }

    /// Build an event for this record.
    ///
    /// One native record can expand into several events — an assistant turn is
    /// routinely `thinking` + `text` + `tool_use` — so `slot` disambiguates
    /// them while keeping the native uuid recognisable in the id.
    fn event(&self, slot: usize, visibility: Visibility, body: Body, capsules: Vec<Capsule>) -> Event {
        Event {
            id: if slot == 0 {
                self.uuid.clone()
            } else {
                format!("{}#{slot}", self.uuid)
            },
            parent: self.parent.clone(),
            branch: self.branch.clone(),
            turn: self.turn.clone(),
            ts: self.ts,
            visibility,
            body,
            capsules,
            source: self.source.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The emission door
// ---------------------------------------------------------------------------

/// The session being built, and the one door every event goes through.
///
/// This is a struct rather than three threaded parameters because of `seen`.
/// [`Event::id`] is unique within the session, Claude Code breaks that on its
/// own (see the module docs), and the only way to notice is to remember what has
/// already been emitted. Holding that beside the session means the check happens
/// once, at the single point of emission, instead of being a rule each of the
/// four record handlers has to observe.
struct Sink {
    ir: SessionIr,
    /// Ids of the model-visible events still live, in order. This is the set
    /// [`push_system`] partitions on `preservedMessages.allUuids`.
    live_model: Vec<String>,
    /// Every id emitted so far, to its index in `ir.events`.
    seen: HashMap<String, usize>,
}

impl Sink {
    fn new(ir: SessionIr) -> Self {
        Self {
            ir,
            live_model: Vec::new(),
            seen: HashMap::new(),
        }
    }

    /// Add one event to the session, unless it restates one already there.
    fn emit(&mut self, event: Event) {
        let event = match self.seen.get(&event.id).copied() {
            None => event,
            Some(index) => {
                if is_restatement(&self.ir.events[index], &event) {
                    self.ir.capture.restated += 1;
                    return;
                }
                self.rename(event)
            }
        };
        if event.visibility == Visibility::Model {
            self.live_model.push(event.id.clone());
        }
        self.seen.insert(event.id.clone(), self.ir.events.len());
        self.ir.capture.record(&event);
        self.ir.events.push(event);
    }

    /// Same id, different content: keep the event, under an id of its own.
    ///
    /// Dropping it would lose data, and keeping it under the id it arrived with
    /// would leave the ambiguity this reader exists to remove. So it gets a
    /// `#`-suffixed id — the shape already used for the blocks one record splits
    /// into, so `replay::record_of` still recovers the native record — and the
    /// collision is counted, because a provider reusing one identifier for two
    /// different things is worth seeing rather than resolving quietly.
    fn rename(&mut self, mut event: Event) -> Event {
        // `events.len()` is strictly increasing, so this is unique by
        // construction and needs no probing. `dup` is non-numeric, so it cannot
        // be mistaken for — or collide with — a `<uuid>#<slot>` a multi-block
        // record already minted.
        let minted = format!("{}#dup{}", event.id, self.ir.events.len());
        let reused = std::mem::replace(&mut event.id, minted);
        self.ir.capture.id_collisions += 1;
        self.ir.capture.note(format!(
            "line {} reuses id {reused:?} with different content; kept as {:?}",
            event.source.line, event.id
        ));
        event
    }
}

/// Is `candidate` the event `existing` already is, restated?
///
/// # Why this is not byte-identity
///
/// Measured rather than assumed. Across 691 corpus transcripts there are ten
/// re-emissions, and **not one of them is byte-identical** to its first
/// occurrence: Claude stamps a `slug` onto the re-appended copy, a field this
/// reader does not read at all. Nine of the ten additionally carry the
/// *compaction's* `promptId` and the then-current `cwd`. A byte-identity rule —
/// or any rule over the whole raw record — therefore fires on zero of the ten
/// and leaves every duplicate id exactly where it was.
///
/// So the comparison is over the [`Event`] the reader built. That is also the
/// only definition the compiler can keep honest: a list of JSON key names would
/// drift the moment the reader learns a new field, silently loosening the rule,
/// whereas the destructuring below breaks the build.
///
/// Two fields are excluded, each for a stated reason:
///
/// - `source` is the line number and that line's hash. A restatement is by
///   definition a *different line*, so comparing it would make the rule fire
///   never.
/// - `turn` is `promptId`, and the re-append re-stamps it. All nine differing
///   copies carry the compaction's own prompt id rather than the four distinct
///   ones the records were typed under. `Event::turn` is what
///   `replay::roll_back` reads as "the last N typed turns", so adopting the
///   re-append's value would collapse four historical turns into one and let a
///   single rollback undo the entire preserved history. Keeping the first
///   occurrence is both the dedupe rule and the more accurate value.
///
/// `ts` is deliberately *not* excluded even though it is identical on all ten,
/// so the strictness costs nothing measured — and it fails safe: a future
/// re-append that also re-stamps the timestamp stops being recognised, and the
/// record is then kept with a distinct id and counted, which is loud, rather
/// than dropped, which would be silent.
fn is_restatement(existing: &Event, candidate: &Event) -> bool {
    // Destructured with no `..` on purpose. A field added to `Event` is a
    // compile error here, naming this function as the place that has to decide
    // whether the new field makes a re-emission a different event — the same
    // rule `replay.rs` applies to `Body`, and for the same reason.
    let Event {
        id,
        parent,
        branch,
        turn: _,
        ts,
        visibility,
        body,
        capsules,
        source: _,
    } = candidate;
    existing.id == *id
        && existing.parent == *parent
        && existing.branch == *branch
        && existing.ts == *ts
        && existing.visibility == *visibility
        && existing.body == *body
        && existing.capsules == *capsules
}

// ---------------------------------------------------------------------------
// Record handlers
// ---------------------------------------------------------------------------

fn apply_envelope(ir: &mut SessionIr, value: &Value) {
    if ir.origin.native_session_id.is_empty()
        && let Some(id) = value.get("sessionId").and_then(Value::as_str)
    {
        ir.origin.native_session_id = id.to_string();
    }
    if ir.origin.agent_version.is_none()
        && let Some(version) = value.get("version").and_then(Value::as_str)
    {
        ir.origin.agent_version = Some(version.to_string());
    }
    if ir.workspace.cwd.is_none()
        && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
    {
        ir.workspace.cwd = Some(cwd.into());
    }
    if ir.workspace.git_branch.is_none()
        && let Some(branch) = value.get("gitBranch").and_then(Value::as_str)
        && !branch.is_empty()
    {
        ir.workspace.git_branch = Some(branch.to_string());
    }
    if ir.origin.model.is_none()
        && let Some(model) = value
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
    {
        ir.origin.model = Some(model.to_string());
    }
    if ir.origin.captured_at.is_none() {
        ir.origin.captured_at = value.get("timestamp").and_then(parse_timestamp);
    }
}

fn push_assistant(sink: &mut Sink, ctx: &RecordContext, value: &Value) {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let model = value
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // Text blocks coalesce into one message; thinking and tool_use each become
    // their own event because they are different kinds of thing, and because a
    // tool call needs an identity a text block does not have.
    let mut text_blocks: Vec<Block> = Vec::new();
    let mut slot = 0usize;

    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let capsules = item
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|sealed| !sealed.is_empty())
                    .map(|sealed| {
                        vec![Capsule {
                            kind: CapsuleKind::AnthropicThinkingSignature,
                            bound: CapsuleBinding {
                                provider: ASSUMED_PROVIDER.to_string(),
                                model: model.clone(),
                            },
                            sealed: sealed.to_string(),
                        }]
                    })
                    .unwrap_or_default();
                let text = item
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                let body = Body::Reasoning {
                    text,
                    summary: Vec::new(),
                };
                sink.emit(ctx.event(slot, Visibility::Model, body, capsules));
                slot += 1;
            }
            // Sealed the same way a signature is, and just as vendor-bound.
            // It carries no readable text at all, so the reasoning body is
            // empty and the whole value of the event is its capsule.
            Some("redacted_thinking") => {
                let capsules = item
                    .get("data")
                    .and_then(Value::as_str)
                    .filter(|sealed| !sealed.is_empty())
                    .map(|sealed| {
                        vec![Capsule {
                            kind: CapsuleKind::AnthropicRedactedThinking,
                            bound: CapsuleBinding {
                                provider: ASSUMED_PROVIDER.to_string(),
                                model: model.clone(),
                            },
                            sealed: sealed.to_string(),
                        }]
                    })
                    .unwrap_or_default();
                let body = Body::Reasoning {
                    text: None,
                    summary: Vec::new(),
                };
                sink.emit(ctx.event(slot, Visibility::Model, body, capsules));
                slot += 1;
            }
            Some("tool_use") => {
                let body = Body::ToolCall {
                    call_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    namespace: item
                        .get("caller")
                        .and_then(|c| c.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    // Claude has one calling convention: a JSON object.
                    input: ToolInput::from_json_field(
                        item.get("input").unwrap_or(&Value::Null),
                    ),
                };
                sink.emit(ctx.event(slot, Visibility::Model, body, Vec::new()));
                slot += 1;
            }
            _ => text_blocks.push(block_from_item(item)),
        }
    }

    if !text_blocks.is_empty() {
        let body = Body::Message {
            role: Role::Assistant,
            blocks: text_blocks,
        };
        sink.emit(ctx.event(slot, Visibility::Model, body, Vec::new()));
    }
}

fn push_user(sink: &mut Sink, ctx: &RecordContext, value: &Value) {
    let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    // `toolUseResult` is Claude's structured companion to a `tool_result`
    // block: stdout/stderr/interrupted for a command, structuredPatch for an
    // edit, and a dozen other shapes. Flattening it to text throws away the
    // part a tool actually needs.
    let structured = value.get("toolUseResult").cloned();

    match content {
        Value::String(text) => {
            if text.trim().is_empty() {
                return;
            }
            let body = Body::Message {
                role: Role::User,
                blocks: vec![Block::Text { text: text.clone() }],
            };
            sink.emit(ctx.event(0, Visibility::Model, body, Vec::new()));
        }
        Value::Array(items) => {
            let mut text_blocks: Vec<Block> = Vec::new();
            let mut slot = 0usize;
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let body = Body::ToolResult {
                        call_id: item
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        // Claude does write the flag, so absence is its own
                        // answer rather than an excuse to assume success.
                        outcome: match item.get("is_error").and_then(Value::as_bool) {
                            Some(true) => ToolOutcome::Failed,
                            Some(false) => ToolOutcome::Succeeded,
                            None => ToolOutcome::Unknown,
                        },
                        output: blocks_from_content(item.get("content")),
                        structured: structured.clone(),
                    };
                    sink.emit(ctx.event(slot, Visibility::Model, body, Vec::new()));
                    slot += 1;
                } else {
                    text_blocks.push(block_from_item(item));
                }
            }
            if !text_blocks.is_empty() {
                let body = Body::Message {
                    role: Role::User,
                    blocks: text_blocks,
                };
                sink.emit(ctx.event(slot, Visibility::Model, body, Vec::new()));
            }
        }
        _ => {}
    }
}

fn push_system(sink: &mut Sink, ctx: &RecordContext, value: &Value) {
    let subtype = value
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    if subtype != "compact_boundary" {
        let body = Body::Control {
            control_kind: format!("system.{subtype}"),
            data: value.clone(),
        };
        sink.emit(ctx.event(0, Visibility::Ui, body, Vec::new()));
        return;
    }

    // Claude names the survivors, so the superseded set is exact rather than
    // "everything before this line".
    let preserved: HashSet<String> = value
        .get("compactMetadata")
        .and_then(|meta| meta.get("preservedMessages"))
        .and_then(|preserved| preserved.get("allUuids"))
        .and_then(Value::as_array)
        .map(|uuids| {
            uuids
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Claude does not inline a replacement history the way Codex does: the
    // summary arrives afterwards as an ordinary message flagged
    // `isCompactSummary`. So the post-compaction context is simply the part of
    // the live set Claude named as preserved, and the rest is what it dropped.
    // Ids of split blocks are `<uuid>#<slot>`; the preserved set names the
    // record, so compare on the record part.
    let (context, supersedes): (Vec<String>, Vec<String>) = sink
        .live_model
        .iter()
        .cloned()
        .partition(|id| preserved.contains(id.split('#').next().unwrap_or(id)));

    let body = Body::Compaction {
        context: context.clone(),
        supersedes,
        note: None,
        window_from: None,
        window_to: value
            .get("compactMetadata")
            .and_then(|meta| meta.get("trigger"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    sink.live_model = context;
    sink.emit(ctx.event(0, Visibility::Model, body, Vec::new()));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn blocks_from_content(content: Option<&Value>) -> Vec<Block> {
    let Some(content) = content else {
        return Vec::new();
    };
    match content {
        Value::String(text) => vec![Block::Text { text: text.clone() }],
        Value::Array(items) => items.iter().map(block_from_item).collect(),
        Value::Null => Vec::new(),
        other => vec![Block::Text {
            text: other.to_string(),
        }],
    }
}

/// One content block.
///
/// Never returns "nothing". An unrecognised block becomes [`Block::Unknown`]
/// so that it is counted and can be inspected, rather than vanishing the way
/// an `Option`-returning parser used to let it.
fn block_from_item(item: &Value) -> Block {
    let native_type = item.get("type").and_then(Value::as_str);
    match native_type {
        Some("image") => image_block(item).unwrap_or_else(|| unknown_block(native_type, item)),
        Some("document") => Block::Document { data: item.clone() },
        _ => match item.get("text").and_then(Value::as_str) {
            Some(text) => Block::Text {
                text: text.to_string(),
            },
            None => unknown_block(native_type, item),
        },
    }
}

fn image_block(item: &Value) -> Option<Block> {
    let source = item.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => Some(Block::Image {
            url: format!(
                "data:{};base64,{}",
                source.get("media_type").and_then(Value::as_str)?,
                source.get("data").and_then(Value::as_str)?
            ),
            media_type: source
                .get("media_type")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => Some(Block::Image {
            url: source.get("url").and_then(Value::as_str)?.to_string(),
            media_type: None,
        }),
    }
}

fn unknown_block(native_type: Option<&str>, item: &Value) -> Block {
    Block::Unknown {
        native_type: native_type.map(str::to_string),
        raw: item.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CapsuleFit;
    use std::io::Write;

    fn transcript<L: AsRef<str>>(lines: &[L]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{}", line.as_ref()).expect("write");
        }
        file.flush().expect("flush");
        file
    }

    fn user(uuid: &str, parent: &str, text: &str) -> String {
        let parent = if parent.is_empty() {
            "null".to_string()
        } else {
            format!("\"{parent}\"")
        };
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent},"isSidechain":false,"sessionId":"s1","cwd":"/work","version":"2.1.220","timestamp":"2026-07-25T10:00:00.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    #[test]
    fn requires_a_session_id() {
        let file = transcript(&[r#"{"type":"mode","mode":"default"}"#]);
        let error = read(file.path()).expect_err("must refuse a transcript with no sessionId");
        assert!(error.to_string().contains("sessionId"));
    }

    #[test]
    fn thinking_signature_becomes_a_capsule() {
        let sealed = "EpYCCokBCA8YsignatureBLOB";
        let file = transcript(&[
            user("u1", "", "hi"),
            format!(
                r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:01.000Z","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"thinking","thinking":"","signature":"{sealed}"}},{{"type":"text","text":"done"}}]}}}}"#
            ),
        ]);
        let ir = read(file.path()).expect("parse");

        let reasoning = ir
            .events
            .iter()
            .find(|event| matches!(event.body, Body::Reasoning { .. }))
            .expect("reasoning event");
        assert_eq!(reasoning.capsules.len(), 1);
        assert_eq!(reasoning.capsules[0].sealed, sealed);
        assert_eq!(
            reasoning.capsules[0].bound.model.as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(reasoning.capsules[0].fits("anthropic"), CapsuleFit::SameVendor);
        assert_eq!(reasoning.capsules[0].fits("openai"), CapsuleFit::ForeignVendor);
    }

    #[test]
    fn assistant_turn_splits_into_reasoning_tool_and_text() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"sig"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"},"caller":{"type":"direct"}},{"type":"text","text":"running"}]}}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");
        let kinds: Vec<&str> = ir.events.iter().map(|e| e.body.kind()).collect();
        assert_eq!(kinds, ["message", "reasoning", "tool_call", "message"]);

        // Split events must stay distinguishable but keep the native uuid visible.
        let ids: Vec<&str> = ir.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["u1", "a1", "a1#1", "a1#2"]);
    }

    #[test]
    fn tool_result_keeps_the_structured_companion() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"user","uuid":"u2","parentUuid":"u1","isSidechain":false,"sessionId":"s1","timestamp":"2026-07-25T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]},"toolUseResult":{"stdout":"ok","stderr":"","interrupted":false,"isImage":false}}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");
        let structured = ir
            .events
            .iter()
            .find_map(|event| match &event.body {
                Body::ToolResult { structured, .. } => structured.clone(),
                _ => None,
            })
            .expect("tool result carries toolUseResult");
        assert_eq!(structured.get("stdout").and_then(Value::as_str), Some("ok"));
    }

    #[test]
    fn error_tool_result_is_not_reported_as_success() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"user","uuid":"u2","parentUuid":"u1","isSidechain":false,"sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");
        let outcome = ir
            .events
            .iter()
            .find_map(|event| match &event.body {
                Body::ToolResult { outcome, .. } => Some(*outcome),
                _ => None,
            })
            .expect("tool result");
        assert_eq!(outcome, ToolOutcome::Failed);
    }

    #[test]
    fn sidechain_records_get_their_own_branch() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"assistant","uuid":"s1a","parentUuid":"u1","isSidechain":true,"agentId":"agent-7","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"sub work"}]}}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");
        let branch = ir
            .events
            .iter()
            .find(|event| event.id == "s1a")
            .map(|event| event.branch.clone())
            .expect("sidechain event");
        assert_eq!(branch, Branch::Sub("agent-7".to_string()));
    }

    #[test]
    fn compaction_uses_the_preserved_set_not_a_cutoff() {
        let file = transcript(&[
            user("u1", "", "dropped"),
            user("u2", "u1", "kept"),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","logicalParentUuid":"u2","sessionId":"s1","content":"","level":"info","compactMetadata":{"trigger":"auto","preTokens":100,"postTokens":10,"preservedMessages":{"anchorUuid":"u2","uuids":["u2"],"allUuids":["u2"]}}}"#.to_string(),
            user("u3", "cb1", "after"),
        ]);
        let ir = read(file.path()).expect("parse");

        let (context, supersedes) = ir
            .events
            .iter()
            .find_map(|event| match &event.body {
                Body::Compaction {
                    context,
                    supersedes,
                    ..
                } => Some((context.clone(), supersedes.clone())),
                _ => None,
            })
            .expect("compaction event");
        assert_eq!(
            supersedes,
            ["u1"],
            "only messages absent from allUuids are superseded"
        );
        assert_eq!(
            context,
            ["u2"],
            "the post-compaction context is the preserved segment, not empty"
        );

        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(visible, ["u2", "u3"], "the preserved segment must survive");
    }

    #[test]
    fn unrecognised_record_types_are_flagged_not_filed_under_control() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"some-future-record","uuid":"x1","sessionId":"s1"}"#.to_string(),
            r#"{"type":"mode","uuid":"m1","sessionId":"s1","mode":"default"}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");
        assert_eq!(
            ir.capture.unknown, 1,
            "only the unrecognised type counts; `mode` is a known control record"
        );
        assert_eq!(ir.capture.by_kind.get("control"), Some(&1));
    }

    /// The live branch head is lifted onto the session.
    ///
    /// Claude appends a fresh `last-prompt` on every submit and keeps the older
    /// ones, so the newest wins. The record itself stays an ordinary control
    /// event — it is still chrome — but the resolver reads the head from
    /// [`SessionIr::live_head`] and never learns what a `last-prompt` is.
    #[test]
    fn the_newest_last_prompt_sets_the_live_head() {
        let file = transcript(&[
            user("u1", "", "one"),
            r#"{"type":"last-prompt","uuid":"lp1","sessionId":"s1","lastPrompt":"one","leafUuid":"u1"}"#.to_string(),
            user("u2", "u1", "two"),
            r#"{"type":"last-prompt","uuid":"lp2","sessionId":"s1","lastPrompt":"two","leafUuid":"u2"}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");

        assert_eq!(ir.live_head.as_deref(), Some("u2"), "the newest head wins");
        assert_eq!(
            ir.capture.by_kind.get("control"),
            Some(&2),
            "the record is still chrome and still counted as such"
        );
    }

    /// A transcript with no `last-prompt` names no head, and the resolver then
    /// prunes nothing. Same structural no-op Codex gets.
    #[test]
    fn a_transcript_with_no_last_prompt_names_no_head() {
        let file = transcript(&[user("u1", "", "one")]);
        let ir = read(file.path()).expect("parse");
        assert_eq!(ir.live_head, None);
    }

    // -----------------------------------------------------------------------
    // Re-emission across a `/compact`
    // -----------------------------------------------------------------------

    /// A `tool_result` re-appended before a `compact_boundary`, in the exact
    /// shape the corpus has it: same `uuid`, same `parentUuid`, same
    /// `timestamp`, same content — and a re-stamped `promptId` plus a `slug`
    /// this reader never reads. Not one of the ten corpus re-emissions is
    /// byte-identical, so a byte-identity rule would let both copies through.
    #[test]
    fn a_re_emission_that_only_restates_is_not_a_second_event() {
        let result = |prompt: &str, slug: &str| {
            format!(
                r#"{{"type":"user","uuid":"u2","parentUuid":"u1","isSidechain":false,"sessionId":"s1","promptId":"{prompt}"{slug},"timestamp":"2026-07-25T10:00:02.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}}]}},"toolUseResult":{{"stdout":"ok"}}}}"#
            )
        };
        let file = transcript(&[
            user("u1", "", "hi"),
            result("typed-turn", ""),
            result("compaction-turn", r#","slug":"eager-giggling-garden""#),
        ]);
        let ir = read(file.path()).expect("parse");

        let ids: Vec<&str> = ir.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["u1", "u2"], "the restatement must not become an event");
        assert_eq!(ir.capture.restated, 1, "and it must be counted as recognised");
        assert_eq!(ir.capture.id_collisions, 0, "nothing collided; it restated");
        assert_eq!(
            ir.capture.lines_read, 3,
            "the line was still read; only the event was skipped"
        );
        assert_eq!(
            ir.events[1].turn.as_deref(),
            Some("typed-turn"),
            "the turn the record was typed under wins over the compaction's; \
             `replay::roll_back` reads this as `the last N typed turns`, and \
             adopting the re-append's value collapses the preserved history \
             into one turn"
        );
    }

    /// The same id with *different* content is not a restatement. Dropping it
    /// would lose data, so it is kept under an id of its own and the collision
    /// is counted rather than silently resolved.
    #[test]
    fn a_re_emission_that_changes_content_keeps_both_under_distinct_ids() {
        let file = transcript(&[
            user("u1", "", "hi"),
            user("u2", "u1", "first"),
            user("u2", "u1", "edited"),
        ]);
        let ir = read(file.path()).expect("parse");

        assert_eq!(ir.events.len(), 3, "nothing may be dropped");
        assert_eq!(ir.capture.id_collisions, 1);
        assert_eq!(ir.capture.restated, 0);
        assert_eq!(ir.events[1].id, "u2");
        assert!(
            ir.events[2].id.starts_with("u2#"),
            "the minted id must keep the native record recoverable by \
             `replay::record_of`, which splits on `#`: {:?}",
            ir.events[2].id
        );
        assert_ne!(ir.events[1].id, ir.events[2].id);
        assert!(
            ir.capture.notes.iter().any(|note| note.contains("u2")),
            "a reused identifier is an anomaly and must be visible: {:?}",
            ir.capture.notes
        );
    }

    /// The latent half of the defect, which the exclusion double-count only
    /// hinted at: when Claude *does* name a re-emitted record as preserved, both
    /// copies used to land in the compaction's `context` — the preserved set is
    /// matched on the record part of the id — and the target would be shown the
    /// same message twice.
    #[test]
    fn a_restated_record_that_is_preserved_enters_the_context_once() {
        let file = transcript(&[
            user("u1", "", "dropped"),
            user("u2", "u1", "kept"),
            // The re-append, immediately before the boundary.
            user("u2", "u1", "kept"),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","logicalParentUuid":"u2","sessionId":"s1","compactMetadata":{"trigger":"manual","preservedMessages":{"allUuids":["u2"]}}}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");

        let (context, supersedes) = ir
            .events
            .iter()
            .find_map(|event| match &event.body {
                Body::Compaction {
                    context,
                    supersedes,
                    ..
                } => Some((context.clone(), supersedes.clone())),
                _ => None,
            })
            .expect("compaction event");
        assert_eq!(context, ["u2"], "the preserved record is shown once, not twice");
        assert_eq!(supersedes, ["u1"]);

        let plan = crate::replay::resolve(&ir);
        assert_eq!(plan.events, ["u2"]);
        assert_eq!(
            plan.excluded.len(),
            1,
            "one exclusion, not two: an event dropped twice is counted twice in \
             every fidelity report — {:?}",
            plan.excluded
        );
    }

    /// A record whose `uuid` is absent falls back to a line-derived id, so two
    /// of them are distinct by construction and must not be mistaken for a
    /// re-emission of one another.
    #[test]
    fn records_with_no_uuid_are_not_restatements_of_each_other() {
        let file = transcript(&[
            user("u1", "", "hi"),
            r#"{"type":"mode","sessionId":"s1","mode":"default"}"#.to_string(),
            r#"{"type":"mode","sessionId":"s1","mode":"default"}"#.to_string(),
        ]);
        let ir = read(file.path()).expect("parse");

        assert_eq!(ir.capture.by_kind.get("control"), Some(&2));
        assert_eq!(ir.capture.restated, 0);
        assert_eq!(ir.capture.id_collisions, 0);
    }

    #[test]
    fn parent_links_come_from_the_transcript() {
        let file = transcript(&[user("u1", "", "one"), user("u2", "u1", "two")]);
        let ir = read(file.path()).expect("parse");
        let second = ir.events.iter().find(|e| e.id == "u2").expect("second");
        assert_eq!(second.parent.as_deref(), Some("u1"));
    }
}
