//! Structured session IR — the high-fidelity track.
//!
//! [`crate::model::CanonicalSession`] flattens every provider into text. That is
//! the right trade for providers where a conversational handoff is all anyone
//! wants, and it stays the default. It is the wrong trade for Codex and Claude
//! Code, where the material that gets flattened away is the material that makes
//! a resume feel like a resume.
//!
//! This module is the second track. A provider that can populate it does; one
//! that cannot keeps using the flat model and the conversion is labelled
//! accordingly. The two tracks coexist rather than one replacing the other,
//! which is why adding this file changed no existing provider.
//!
//! # What the flat model loses, and why each loss is modelled here
//!
//! - **Provider-bound reasoning.** Both vendors stopped writing reasoning
//!   plaintext to disk. Claude Code records `thinking: ""` with a `signature`
//!   blob; Codex records an empty `summary` with an `encrypted_content` blob.
//!   Neither blob is a checksum — their length tracks how much reasoning
//!   happened, so they carry the reasoning itself. Dropping them does not
//!   discard metadata, it discards the model's train of thought. See [`Capsule`].
//! - **Compaction.** Codex rewrites the model-visible history in a `compacted`
//!   event. Replaying a session while ignoring those rewrites feeds the target
//!   the *pre*-compaction history. Worse, Codex hands the rewritten history
//!   back **sealed** — 87.6 MB of `encrypted_content` across the corpus — so a
//!   reader that only looks for `role`/`content` finds an empty message where
//!   the whole conversation should be. See [`Body::Compaction`] and
//!   [`Body::SealedContext`].
//! - **Model-visible vs. chrome.** Codex separates these structurally
//!   (`response_item` vs `event_msg`); Claude Code interleaves them. Without
//!   [`Visibility`], a converter either drops the UI history or poisons the
//!   model context with it.
//! - **Tool protocol.** Codex has two — JSON-argument `function_call` and
//!   freeform `custom_tool_call`. Collapsing them is a one-way downgrade. See
//!   [`ToolProtocol`].
//! - **Unknown shapes.** Session formats change every release. An unrecognised
//!   line becomes [`Body::Unknown`] with a pointer back to the bytes, never a
//!   silent `continue`.
//!
//! # The IR is a projection, not the archive
//!
//! Every event carries a [`SourceRef`] back to the line it came from. The IR is
//! meant to sit *beside* the original bytes, not to replace them: same-agent
//! resume should replay the native file, and only a cross-agent handoff should
//! be compiled out of this IR. Treating the IR as the only copy is how a
//! converter ends up losing more than it knows.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::MessageRole;

/// On-disk/on-wire version tag for the structured IR.
///
/// Bump on any change that an older reader could misinterpret. Readers must
/// reject a version they do not recognise rather than guess.
///
/// Nothing reads it yet — no code in the crate deserializes a [`SessionIr`],
/// because the IR is a derived cache and the provider's own bytes are the
/// source of truth (see `docs/EXTENDING.md`). The stamp is still bumped when
/// the shape changes, on the reasoning that the cheap half of the rule costs
/// nothing now and keeps history unambiguous for the first reader that does
/// check it. `/2` added `Body::Rollback`, `Body::Abort` and
/// `SessionIr::live_head`.
///
/// The enforcement point arrives with the session store, which is the first
/// thing that will persist an IR and therefore the first thing that can read a
/// stale one.
pub const IR_VERSION: &str = "agsx-ir/2";

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A session captured on the high-fidelity track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionIr {
    /// Always [`IR_VERSION`] when produced by this crate.
    pub ir_version: String,
    /// Where the session came from.
    pub origin: Origin,
    /// Where it was running.
    pub workspace: Workspace,
    /// Events in topological order: an event never precedes its parent.
    pub events: Vec<Event>,
    /// Head of the live branch, when the provider records one.
    ///
    /// A fact about the session rather than a thing that happened, which is why
    /// it lives here and not on an [`Event`]. Claude Code writes a
    /// `last-prompt` record naming the leaf the newest turn was attached to,
    /// and the newest such record wins.
    ///
    /// `None` means the provider names no branch head — Codex does not — and
    /// [`crate::replay::resolve`] then prunes no forks at all. That no-op is
    /// structural rather than an agent check: nothing in the resolver looks at
    /// [`Origin::agent`].
    pub live_head: Option<String>,
    /// What the capture did and did not manage to preserve.
    pub capture: CaptureReport,
}

impl SessionIr {
    /// Start an empty IR for `agent` / `native_session_id`.
    pub fn new(agent: impl Into<String>, native_session_id: impl Into<String>) -> Self {
        Self {
            ir_version: IR_VERSION.to_string(),
            origin: Origin {
                agent: agent.into(),
                native_session_id: native_session_id.into(),
                agent_version: None,
                provider: None,
                model: None,
                captured_at: None,
            },
            workspace: Workspace::default(),
            events: Vec::new(),
            live_head: None,
            capture: CaptureReport::default(),
        }
    }

    /// Events the target model should actually be shown.
    ///
    /// A thin view over [`crate::replay::resolve`]. Compaction, rollback,
    /// aborts and abandoned forks all edit history after the fact, and an
    /// earlier revision of this method reimplemented a subset of that here —
    /// two answers to "what does the model see" is the bug the resolver
    /// exists to remove, so this one is a lookup rather than a second fold.
    ///
    /// The compaction marker itself is not replayable content, so it is
    /// omitted. It stays in `events` because later compactions supersede it in
    /// turn, and because the fidelity report needs its window ids.
    pub fn model_visible(&self) -> Vec<&Event> {
        let by_id: std::collections::HashMap<&str, &Event> = self
            .events
            .iter()
            .map(|event| (event.id.as_str(), event))
            .collect();
        crate::replay::resolve(self)
            .events
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect()
    }
}

/// Identity and provenance of the captured session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// Agent slug, e.g. `"codex"` or `"claude-code"`.
    pub agent: String,
    /// Agent CLI version as recorded in the session, when present.
    pub agent_version: Option<String>,
    /// The agent's own session identifier.
    pub native_session_id: String,
    /// Inference provider the session ran against, e.g. `"openai"`,
    /// `"anthropic"`, or a gateway's provider key.
    ///
    /// This gates [`Capsule`] replay, so it is deliberately separate from
    /// [`Origin::agent`]: the same agent can run against different providers.
    pub provider: Option<String>,
    /// Model name, when the session records one.
    pub model: Option<String>,
    /// Capture time, epoch milliseconds.
    pub captured_at: Option<i64>,
}

/// Filesystem and VCS context the session ran in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub cwd: Option<PathBuf>,
    /// Additional roots the agent was given access to (Codex `workspace_roots`).
    pub roots: Vec<PathBuf>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One event in the session graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Stable identifier, unique within the session.
    pub id: String,
    /// Parent in the session DAG. `None` for a root.
    pub parent: Option<String>,
    /// Which conversation branch this belongs to.
    pub branch: Branch,
    /// Turn grouping (Codex `turn_id`, Claude `promptId`), when known.
    pub turn: Option<String>,
    /// Event time, epoch milliseconds.
    pub ts: Option<i64>,
    /// Whether the model sees this, or only the user interface does.
    pub visibility: Visibility,
    /// The payload.
    pub body: Body,
    /// Opaque provider-bound material attached to this event.
    pub capsules: Vec<Capsule>,
    /// Pointer back to the bytes this was parsed from.
    pub source: SourceRef,
}

/// Which conversation branch an event belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Branch {
    /// The main conversation.
    Main,
    /// A subagent / sidechain, identified by the agent's own id for it.
    Sub(String),
}

/// Whether an event is part of the model's context or only the UI's history.
///
/// Getting this wrong is not cosmetic. Treating chrome as model-visible inflates
/// the target's context with rendering artifacts; treating model-visible events
/// as chrome silently truncates the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Part of the conversation sent to the model.
    Model,
    /// Rendered to the user, never sent to the model.
    Ui,
    /// Accounting and instrumentation; safe to drop, but say so.
    Telemetry,
    /// This version cannot tell. Never replayed, always reported.
    ///
    /// The alternative — defaulting an unrecognised record to [`Visibility::Ui`]
    /// — is an assumption that reads as a fact: a future release that adds a
    /// model-context record type would have it silently classified as chrome
    /// and dropped from every conversion, with nothing to show that anything
    /// was lost.
    Unclassified,
}

/// Event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// A conversational message.
    Message { role: Role, blocks: Vec<Block> },

    /// A reasoning step.
    ///
    /// `text` and `summary` are empty in practice for both current agents — the
    /// substance rides in [`Event::capsules`]. They exist for older sessions and
    /// for providers that do emit readable reasoning.
    Reasoning {
        text: Option<String>,
        summary: Vec<String>,
    },

    /// A tool invocation.
    ToolCall {
        /// Pairs with [`Body::ToolResult::call_id`].
        call_id: String,
        name: String,
        /// Tool namespace, where the agent records one.
        namespace: Option<String>,
        /// Arguments, carrying their own calling convention. See [`ToolInput`].
        input: ToolInput,
    },

    /// The result of a tool invocation.
    ToolResult {
        call_id: String,
        /// What the agent recorded about how it went. See [`ToolOutcome`] —
        /// in particular, why "no failure marker" is not "success".
        outcome: ToolOutcome,
        output: Vec<Block>,
        /// The agent's own structured result, when richer than `output`
        /// (Claude Code's `toolUseResult`, which has a dozen-plus shapes).
        structured: Option<serde_json::Value>,
    },

    /// Context compaction: the agent rewrote its own model-visible history.
    ///
    /// Present in roughly three quarters of real Codex rollouts. Any consumer
    /// that flattens the event list without honouring this will replay the
    /// pre-compaction history.
    ///
    /// This is a *marker*, not a container. Both the superseded events and the
    /// substituted ones are ordinary entries in [`SessionIr::events`]; only
    /// their ids appear here.
    Compaction {
        /// The COMPLETE model-visible context immediately after this
        /// operation, in order, by event id.
        ///
        /// A state assignment, not a patch: [`crate::replay::resolve`] does
        /// `live := context` and never diffs. An earlier revision modelled
        /// compaction as "remove these ids", which cannot express Codex —
        /// its replacement history is a *new* list sharing no ids with what
        /// came before. Holding the whole post-state lets both readers
        /// normalize onto one shape, so the fold never branches on the agent.
        ///
        /// Held by reference rather than inline. An earlier revision nested
        /// `Vec<Event>` here, which put those events outside every traversal
        /// in the crate: [`CaptureReport`] never counted them, so 87 MB of
        /// sealed Codex compaction state was being discarded with nothing in
        /// the capture report to show for it. One list, one traversal.
        context: Vec<String>,
        /// Ids that were live before this and are not in `context`.
        ///
        /// Derived — the fold could recompute it — but kept because the
        /// fidelity report has to say what was displaced, and because it is
        /// what each reader already computes on the way to `context`.
        supersedes: Vec<String>,
        /// The preamble the provider wrote to introduce the replacement, when
        /// it wrote one (Codex's `payload.message`).
        note: Option<String>,
        window_from: Option<String>,
        window_to: Option<String>,
    },

    /// Model context the provider will only hand back sealed.
    ///
    /// Codex's compaction does not return the compacted history as messages.
    /// It returns one `type: "compaction"` item carrying an
    /// `encrypted_content` blob, introduced by a preamble that reads *"here is
    /// the summary produced by the other language model"*. Across the corpus
    /// that is 4,325 items and 87.6 MB, and it is the entire pre-compaction
    /// history of three quarters of all rollouts.
    ///
    /// The blob itself rides in [`Event::capsules`], because it is subject to
    /// exactly the same replay rule as sealed reasoning. This body carries the
    /// little that is left around it. An event of this kind with its capsule
    /// dropped is not a degraded event — it is a hole where the conversation
    /// used to be, and a writer must grade the conversion accordingly.
    SealedContext {
        /// The provider's identifier for the artifact (Codex `cmp_…`).
        native_id: Option<String>,
        /// Provider metadata attached alongside it.
        meta: serde_json::Value,
    },

    /// Per-turn configuration (Codex `turn_context`).
    TurnConfig {
        model: Option<String>,
        effort: Option<String>,
        sandbox: Option<serde_json::Value>,
        approval: Option<serde_json::Value>,
        personality: Option<serde_json::Value>,
        instructions: Option<String>,
    },

    /// Ambient environment the agent was told about (Codex `world_state`,
    /// Claude Code's environment `system` records).
    EnvSnapshot { data: serde_json::Value },

    /// An attachment record (Claude Code `attachment`).
    Attachment {
        attachment_kind: String,
        data: serde_json::Value,
    },

    /// The last `turns` typed turns are no longer in the model's context.
    ///
    /// Typed rather than left as a [`Body::Control`] string because
    /// [`crate::replay::resolve`] has to act on it. When the resolver
    /// recognises a rollback by matching one provider's private wire
    /// vocabulary, a second provider with rollback semantics gets no rollback
    /// handling and nothing says so — its control events become ordinary
    /// conversation content.
    ///
    /// `turns` is 1 in all 714 corpus occurrences of Codex
    /// `thread_rolled_back`. A reader that finds no count writes 1 rather than
    /// "everything": a rollback that cannot say how far it goes must not be
    /// allowed to eat the session.
    ///
    /// A reader emits this at whatever visibility the provider's own record
    /// carries — Codex writes it as an `event_msg`, which is
    /// [`Visibility::Ui`] — because the resolver reads it *before* the
    /// visibility gate. Behind the gate the rule fires on zero of 714 real
    /// rollbacks.
    Rollback { turns: u32 },

    /// A turn was interrupted before it finished.
    ///
    /// An annotation, never a removal, and the type says so by carrying no
    /// scope to remove. Of the 2,304 aborts in the corpus 1,587 had no
    /// following rollback, and 286 of those had already produced real output
    /// that stayed in the model's context — so treating an abort as a removal
    /// deletes work the model saw.
    ///
    /// Carries no turn of its own. It briefly did, and on the only provider
    /// that emits aborts the value was identical to [`Event::turn`] for all
    /// 1,821 corpus occurrences, because the reader sets the event's turn from
    /// the same `turn_aborted.turn_id` before building it. Two fields holding
    /// one fact, with no reader for the second. A provider whose abort record
    /// names some *other* turn would need it back — with a consumer, at that
    /// point.
    ///
    /// Left as a braced variant with no fields so that adding one later does
    /// not churn every `Body::Abort { .. }` pattern in the crate.
    Abort {},

    /// Session control that carries no replay semantics: mode changes, queue
    /// operations, subagent plumbing, rendering echoes.
    ///
    /// The catch-all for genuine chrome, and deliberately *only* that. Anything
    /// the resolver has to act on gets its own variant, so that a new provider
    /// wires up correct replay by emitting the right variant from its reader
    /// rather than by teaching the resolver another vocabulary.
    Control {
        control_kind: String,
        data: serde_json::Value,
    },

    /// A line this version does not understand.
    ///
    /// The escape hatch that keeps format drift loud. Upstream's `_ => {}` made
    /// an unrecognised event indistinguishable from an absent one; this makes
    /// the count visible in [`CaptureReport::unknown`] and keeps a pointer to
    /// the original bytes so the loss is recoverable.
    Unknown {
        /// The agent's own type tag for the line, when it had one.
        native_type: Option<String>,
        raw: serde_json::Value,
    },
}

impl Body {
    /// Stable slug for counting and reporting.
    pub fn kind(&self) -> &'static str {
        match self {
            Body::Message { .. } => "message",
            Body::Reasoning { .. } => "reasoning",
            Body::ToolCall { .. } => "tool_call",
            Body::ToolResult { .. } => "tool_result",
            Body::Compaction { .. } => "compaction",
            Body::SealedContext { .. } => "sealed_context",
            Body::TurnConfig { .. } => "turn_config",
            Body::EnvSnapshot { .. } => "env_snapshot",
            Body::Attachment { .. } => "attachment",
            Body::Rollback { .. } => "rollback",
            Body::Abort { .. } => "abort",
            Body::Control { .. } => "control",
            Body::Unknown { .. } => "unknown",
        }
    }

    /// Whether this body instructs the replay fold rather than feeding the model.
    ///
    /// Exists to be exhaustive. [`crate::replay::resolve`] has to read these
    /// before its visibility gate, because Codex writes both as `event_msg` and
    /// the reader correctly files that as [`Visibility::Ui`] — rendering, not
    /// context. Behind the gate the rollback rule fires on zero of 714 real
    /// corpus rollbacks.
    ///
    /// Written as a `match` with every variant spelled out, and deliberately
    /// not as `matches!(self, Body::Rollback { .. } | Body::Abort { .. })`. The
    /// fold's own `match` already forces a decision about a new variant, but an
    /// inline `matches!` at the gate would not: a third directive left out of it
    /// is silently skipped on every provider that files control records as
    /// chrome, which is exactly the 0-of-714 failure the typed variants exist to
    /// kill. Here, omitting it does not compile.
    pub fn is_history_directive(&self) -> bool {
        match self {
            Body::Rollback { .. } | Body::Abort { .. } => true,
            Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::Compaction { .. }
            | Body::SealedContext { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => false,
        }
    }
}

/// The role a message was recorded under.
///
/// Deliberately not [`crate::model::MessageRole`]. The flat model folds
/// `developer` into `System` because it has nowhere else to put it, and the
/// corpus contains 4,864 Codex `developer` messages. The two are not
/// interchangeable: `developer` is instruction the operator injected into the
/// conversation, `system` is the harness's own preamble. A writer that cannot
/// tell them apart cannot decide which to carry across and which to let the
/// target agent supply for itself — and carrying the wrong one means the
/// resumed session runs under two conflicting sets of instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Developer,
    Tool,
    /// A role string this version does not recognise, kept verbatim.
    Other(String),
}

impl Role {
    /// Read a native role string without collapsing anything.
    pub fn from_native(role: &str) -> Self {
        match role.trim().to_ascii_lowercase().as_str() {
            "user" | "human" => Role::User,
            "assistant" | "model" | "agent" | "gemini" => Role::Assistant,
            "system" => Role::System,
            "developer" => Role::Developer,
            "tool" => Role::Tool,
            _ => Role::Other(role.to_string()),
        }
    }

    /// Projection onto the flat model, for the text track and for search.
    ///
    /// Lossy by construction — this is the collapse the IR exists to avoid,
    /// performed explicitly at the boundary instead of silently at read time.
    pub fn flat(&self) -> MessageRole {
        match self {
            Role::User => MessageRole::User,
            Role::Assistant => MessageRole::Assistant,
            Role::System | Role::Developer => MessageRole::System,
            Role::Tool => MessageRole::Tool,
            Role::Other(other) => MessageRole::Other(other.clone()),
        }
    }
}

/// How a tool call passes its arguments.
///
/// Codex uses both. Rewriting [`ToolProtocol::Freeform`] into
/// [`ToolProtocol::JsonArgs`] loses the distinction permanently, because a
/// freeform body is not required to be JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProtocol {
    /// Arguments are a JSON object (`function_call`, Claude `tool_use`).
    JsonArgs,
    /// Input is an opaque string the tool parses itself (`custom_tool_call`).
    Freeform,
}

/// A tool call's arguments, together with how they were passed.
///
/// One field rather than a `protocol` tag beside a bare `Value`, because that
/// pairing admits combinations that cannot occur — freeform input holding a
/// parsed object, JSON arguments holding a bare string — and leaves every
/// writer to work out at runtime which it got.
///
/// The `original` text matters more than it looks. Codex records
/// `function_call.arguments` as a *string* in all 24,366 corpus occurrences
/// while Claude Code records `tool_use.input` as an object. Without the
/// original, a same-agent write-back has to re-serialise and hope the key
/// order and spacing do not matter to anything downstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum ToolInput {
    /// Arguments the provider defines as a JSON object.
    Json {
        /// The parsed arguments.
        value: serde_json::Value,
        /// The exact text the provider wrote, when it wrote text.
        original: Option<String>,
    },
    /// Input the tool parses itself. Not required to be JSON.
    Freeform { text: String },
}

impl ToolInput {
    /// Which calling convention this is.
    pub fn protocol(&self) -> ToolProtocol {
        match self {
            ToolInput::Json { .. } => ToolProtocol::JsonArgs,
            ToolInput::Freeform { .. } => ToolProtocol::Freeform,
        }
    }

    /// Parse a provider's argument field, keeping the original when it was text.
    pub fn from_json_field(raw: &serde_json::Value) -> Self {
        match raw.as_str() {
            Some(text) => ToolInput::Json {
                value: serde_json::from_str(text).unwrap_or(serde_json::Value::Null),
                original: Some(text.to_string()),
            },
            None => ToolInput::Json {
                value: raw.clone(),
                original: None,
            },
        }
    }
}

/// What the agent recorded about how a tool call went.
///
/// Three states, because the corpus has three. Codex writes **no** success or
/// error marker on `function_call_output` or `custom_tool_call_output` — not
/// `success`, not `status`, not an exit code, in any of the 85,584 tool
/// outputs sampled. Reporting those as successes, which is what a `bool`
/// forces, is an assertion the data does not support; the earlier
/// implementation did exactly that for every Codex tool result in every
/// converted session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The agent recorded success.
    Succeeded,
    /// The agent recorded a failure, a timeout, or an error status.
    Failed,
    /// The agent recorded nothing either way.
    Unknown,
}

/// A piece of message or tool-result content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        /// `data:` URL or remote URL, as recorded.
        url: String,
        media_type: Option<String>,
    },
    Document {
        data: serde_json::Value,
    },
    /// Content the agent recorded but redacted.
    Redacted {
        reason: Option<String>,
    },
    /// A content block this version does not understand.
    ///
    /// The block-level counterpart of [`Body::Unknown`], and it was missing:
    /// both readers used to `filter_map` unrecognised blocks away, which
    /// discarded 4,562 sealed `encrypted_content` blocks sitting inside Codex
    /// `agent_message` content. A record-level escape hatch does not help when
    /// the loss happens one level down.
    Unknown {
        native_type: Option<String>,
        raw: serde_json::Value,
    },
}

impl Block {
    /// Plain-text projection, for the flat model and for search.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Block::Text { text } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Capsules
// ---------------------------------------------------------------------------

/// Opaque, provider-bound material carried verbatim.
///
/// Both current agents keep their reasoning in a blob only the issuing provider
/// can interpret: Anthropic in `thinking.signature`, OpenAI in
/// `reasoning.encrypted_content`. Codex additionally seals the *compacted
/// conversation* the same way. None of it is readable here and none of it
/// should be parsed, rewritten, or "normalised" — the only correct handling is
/// to move the bytes unchanged and to know when they may be replayed.
///
/// Replay rule: a capsule may be written into a target session only when
/// [`CapsuleBinding::provider`] matches the target's provider. Across providers
/// it must be dropped and the drop reported — never converted into a
/// placeholder, which costs context window and tells the model its own reasoning
/// was truncated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capsule {
    pub kind: CapsuleKind,
    pub bound: CapsuleBinding,
    /// The provider's own opaque string, stored exactly as it appeared.
    ///
    /// Both vendors already record base64 here, so there is nothing to decode
    /// and no reason to: decoding and re-encoding is an opportunity to corrupt
    /// a blob whose format is undocumented and whose only consumer is the
    /// provider that issued it.
    pub sealed: String,
}

/// Which vendor's sealed format a capsule holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleKind {
    /// Claude Code `thinking.signature`.
    AnthropicThinkingSignature,
    /// Claude Code `redacted_thinking.data`.
    ///
    /// Anthropic returns this when it declines to show its reasoning in the
    /// clear. It is sealed exactly like a signature and is meaningless to any
    /// other vendor, so it belongs here rather than in [`Block::Unknown`]:
    /// filed as an unknown block its bytes would survive ungated and be
    /// written verbatim into a rollout on the other side of the vendor
    /// boundary. Absent from the local corpus, which is why it went unnoticed
    /// — the gate is not a corpus finding, it is the invariant the corpus
    /// happens not to test.
    AnthropicRedactedThinking,
    /// Codex `reasoning.encrypted_content`.
    OpenaiReasoningEncryptedContent,
    /// Codex `compacted.replacement_history[].encrypted_content` — the
    /// compacted conversation itself, not reasoning about it. Losing this one
    /// costs history rather than train of thought.
    OpenaiCompactedContext,
}

impl CapsuleKind {
    /// The vendor whose format this is.
    ///
    /// Distinct from the endpoint a session was served by: 109 of the 591
    /// rollouts in the corpus ran through a gateway whose `model_provider` is
    /// its own name, but the blobs it relayed are still OpenAI's. Replay
    /// compatibility follows the format, so it is decided here rather than
    /// from [`Origin::provider`].
    pub fn vendor(self) -> &'static str {
        match self {
            CapsuleKind::AnthropicThinkingSignature
            | CapsuleKind::AnthropicRedactedThinking => "anthropic",
            CapsuleKind::OpenaiReasoningEncryptedContent
            | CapsuleKind::OpenaiCompactedContext => "openai",
        }
    }
}

/// What a capsule is valid for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleBinding {
    /// Provider that issued it, e.g. `"anthropic"`.
    pub provider: String,
    /// Model that issued it, when known.
    ///
    /// Whether a capsule survives a model change within one provider is not
    /// established; callers that want to be conservative should require this to
    /// match too.
    pub model: Option<String>,
}

/// What is known about replaying a capsule into a given target.
///
/// Deliberately two states rather than three. It is tempting to add a "safe"
/// tier for a matching vendor *and* model, but nothing in the corpus
/// establishes that a blob minted in one session is accepted in another, and
/// the IR path is only ever used to cross a session boundary. Naming a tier
/// "safe" on that evidence would be a guess wearing a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleFit {
    /// The target speaks this format. Whether it accepts *this* blob is the
    /// writer's policy call, and the outcome belongs in the fidelity report.
    SameVendor,
    /// The target cannot interpret this at all. Sending it is not a gamble,
    /// it is an error.
    ForeignVendor,
}

impl Capsule {
    /// How this capsule relates to a target running on `target_vendor`.
    ///
    /// Keyed on [`CapsuleKind::vendor`], not on [`CapsuleBinding::provider`].
    /// The binding records which *endpoint* served the session, and 109 of the
    /// 591 rollouts in the corpus were served by a gateway under its own name;
    /// gating on that string would throw away reasoning that is plainly
    /// OpenAI's, for no reason but a label.
    pub fn fits(&self, target_vendor: &str) -> CapsuleFit {
        if self.kind.vendor().eq_ignore_ascii_case(target_vendor.trim()) {
            CapsuleFit::SameVendor
        } else {
            CapsuleFit::ForeignVendor
        }
    }
}

// ---------------------------------------------------------------------------
// Provenance and reporting
// ---------------------------------------------------------------------------

/// Pointer from an IR event back to the bytes it was parsed from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    /// 1-based line number in the native session file.
    pub line: u64,
    /// SHA-256 of that line, so a stale IR can be detected against its source.
    pub sha256: String,
}

/// What a capture preserved, degraded, and could not represent.
///
/// Produced at capture time rather than inferred later, so the numbers describe
/// the source rather than the converter's opinion of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureReport {
    /// Native lines read.
    pub lines_read: u64,
    /// Events emitted, by [`Body::kind`].
    pub by_kind: std::collections::BTreeMap<String, u64>,
    /// Events emitted as [`Body::Unknown`]. Non-zero means format drift.
    pub unknown: u64,
    /// Capsules carried.
    pub capsules: u64,
    /// Human-readable notes about anything degraded during capture.
    pub notes: Vec<String>,
}

impl CaptureReport {
    /// Record one emitted event.
    pub fn record(&mut self, event: &Event) {
        *self.by_kind.entry(event.body.kind().to_string()).or_insert(0) += 1;
        if matches!(event.body, Body::Unknown { .. }) {
            self.unknown += 1;
        }
        self.capsules += event.capsules.len() as u64;
    }

    /// Add a note about something that could not be represented faithfully.
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
}

/// How faithful a completed conversion actually was.
///
/// Attached to every conversion and shown before launch. The point is that a
/// handoff which drops reasoning and downgrades tool protocols is reported as
/// [`Fidelity::ConversationOnly`] rather than as a restore.
///
/// # Ordering
///
/// Variants are declared best-first, and `Ord` follows declaration order, so
/// `max` over the per-aspect grades yields the *worst* one — which is the
/// conservative combination and the one to show the user. Read `a > b` as
/// "a is a worse outcome than b".
///
/// # Why losing reasoning ranks better than losing history
///
/// These two are easy to conflate, because on disk they look identical: both
/// are sealed blobs the issuing provider alone can read. They are not
/// comparable losses.
///
/// Dropping *reasoning* costs little. Anthropic strips prior-turn thinking
/// from the context on subsequent requests, so historical thinking blocks
/// would not have been shown to the model anyway; they matter only inside an
/// unfinished tool loop, which is where the transcript ends in 129 of the
/// 64,393 Codex tool calls sampled.
///
/// Dropping a [`Body::SealedContext`] capsule is categorically worse. That
/// blob is not reasoning about the conversation, it *is* the conversation:
/// Codex returns each compacted history sealed, and 75% of real rollouts are
/// compacted. Losing it deletes the past rather than the train of thought,
/// which is why it ranks below even [`Fidelity::TranscriptOnly`] — a
/// transcript at least still contains everything that was said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Native bytes replayed unchanged. Only possible same-agent.
    ByteIdentical,
    /// Native bytes plus path remapping. Same agent, different workspace.
    NativeEquivalent,
    /// Every model-visible event preserved, including capsules.
    ContextComplete,
    /// Model-visible events preserved; reasoning capsules dropped.
    ContextNoReasoning,
    /// Tool protocol downgraded, or compaction structure flattened, but every
    /// piece of the conversation is still present.
    ConversationOnly,
    /// Text survived; structure did not.
    TranscriptOnly,
    /// Part of the conversation itself could not be carried across.
    ///
    /// Currently means a sealed compaction was dropped. The resumed session
    /// will be missing history and will not know it.
    HistoryIncomplete,
}

impl Fidelity {
    /// One-line description for the pre-launch summary.
    pub fn describe(self) -> &'static str {
        match self {
            Fidelity::ByteIdentical => "byte-identical native replay",
            Fidelity::NativeEquivalent => "native replay with workspace remapping",
            Fidelity::ContextComplete => "all model-visible events preserved, reasoning intact",
            Fidelity::ContextNoReasoning => {
                "all model-visible events preserved, reasoning dropped across providers"
            }
            Fidelity::ConversationOnly => {
                "conversation preserved, tool protocol or compaction degraded"
            }
            Fidelity::TranscriptOnly => "text only, structure not preserved",
            Fidelity::HistoryIncomplete => {
                "INCOMPLETE: compacted history could not be carried across; \
                 the resumed session is missing conversation"
            }
        }
    }

    /// The worse of two grades.
    ///
    /// Spelled out rather than left to `max`, so that the direction of the
    /// comparison is stated once here instead of assumed at every call site.
    pub fn worse_of(self, other: Self) -> Self {
        self.max(other)
    }
}

/// One thing a conversion could not carry, and the grade it forced.
///
/// The writers already knew all of this — they were flattening it into warning
/// strings at the boundary. That meant the CLI could print a loss but nothing
/// could reason about one: the launch refusal, which is the single place the
/// detail matters most, fell back to a bare [`Fidelity::describe`] because the
/// counts had nowhere to travel. One structured list, rendered where it is
/// needed rather than pre-rendered at the source.
///
/// A [`Fidelity`] worse than [`Fidelity::ContextComplete`] should always be
/// explained by at least one `Loss`. A writer that degrades silently is
/// reporting a grade nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loss {
    pub kind: LossKind,
    /// How many events this affected.
    pub events: usize,
    /// How many sealed capsules went with them, and how many bytes those held.
    ///
    /// Both were prose-only at first, formatted into `note` by every producer
    /// that had them — the comparator tallies exactly these two numbers, and
    /// `pipeline::flat_fidelity` was summing `capsule.sealed.len()` into a
    /// sentence. A count a caller has to parse back out of English is not a
    /// count. Zero where the loss carried no sealed material.
    pub capsules: usize,
    pub bytes: usize,
    /// The grade this loss forces on its own. The conversion's grade is the
    /// worst across all of them.
    pub grade: Fidelity,
    /// One sentence for a human, written by the writer that has the context.
    pub note: String,
}

/// What kind of thing was lost.
///
/// Separate from the note so that callers can filter and rank without parsing
/// prose — the launch refusal cares only about [`LossKind::SealedContext`],
/// because that is the one that deletes conversation rather than degrading it.
/// A new provider that loses something genuinely new adds a variant here and
/// every `match` over it stops compiling until it is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossKind {
    /// Sealed compacted history. Deletes the conversation itself.
    SealedContext,
    /// Something the model was shown is simply not in the output.
    ///
    /// The most serious thing a comparison can find, and it needs its own name.
    /// It briefly filed under [`LossKind::Metadata`] — the vocabulary's
    /// blandest label — with the severity carried only by the `grade`, so a
    /// consumer filtering on `kind` (as the launch refusal does, on
    /// [`LossKind::SealedContext`]) would have walked straight past a deleted
    /// message. Distinct from `SealedContext`, which is history the vendor
    /// boundary made impossible to carry; this is history that went missing
    /// with no such excuse, and it is a writer bug.
    Conversation,
    /// Reasoning capsules minted by a vendor the target cannot replay.
    Reasoning,
    /// Media the target's content model has no type for.
    Media,
    /// A calling convention the target does not distinguish.
    ToolProtocol,
    /// Agent-side metadata with nowhere to live in the target.
    Metadata,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, visibility: Visibility, body: Body) -> Event {
        Event {
            id: id.to_string(),
            parent: None,
            branch: Branch::Main,
            turn: None,
            ts: None,
            visibility,
            body,
            capsules: Vec::new(),
            source: SourceRef {
                line: 1,
                sha256: String::new(),
            },
        }
    }

    fn message(id: &str, text: &str) -> Event {
        event(
            id,
            Visibility::Model,
            Body::Message {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: text.to_string(),
                }],
            },
        )
    }

    #[test]
    fn model_visible_skips_ui_and_telemetry() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(message("a", "kept"));
        ir.events.push(event(
            "b",
            Visibility::Ui,
            Body::Attachment {
                attachment_kind: "file".into(),
                data: serde_json::Value::Null,
            },
        ));
        ir.events.push(event(
            "c",
            Visibility::Telemetry,
            Body::Control {
                control_kind: "token_count".into(),
                data: serde_json::Value::Null,
            },
        ));

        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(visible, ["a"]);
    }

    #[test]
    fn compaction_replaces_superseded_events() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(message("old1", "before compaction"));
        ir.events.push(message("old2", "also before"));
        // The replacement is an ordinary event in the same list.
        ir.events.push(message("summary", "condensed"));
        ir.events.push(event(
            "compact",
            Visibility::Model,
            Body::Compaction {
                context: vec!["summary".into()],
                supersedes: vec!["old1".into(), "old2".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));
        ir.events.push(message("new", "after compaction"));

        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            visible,
            ["summary", "new"],
            "pre-compaction events must not be replayed, and the marker is not content"
        );
    }

    #[test]
    fn chained_compaction_keeps_only_the_last_replacement() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(message("old", "original"));
        ir.events.push(message("sum1", "first summary"));
        ir.events.push(event(
            "c1",
            Visibility::Model,
            Body::Compaction {
                context: vec!["sum1".into()],
                supersedes: vec!["old".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));
        ir.events.push(message("mid", "more work"));
        ir.events.push(message("sum2", "second summary"));
        ir.events.push(event(
            "c2",
            Visibility::Model,
            Body::Compaction {
                // A later compaction supersedes the earlier one's context and
                // everything added since.
                context: vec!["sum2".into()],
                supersedes: vec!["sum1".into(), "mid".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));

        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            visible,
            ["sum2"],
            "a superseded replacement must not survive its own compaction"
        );
    }

    #[test]
    fn sealed_context_is_replayable_content() {
        let mut ir = SessionIr::new("codex", "s1");
        let mut sealed = event(
            "cmp",
            Visibility::Model,
            Body::SealedContext {
                native_id: Some("cmp_abc".into()),
                meta: serde_json::Value::Null,
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
        ir.events.push(sealed);

        let visible = ir.model_visible();
        assert_eq!(visible.len(), 1, "sealed context is context, not chrome");
        assert_eq!(visible[0].capsules.len(), 1);
        assert_eq!(
            visible[0].capsules[0].kind.vendor(),
            "openai",
            "a gateway endpoint does not change whose format the blob is in"
        );
    }

    #[test]
    fn capsule_fit_follows_the_format_not_the_endpoint() {
        let capsule = Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".into(),
                model: Some("claude-opus-4-8".into()),
            },
            sealed: "AAAA".into(),
        };
        assert_eq!(capsule.fits("anthropic"), CapsuleFit::SameVendor);
        assert_eq!(capsule.fits("Anthropic"), CapsuleFit::SameVendor);
        assert_eq!(capsule.fits("openai"), CapsuleFit::ForeignVendor);

        // The gateway case: served by `sub2api`, still an OpenAI blob.
        let relayed = Capsule {
            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
            bound: CapsuleBinding {
                provider: "sub2api".into(),
                model: None,
            },
            sealed: "BBBB".into(),
        };
        assert_eq!(
            relayed.fits("openai"),
            CapsuleFit::SameVendor,
            "gating on the endpoint name would discard 109 rollouts' reasoning"
        );
        assert_eq!(relayed.fits("anthropic"), CapsuleFit::ForeignVendor);
    }

    #[test]
    fn capture_report_counts_kinds_and_capsules() {
        let mut report = CaptureReport::default();
        let mut with_capsule = message("a", "hi");
        with_capsule.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
            bound: CapsuleBinding {
                provider: "openai".into(),
                model: None,
            },
            sealed: "BBBB".into(),
        });
        report.record(&with_capsule);
        report.record(&event(
            "u",
            Visibility::Model,
            Body::Unknown {
                native_type: Some("brand_new_event".into()),
                raw: serde_json::Value::Null,
            },
        ));

        assert_eq!(report.by_kind.get("message"), Some(&1));
        assert_eq!(report.by_kind.get("unknown"), Some(&1));
        assert_eq!(report.unknown, 1);
        assert_eq!(report.capsules, 1);
    }

    #[test]
    fn losing_history_grades_worse_than_losing_reasoning() {
        assert!(
            Fidelity::HistoryIncomplete > Fidelity::ContextNoReasoning,
            "a dropped compaction deletes conversation; a dropped thinking \
             block deletes a train of thought the provider would have stripped \
             anyway"
        );
        assert!(Fidelity::HistoryIncomplete > Fidelity::TranscriptOnly);
        assert_eq!(
            Fidelity::ContextComplete.worse_of(Fidelity::HistoryIncomplete),
            Fidelity::HistoryIncomplete,
            "combining per-aspect grades must report the worst, not the best"
        );
        assert_eq!(
            Fidelity::ByteIdentical.worse_of(Fidelity::ByteIdentical),
            Fidelity::ByteIdentical
        );
    }

    #[test]
    fn ir_round_trips_through_json() {
        let mut ir = SessionIr::new("claude-code", "s1");
        ir.origin.provider = Some("anthropic".into());
        ir.events.push(message("a", "hello"));
        let encoded = serde_json::to_string(&ir).expect("encode");
        let decoded: SessionIr = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(ir, decoded);
    }
}
