//! Typed JSON response structs for all machine-readable CLI outputs.
//!
//! Every `--json` subcommand serializes one of these structs. Using concrete
//! `#[derive(Serialize)]` types instead of ad-hoc `serde_json::json!` objects
//! guarantees that field names, types, and `schema_version` are consistent
//! across the codebase and testable at compile time.

use std::path::PathBuf;

use serde::Serialize;

use crate::ir::{Body, Fidelity, SessionIr};
use crate::model::CanonicalSession;
use crate::store::{Availability, OriginState};

/// Current schema version for all JSON envelopes and per-record outputs.
///
/// Bump this when adding/removing/renaming fields in any response struct.
///
/// 3 added `losses`, `verified_fidelity` and `launch_error` to
/// [`ResumeSuccess`], and made its `ok` false when a launch could not be
/// prepared.
///
/// 4 added `detected_format`, `summary` and `live_summary` to
/// [`InfoResponse`], so that a caller can see which reader produced the
/// numbers, what the session holds by event type, and what of it the agent
/// would actually see. Both summaries report `null`, not `0`, for a count
/// their reader cannot establish — see [`EventSummary`]. It also made
/// [`ListItem::tool_uses`] an `Option`, because every provider but the four
/// with a scanner had no way to count tool uses and was reporting `0` for
/// every session.
///
/// 5 added `skipped` to [`ListEnvelope`]: the session files `list` found and
/// could not read, with the reader's own reason for each. `list` used to drop
/// them with `read_session(&path).ok()?`, so a broken file — or a whole broken
/// provider — subtracted itself from `items` and left nothing behind that a
/// caller could see. `items` alone cannot carry that fact: a short list and a
/// complete one are the same document. The field is always present and `[]`
/// when nothing was skipped, because an absent key would mean "old build", not
/// "clean run".
pub const SCHEMA_VERSION: u32 = 5;

// ---------------------------------------------------------------------------
// `list --json`
// ---------------------------------------------------------------------------

/// Versioned envelope wrapping `list --json` output.
#[derive(Debug, Clone, Serialize)]
pub struct ListEnvelope {
    pub schema_version: u32,
    pub items: Vec<ListItem>,
    /// Session files the listing found and could not read; see
    /// [`SkippedSession`].
    ///
    /// Serialized always, `[]` for a clean run. Omitting it when empty would
    /// make "this build cannot tell you" and "nothing was skipped" the same
    /// bytes, which is the distinction the field exists to draw.
    pub skipped: Vec<SkippedSession>,
}

impl ListEnvelope {
    pub fn new(items: Vec<ListItem>, skipped: Vec<SkippedSession>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            items,
            skipped,
        }
    }
}

/// One candidate `list` could not turn into a row, and the reader's reason.
///
/// # Why this is data and not only a warning
///
/// `list` reports two other things on stderr and nothing else — a store that
/// would not open, and sessions hidden by `--workspace` — so stderr is the
/// established channel for a listing's diagnostics, and this one is printed
/// there too. But `--json` exists for callers that read stdout and nothing
/// else, and for them a dropped session is invisible in exactly the way it is
/// invisible to a human: `items` is shorter, and nothing in the document says
/// shorter than what. The precedent that settles it is `resume --json`, which
/// used to print `ok: true` on stdout while the same run put its failure on
/// stderr and exited non-zero; the fix was to carry the failure in the envelope
/// (`launch_error`, schema 3), not to trust the caller to read two streams.
///
/// # Why the reader's own text
///
/// A reason is what makes the count actionable: "3 skipped" tells a user their
/// listing is incomplete, and "failed to parse JSON <path>" tells them whether
/// to repair the file, report a reader bug, or ignore a stray file that was
/// never a session. The reader already writes that sentence for `info`, which
/// reads the same file through the same call and fails loudly; `list` had the
/// text all along and threw it away with `.ok()`.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedSession {
    /// Slug of the provider whose reader was asked.
    pub provider: String,
    /// The file that could not be read.
    pub path: String,
    /// The reader's error, verbatim and unclassified — the same string `info`
    /// prints for the same file.
    pub error: String,
}

/// A single session entry in `list --json` output.
#[derive(Debug, Clone, Serialize)]
pub struct ListItem {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: String,
    pub title: Option<String>,
    /// Provider-native session name (e.g. Claude Code `/rename` title, Amp
    /// thread title). `null` for providers without such a concept.
    pub native_name: Option<String>,
    pub messages: usize,
    pub workspace: Option<String>,
    pub started_at: Option<i64>,
    pub last_active_at: Option<i64>,
    pub file_size_bytes: u64,
    pub file_size_kb: u64,
    pub unique_user_messages: usize,
    pub avg_agent_response_chars: f64,
    pub avg_agent_response_chars_rounded: u64,
    /// Tool invocations in the session, or `null` where nothing could count them.
    ///
    /// `list` counts these from [`crate::model::CanonicalMessage::tool_calls`],
    /// and falls back to a per-provider scan of the source file when that comes
    /// back empty — because an empty `tool_calls` means "this reader does not
    /// populate the field" on most providers and "no tool calls" on a few, and
    /// nothing distinguishes the two from the outside.
    ///
    /// Only four providers have such a scan. Every other one used to fall
    /// off the end of the match and report `0`, so every Aider, Cline, Cursor,
    /// Amp, OpenCode, ChatGPT, Vibe, Kiro, Grok and OpenClaw session in every
    /// `list --json` claimed to have made no tool calls at all. `null` says the
    /// only true thing available: nothing here could count them.
    pub tool_uses: Option<usize>,
    pub path: String,
    /// Workspace name derived from session metadata (directory basename or title).
    pub workspace_name: Option<String>,
    /// How `workspace_name` was determined.
    pub workspace_name_source: Option<String>,
    /// Repository name from filesystem git root (only when `--enrich-fs` is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-event-type counts
// ---------------------------------------------------------------------------

/// Events broken down by type, for one of the two questions below.
///
/// One field per [`Body`] variant, named exactly as [`Body::kind`] names it so
/// the two cannot drift apart, and serialized in that order with the four
/// keys an external consumer already parses — `message`, `reasoning`,
/// `tool_call`, `tool_result` — first.
///
/// # Two questions, two fields
///
/// `info --json` reports this shape twice, because "what is in this session"
/// and "what would the agent actually see" are different questions with
/// different answers, and a caller cannot derive either from the other.
///
/// - `summary` ([`Self::of_ir`]) counts every event in the file, superseded
///   history included.
/// - `live_summary` ([`Self::of_live`]) counts what [`crate::replay::resolve`]
///   says survives: after compaction has replaced the history, rollbacks have
///   removed turns, abandoned forks have been pruned and the visibility gate
///   has dropped the chrome.
///
/// Reporting only the first was measured against the corpus and is wrong for
/// the use this field exists for. Across 400 real Codex rollouts, **zero** had
/// `summary == live_summary`; the median session's live context is 12% of its
/// file (p05 2%, worst 0.6%), and 156,419 of 174,329 messages are superseded
/// or gated away. Claude Code is closer but not equal — 1 of 200 identical,
/// median 97%, and 5,054 of 8,837 messages not live. A checker comparing a
/// Codex source's *file* against its Claude conversion — which writes the live
/// context, not the superseded history — would fail every good conversion it
/// ever saw. Comparing `live_summary` to `live_summary` is the check that
/// holds; `summary` is what tells a human how much the file has been through.
///
/// Note that compaction is not the only cause and could not have been the only
/// field: `control`, `env_snapshot` and `turn_config` differ in 400 of 400
/// Codex sessions on the visibility gate alone, where only 303 are compacted.
///
/// # `null` is not `0`
///
/// `Some(n)` is a count. `None` — serialized as `null`, and never omitted —
/// means *the reader that produced this cannot tell*. That distinction is the
/// entire reason the field is worth having. The consumer this exists for counts
/// a source session, converts it, counts the target, and compares the two as
/// its own independent check that the conversion dropped nothing; a reader that
/// answers `0` for a category it cannot see makes a session that lost all of
/// its reasoning compare clean against one that never had any.
///
/// Which reader answers what is a property of the track, not of the session:
///
/// - The structured track ([`SessionIr`]) counts real [`Body`] variants, so
///   every one of its counts is a number. Zero there means zero.
/// - The flat track ([`CanonicalSession`]) is message-level. It has fields for
///   messages, tool calls and tool results, and nowhere at all to put
///   reasoning, compaction, sealed context, or any of the rest — those are not
///   absent from its sessions, they are invisible to it. They are `null`, and
///   `null` appears nowhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventSummary {
    pub message: Option<u64>,
    pub reasoning: Option<u64>,
    pub tool_call: Option<u64>,
    pub tool_result: Option<u64>,
    pub compaction: Option<u64>,
    pub sealed_context: Option<u64>,
    pub turn_config: Option<u64>,
    pub env_snapshot: Option<u64>,
    pub attachment: Option<u64>,
    pub rollback: Option<u64>,
    pub abort: Option<u64>,
    pub control: Option<u64>,
    pub unknown: Option<u64>,
}

impl EventSummary {
    /// Everything the file holds, superseded history included.
    ///
    /// Answers *what does this session contain*. See [`Self::of_live`] for the
    /// other question, and the type's own docs for why both are reported.
    pub fn of_ir(ir: &SessionIr) -> Self {
        Self::of_events(&ir.events)
    }

    /// Only what the replay fold says is still live.
    ///
    /// Answers *what would the agent actually see*. Delegates to
    /// [`SessionIr::model_visible`], so compaction, rollback, aborts and
    /// abandoned forks are all applied by the one fold in [`crate::replay`]
    /// rather than re-derived here — a second opinion about what survives is
    /// the bug that module exists to remove.
    pub fn of_live(ir: &SessionIr) -> Self {
        Self::of_events(ir.model_visible())
    }

    /// Count the real [`Body`] variants of a sequence of events.
    fn of_events<'a>(events: impl IntoIterator<Item = &'a crate::ir::Event>) -> Self {
        let mut counts = Self {
            message: Some(0),
            reasoning: Some(0),
            tool_call: Some(0),
            tool_result: Some(0),
            compaction: Some(0),
            sealed_context: Some(0),
            turn_config: Some(0),
            env_snapshot: Some(0),
            attachment: Some(0),
            rollback: Some(0),
            abort: Some(0),
            control: Some(0),
            unknown: Some(0),
        };
        for event in events {
            // Every variant is spelled out and there is no wildcard arm. A new
            // `Body` has to stop this compiling: the alternative is a variant
            // that quietly counts as nothing at all, which is precisely the
            // silent zero the whole type exists to prevent.
            let slot = match &event.body {
                Body::Message { .. } => &mut counts.message,
                Body::Reasoning { .. } => &mut counts.reasoning,
                Body::ToolCall { .. } => &mut counts.tool_call,
                Body::ToolResult { .. } => &mut counts.tool_result,
                Body::Compaction { .. } => &mut counts.compaction,
                Body::SealedContext { .. } => &mut counts.sealed_context,
                Body::TurnConfig { .. } => &mut counts.turn_config,
                Body::EnvSnapshot { .. } => &mut counts.env_snapshot,
                Body::Attachment { .. } => &mut counts.attachment,
                Body::Rollback { .. } => &mut counts.rollback,
                Body::Abort { .. } => &mut counts.abort,
                Body::Control { .. } => &mut counts.control,
                Body::Unknown { .. } => &mut counts.unknown,
            };
            *slot.get_or_insert(0) += 1;
        }
        counts
    }

    /// Count what a flat session can actually account for, and admit the rest.
    ///
    /// Serves both `summary` and `live_summary` on this track, and they are
    /// equal by construction rather than by coincidence: the flat track has no
    /// replay fold. Whatever the reader parsed is what the pipeline hands the
    /// target, so "what is in it" and "what survives" have the same answer.
    /// The thing the flat track cannot see — that the source agent compacted at
    /// all — is already reported, as `compaction: null` rather than as a `0`
    /// that would claim the session was never compacted.
    ///
    /// [`CanonicalSession`] carries messages, and each message carries its tool
    /// calls and tool results. Those three are real fields, so they are real
    /// counts. Nothing else on this track has a field to be counted from:
    /// reasoning is either flattened into a message's text or dropped on the
    /// floor depending on which of the nineteen flat readers ran, and
    /// compaction, sealed context, turn config, environment snapshots,
    /// attachments, rollbacks, aborts, control records and unrecognised lines
    /// have no representation at all. Reporting `0` for any of them would be
    /// this reader stating, on no evidence, that the session contains none.
    pub fn of_flat(session: &CanonicalSession) -> Self {
        Self {
            message: Some(session.messages.len() as u64),
            reasoning: None,
            tool_call: Some(
                session
                    .messages
                    .iter()
                    .map(|message| message.tool_calls.len() as u64)
                    .sum(),
            ),
            tool_result: Some(
                session
                    .messages
                    .iter()
                    .map(|message| message.tool_results.len() as u64)
                    .sum(),
            ),
            compaction: None,
            sealed_context: None,
            turn_config: None,
            env_snapshot: None,
            attachment: None,
            rollback: None,
            abort: None,
            control: None,
            unknown: None,
        }
    }

    /// The counts as name/value pairs, in serialization order.
    ///
    /// Field-by-field rather than derived from the serialized form, so the
    /// human rendering keeps the contract-first ordering that a `BTreeMap`
    /// would sort away. `summary_line_lists_every_json_key` pins this against
    /// the JSON so a field added to one and not the other fails a test.
    pub fn counts(&self) -> [(&'static str, Option<u64>); 13] {
        [
            ("message", self.message),
            ("reasoning", self.reasoning),
            ("tool_call", self.tool_call),
            ("tool_result", self.tool_result),
            ("compaction", self.compaction),
            ("sealed_context", self.sealed_context),
            ("turn_config", self.turn_config),
            ("env_snapshot", self.env_snapshot),
            ("attachment", self.attachment),
            ("rollback", self.rollback),
            ("abort", self.abort),
            ("control", self.control),
            ("unknown", self.unknown),
        ]
    }

    /// One terse line for the human output: `message 42, reasoning 18, …`.
    ///
    /// Counts of zero are dropped and unknowns are kept as `?`. A zero tells a
    /// reader nothing they could act on; a `?` tells them the number does not
    /// exist, which is the one thing about this report they must not guess at.
    pub fn describe(&self) -> String {
        let rendered: Vec<String> = self
            .counts()
            .iter()
            .filter_map(|(name, count)| match count {
                Some(0) => None,
                Some(count) => Some(format!("{name} {count}")),
                None => Some(format!("{name} ?")),
            })
            .collect();
        if rendered.is_empty() {
            "(empty)".to_string()
        } else {
            rendered.join(", ")
        }
    }
}

// ---------------------------------------------------------------------------
// `info --json`
// ---------------------------------------------------------------------------

/// Response struct for `info --json`.
#[derive(Debug, Clone, Serialize)]
pub struct InfoResponse {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: String,
    /// Slug of the reader that actually parsed this session.
    ///
    /// Distinct from `provider`, which is whatever the reader recorded on the
    /// session it produced. The two agree whenever detection was right, and
    /// when they do not this is the one that says where the numbers came from.
    /// A caller passing a path to a moved or copied session file — where
    /// detection is a signature guess rather than a directory lookup — has no
    /// other way to find out what it was read as.
    pub detected_format: String,
    pub title: Option<String>,
    /// Provider-native session name (e.g. Claude Code `/rename` title, Amp
    /// thread title). `null` for providers without such a concept.
    pub native_name: Option<String>,
    pub workspace: Option<String>,
    pub messages: usize,
    /// Everything in the file, by event type — superseded history included.
    ///
    /// Answers *what does this session contain*. `null` for a count this reader
    /// cannot establish, which is not the same answer as `0`; see
    /// [`EventSummary`].
    pub summary: EventSummary,
    /// What the replay fold says is still live, by event type.
    ///
    /// Answers *what would the agent actually see*. This is the one to compare
    /// across a conversion: a target is written from the live context, so
    /// checking it against the source's `summary` fails every conversion of a
    /// compacted session. Equal to `summary` on the flat track, which has no
    /// fold. See [`EventSummary`] for the corpus measurement behind the split.
    pub live_summary: EventSummary,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub model_name: Option<String>,
    pub source_path: String,
    pub metadata: serde_json::Value,
    /// Workspace name derived from session metadata (directory basename or title).
    pub workspace_name: Option<String>,
    /// How `workspace_name` was determined.
    pub workspace_name_source: Option<String>,
    /// Repository name from filesystem git root (only when `--enrich-fs` is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    /// Tail of the transcript (last few turns), present only with `--peek`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_tail: Option<Vec<crate::model::TranscriptTurn>>,
}

// ---------------------------------------------------------------------------
// `providers --json`
// ---------------------------------------------------------------------------

/// A single provider entry in `providers --json`.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub slug: String,
    pub alias: String,
    pub installed: bool,
    pub version: Option<String>,
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// `resume --json` (success)
// ---------------------------------------------------------------------------

/// Response struct for a successful `resume --json` (including dry-run).
#[derive(Debug, Clone, Serialize)]
pub struct ResumeSuccess {
    pub ok: bool,
    pub source_provider: String,
    pub target_provider: String,
    pub source_session_id: String,
    pub target_session_id: Option<String>,
    pub written_paths: Option<Vec<String>>,
    pub resume_command: Option<String>,
    pub dry_run: bool,
    /// How much of the session survived the conversion, as a snake_case grade
    /// (`"conversation_only"`, `"history_incomplete"`, …).
    ///
    /// The one field a script needs to decide whether the converted session is
    /// safe to resume unattended.
    pub fidelity: Fidelity,
    /// The grade an independent read-back of the written file supports.
    ///
    /// Present only when a structural verification actually ran, which is why
    /// it is `null` rather than absent on the flat track: "the check agreed"
    /// and "there was no check" are different answers and a script that has to
    /// tell them apart cannot do it from a missing key. When this is worse than
    /// `fidelity`, `fidelity` is the writer's claim and this is what the CLI
    /// itself acted on.
    pub verified_fidelity: Option<Fidelity>,
    /// What the grade is made of: kind, counts, bytes and the sentence.
    ///
    /// A grade names a category and nothing else, so a machine consumer used to
    /// get `"history_incomplete"` and no way to find out whether that was one
    /// 40-byte capsule or four hundred deleted turns — the counts existed, they
    /// were simply dropped on the way out. Empty when the conversion's grade is
    /// its track's baseline.
    pub losses: Vec<crate::ir::Loss>,
    /// The command `--launch` / `--launch-dry-run` resolved to, shell-quoted.
    ///
    /// `null` when no launch was asked for. Present under `--json` so the
    /// launcher no longer has to route its output to stderr to keep stdout
    /// parseable.
    pub launch_command: Option<String>,
    /// Whether `launch_command` actually names the converted session.
    ///
    /// `false` for the providers that have no session-id resume form: the file
    /// was written correctly, the agent simply will not be pointed at it.
    /// `null` when no launch was asked for, which is not the same as `false`.
    pub launch_targets_session: Option<bool>,
    /// Why the launch could not be prepared, when one was asked for and failed.
    ///
    /// Its presence is exactly the condition under which `ok` is false. The
    /// conversion itself still happened — `written_paths` is populated and
    /// correct — which is why this is a field on the success envelope rather
    /// than an error envelope that would have had to throw those paths away.
    pub launch_error: Option<String>,
    pub warnings: Vec<String>,
    /// The session store read a session other than the one that was named.
    ///
    /// Omitted entirely — not `null` — whenever the conversion read what it was
    /// asked to, which includes every `--no-store` run. That is deliberate: a
    /// script that worked before the store exists sees byte-identical JSON, and
    /// the field's mere presence is the signal that the source was substituted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_selection: Option<SourceSelectionJson>,
}

/// Which incarnation the store read instead, and what the named one would have
/// cost.
///
/// The human form is one sentence from `SourceChoice::explain`. This is the same
/// information as fields, because a caller that has to `grep` a sentence for
/// "would have cost 3 capsules" has no contract at all.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSelectionJson {
    /// Our identifier for the conversation — the one `resume <record-id>` takes.
    pub record_id: String,
    /// Provider slug of the session that was actually read.
    pub provider: String,
    /// That session's provider-native id.
    pub session_id: String,
    /// `"origin"` or `"derived"`.
    pub role: String,
    /// `"ready"` when the session's own file was read, `"archived"` when the
    /// live origin was gone and the store's byte copy stood in.
    pub availability: String,
    /// How this incarnation's recorded snapshot resolved: `"unchanged"`,
    /// `"unchanged_verified"`, `"grew"`, or `"unavailable"`.
    ///
    /// All four are reported. `"unchanged"` without `_verified` is the cheap
    /// answer — same size and mtime, bytes not re-read — and says so rather than
    /// claiming a verification it did not run.
    ///
    /// A derived incarnation carries one too, taken when this tool wrote the
    /// session, and `"grew"` on a `"derived"` role is the signal that matters
    /// most: the user has worked in that session since, and those turns exist
    /// nowhere else. `null` only where there is no snapshot to resolve — a record
    /// written before derived incarnations were snapshotted, which is never
    /// migrated.
    pub origin_state: Option<String>,
    /// Why that resolution, in the store's own words. `null` when unchanged.
    pub origin_detail: Option<String>,
    /// Capsules in the chosen source that the target's vendor can replay.
    pub capsules: usize,
    /// Bytes of sealed material behind `capsules`.
    pub capsule_bytes: usize,
    /// Provider slug of the session the user named.
    pub named_provider: String,
    /// Provider-native id of the session the user named.
    pub named_session_id: String,
    /// Capsules that reading the named session instead would have cost.
    pub cost_capsules: usize,
    /// Bytes of sealed material behind `cost_capsules`.
    pub cost_capsule_bytes: usize,
}

impl SourceSelectionJson {
    /// The structured form of a selection, or `None` when the store read exactly
    /// what it was asked to and there is nothing to report.
    ///
    /// That includes a record the store could not resolve — two incarnations that
    /// each hold work the other does not, or one whose growth cannot be measured.
    /// The store falls back to the session the user named there, so by this
    /// field's own rule there is no substitution to report; what a caller needs to
    /// see is carried by `warnings`, which is where the pipeline puts the cost of
    /// both sides. Adding a field for it would grow the substitution contract to
    /// cover a case where nothing was substituted.
    pub fn of(selection: &crate::pipeline::SourceSelection) -> Option<Self> {
        if !selection.overrode() {
            return None;
        }
        let chosen = selection.chosen()?;
        let named = selection.choice.find(&selection.named);
        let (origin_state, origin_detail) = match &chosen.origin_state {
            None => (None, None),
            Some(state) => match state {
                OriginState::Unchanged { rehashed: false } => (Some("unchanged"), None),
                OriginState::Unchanged { rehashed: true } => (Some("unchanged_verified"), None),
                OriginState::Grew { .. } => (Some("grew"), Some(state.describe())),
                OriginState::Unavailable { .. } => (Some("unavailable"), Some(state.describe())),
            },
        };
        Some(Self {
            record_id: selection.record_id.clone(),
            provider: chosen.key.provider.clone(),
            session_id: chosen.key.provider_session_id.clone(),
            role: chosen.label().to_string(),
            availability: match &chosen.availability {
                Availability::Ready => "ready",
                Availability::Archived => "archived",
                // Not reachable through `chosen()`, which filters on
                // readability; spelled out so a new variant is a compile error.
                Availability::Unavailable { .. } => "unavailable",
            }
            .to_string(),
            origin_state: origin_state.map(str::to_string),
            origin_detail,
            capsules: chosen.capsules.fitting(),
            capsule_bytes: chosen.capsules.fitting_bytes(),
            named_provider: selection.named.provider.clone(),
            named_session_id: selection.named.provider_session_id.clone(),
            cost_capsules: chosen
                .capsules
                .fitting()
                .saturating_sub(named.map_or(0, |named| named.capsules.fitting())),
            cost_capsule_bytes: chosen
                .capsules
                .fitting_bytes()
                .saturating_sub(named.map_or(0, |named| named.capsules.fitting_bytes())),
        })
    }
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// JSON envelope for error responses.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub ok: bool,
    pub error_type: String,
    pub message: String,
}

impl ErrorEnvelope {
    pub fn new(error_type: &str, message: String) -> Self {
        Self {
            ok: false,
            error_type: error_type.to_string(),
            message,
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace name derivation
// ---------------------------------------------------------------------------

/// Source description for how `workspace_name` was resolved.
pub const WS_NAME_SOURCE_SESSION_PATH: &str = "session_workspace_path";
pub const WS_NAME_SOURCE_NONE: &str = "none";

/// Derive a human-readable workspace name from a workspace path.
///
/// Returns the last component of the path (the directory name) as the name,
/// along with the source tag describing how it was derived.
pub fn workspace_name_from_path(workspace: Option<&PathBuf>) -> (Option<String>, Option<String>) {
    match workspace {
        Some(ws) => {
            let name = ws
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            if name.is_some() {
                (name, Some(WS_NAME_SOURCE_SESSION_PATH.to_string()))
            } else {
                (None, Some(WS_NAME_SOURCE_NONE.to_string()))
            }
        }
        None => (None, Some(WS_NAME_SOURCE_NONE.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Schema version is consistent
    // -----------------------------------------------------------------------

    #[test]
    fn schema_version_is_5() {
        assert_eq!(SCHEMA_VERSION, 5);
    }

    // -----------------------------------------------------------------------
    // EventSummary
    // -----------------------------------------------------------------------

    use crate::ir::{Block, Branch, Event, Role, SourceRef, Visibility};
    use crate::model::{CanonicalMessage, MessageRole, ToolCall, ToolResult};

    fn ir_event(id: &str, body: Body) -> Event {
        Event {
            id: id.to_string(),
            parent: None,
            branch: Branch::Main,
            turn: None,
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

    /// One of every [`Body`] variant, so that the counter is exercised on all
    /// thirteen rather than on the four anyone thinks about.
    fn ir_with_one_of_each() -> SessionIr {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events = vec![
            ir_event(
                "a",
                Body::Message {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "hello".into(),
                    }],
                },
            ),
            ir_event(
                "b",
                Body::Reasoning {
                    text: None,
                    summary: Vec::new(),
                },
            ),
            ir_event(
                "c",
                Body::ToolCall {
                    call_id: "call-1".into(),
                    name: "shell".into(),
                    namespace: None,
                    input: crate::ir::ToolInput::Freeform { text: "ls".into() },
                },
            ),
            ir_event(
                "d",
                Body::ToolResult {
                    call_id: "call-1".into(),
                    outcome: crate::ir::ToolOutcome::Unknown,
                    output: Vec::new(),
                    structured: None,
                },
            ),
            ir_event(
                "e",
                Body::Compaction {
                    context: Vec::new(),
                    supersedes: Vec::new(),
                    note: None,
                    window_from: None,
                    window_to: None,
                },
            ),
            ir_event(
                "f",
                Body::SealedContext {
                    native_id: None,
                    meta: serde_json::Value::Null,
                },
            ),
            ir_event(
                "g",
                Body::TurnConfig {
                    model: None,
                    effort: None,
                    sandbox: None,
                    approval: None,
                    personality: None,
                    instructions: None,
                },
            ),
            ir_event(
                "h",
                Body::EnvSnapshot {
                    data: serde_json::Value::Null,
                },
            ),
            ir_event(
                "i",
                Body::Attachment {
                    attachment_kind: "file".into(),
                    data: serde_json::Value::Null,
                },
            ),
            ir_event("j", Body::Rollback { turns: 1 }),
            ir_event("k", Body::Abort {}),
            ir_event(
                "l",
                Body::Control {
                    control_kind: "token_count".into(),
                    data: serde_json::Value::Null,
                },
            ),
            ir_event(
                "m",
                Body::Unknown {
                    native_type: Some("brand_new".into()),
                    raw: serde_json::Value::Null,
                },
            ),
        ];
        ir
    }

    #[test]
    fn structured_summary_counts_every_body_variant() {
        let summary = EventSummary::of_ir(&ir_with_one_of_each());
        for (name, count) in summary.counts() {
            assert_eq!(
                count,
                Some(1),
                "{name} should have been counted once; a variant the match \
                 forgot would show up here as 0"
            );
        }
    }

    #[test]
    fn structured_summary_counts_repeats_and_reports_real_zeroes() {
        let mut ir = SessionIr::new("codex", "s1");
        for id in ["a", "b", "c"] {
            ir.events.push(ir_event(
                id,
                Body::Message {
                    role: Role::Assistant,
                    blocks: Vec::new(),
                },
            ));
        }
        let summary = EventSummary::of_ir(&ir);
        assert_eq!(summary.message, Some(3));
        // Zero on this track is a fact about the session, not a shrug: the
        // reader looked at every event and none of them was reasoning.
        assert_eq!(summary.reasoning, Some(0));
        assert_eq!(summary.sealed_context, Some(0));
    }

    #[test]
    fn structured_summary_totals_every_event() {
        let ir = ir_with_one_of_each();
        let summary = EventSummary::of_ir(&ir);
        let total: u64 = summary
            .counts()
            .iter()
            .map(|(_, count)| count.expect("the structured track knows every count"))
            .sum();
        assert_eq!(
            total as usize,
            ir.events.len(),
            "every event lands in exactly one bucket"
        );
    }

    fn flat_session(messages: Vec<CanonicalMessage>) -> CanonicalSession {
        CanonicalSession {
            session_id: "flat-1".to_string(),
            provider_slug: "opencode".to_string(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages,
            metadata: serde_json::Value::Null,
            source_path: PathBuf::from("/tmp/flat.json"),
            model_name: None,
        }
    }

    fn flat_message(idx: usize, role: MessageRole) -> CanonicalMessage {
        CanonicalMessage {
            idx,
            role,
            content: "text".to_string(),
            timestamp: None,
            author: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            extra: serde_json::Value::Null,
        }
    }

    /// The rule the whole field exists for: the flat track says `null`, not `0`,
    /// for everything it cannot see.
    ///
    /// The consumer is a wrapper that counts a source session, converts it,
    /// counts the target and compares. If a flat reader reported `0` reasoning
    /// events, a conversion that deleted eight hundred of them would compare
    /// clean against a source that reported the same `0` — a catastrophic loss
    /// rendered as agreement. `null` is not a count and cannot be subtracted;
    /// that is the point.
    #[test]
    fn flat_summary_says_null_not_zero_for_what_it_cannot_see() {
        let mut with_tools = flat_message(1, MessageRole::Assistant);
        with_tools.tool_calls = vec![ToolCall {
            id: Some("call-1".into()),
            name: "shell".into(),
            arguments: serde_json::Value::Null,
        }];
        with_tools.tool_results = vec![ToolResult {
            call_id: Some("call-1".into()),
            content: "ok".into(),
            is_error: false,
        }];
        let session = flat_session(vec![flat_message(0, MessageRole::User), with_tools]);

        let summary = EventSummary::of_flat(&session);
        assert_eq!(summary.message, Some(2));
        assert_eq!(summary.tool_call, Some(1));
        assert_eq!(summary.tool_result, Some(1));

        let json = serde_json::to_value(&summary).unwrap();
        // The three the flat model has fields for are numbers.
        assert_eq!(json["message"], 2);
        assert_eq!(json["tool_call"], 1);
        assert_eq!(json["tool_result"], 1);
        // Everything else is `null`, present, and never 0.
        for unknowable in [
            "reasoning",
            "compaction",
            "sealed_context",
            "turn_config",
            "env_snapshot",
            "attachment",
            "rollback",
            "abort",
            "control",
            "unknown",
        ] {
            assert!(
                json.as_object().unwrap().contains_key(unknowable),
                "{unknowable} must be present, not omitted: absent and \
                 unknowable are different answers"
            );
            assert!(
                json[unknowable].is_null(),
                "{unknowable} must be null on the flat track, not {}: 0 would \
                 claim the session has none",
                json[unknowable]
            );
        }
    }

    /// The fold's answer is a subset of the file's, and a strict one here.
    ///
    /// A compaction replaces the history: the superseded messages are still in
    /// `summary` because they are still in the file, and absent from
    /// `live_summary` because the agent will never see them again. A caller
    /// that had only `summary` would compare a compacted source against its
    /// conversion and see events the target was never supposed to contain.
    #[test]
    fn live_summary_drops_what_compaction_superseded() {
        let mut ir = SessionIr::new("codex", "s1");
        for id in ["old1", "old2", "old3"] {
            ir.events.push(ir_event(
                id,
                Body::Message {
                    role: Role::User,
                    blocks: Vec::new(),
                },
            ));
        }
        ir.events.push(ir_event(
            "summary",
            Body::Message {
                role: Role::Assistant,
                blocks: Vec::new(),
            },
        ));
        ir.events.push(ir_event(
            "compact",
            Body::Compaction {
                context: vec!["summary".into()],
                supersedes: vec!["old1".into(), "old2".into(), "old3".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));

        let all = EventSummary::of_ir(&ir);
        let live = EventSummary::of_live(&ir);
        assert_eq!(all.message, Some(4), "the file still holds all four");
        assert_eq!(live.message, Some(1), "only the replacement survives");
        // The marker is an instruction about content, not content.
        assert_eq!(all.compaction, Some(1));
        assert_eq!(live.compaction, Some(0));
        assert_ne!(all, live, "the two fields must be able to disagree");
    }

    /// Chrome is not live either, and that is a second, independent reason the
    /// two fields differ — it fires on sessions that were never compacted.
    #[test]
    fn live_summary_drops_chrome_even_without_compaction() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(ir_event(
            "m",
            Body::Message {
                role: Role::User,
                blocks: Vec::new(),
            },
        ));
        let mut chrome = ir_event(
            "c",
            Body::Control {
                control_kind: "token_count".into(),
                data: serde_json::Value::Null,
            },
        );
        chrome.visibility = Visibility::Telemetry;
        ir.events.push(chrome);

        let all = EventSummary::of_ir(&ir);
        let live = EventSummary::of_live(&ir);
        assert_eq!((all.message, all.control), (Some(1), Some(1)));
        assert_eq!((live.message, live.control), (Some(1), Some(0)));
    }

    /// The fold can only remove, so `live` is bounded by `all` on every key.
    #[test]
    fn live_summary_never_exceeds_summary() {
        let ir = ir_with_one_of_each();
        let all = EventSummary::of_ir(&ir);
        let live = EventSummary::of_live(&ir);
        for ((kind, all_count), (_, live_count)) in all.counts().iter().zip(live.counts().iter()) {
            assert!(
                live_count <= all_count,
                "{kind}: live {live_count:?} exceeds all {all_count:?}"
            );
        }
    }

    /// A flat session that genuinely holds no tool traffic still says `0`.
    ///
    /// The null rule is about what the *model* cannot represent, not about
    /// making every flat number vanish — `tool_calls` is a real field, so an
    /// empty one is a real zero.
    #[test]
    fn flat_summary_reports_zero_where_the_model_has_a_field() {
        let session = flat_session(vec![flat_message(0, MessageRole::User)]);
        let summary = EventSummary::of_flat(&session);
        assert_eq!(summary.tool_call, Some(0));
        assert_eq!(summary.tool_result, Some(0));
        assert_eq!(summary.reasoning, None);
    }

    /// The four names an external consumer already parses, spelled exactly.
    #[test]
    fn the_four_contract_keys_are_always_present() {
        for summary in [
            EventSummary::of_ir(&ir_with_one_of_each()),
            EventSummary::of_flat(&flat_session(Vec::new())),
        ] {
            let json = serde_json::to_value(&summary).unwrap();
            let object = json.as_object().unwrap();
            for key in ["message", "reasoning", "tool_call", "tool_result"] {
                assert!(object.contains_key(key), "{key} is a contract key");
            }
        }
    }

    /// The human line and the JSON cannot drift apart.
    #[test]
    fn summary_line_lists_every_json_key() {
        let summary = EventSummary::of_ir(&ir_with_one_of_each());
        let json = serde_json::to_value(&summary).unwrap();
        let mut from_json: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
        from_json.sort();
        let mut from_counts: Vec<String> = summary
            .counts()
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        from_counts.sort();
        assert_eq!(
            from_counts, from_json,
            "a field added to EventSummary must be added to counts() too, or \
             the human output silently loses it"
        );

        let described = summary.describe();
        for (name, _) in summary.counts() {
            assert!(described.contains(name), "{name} missing from {described}");
        }
    }

    #[test]
    fn summary_line_drops_zeroes_and_keeps_unknowns() {
        let described = EventSummary::of_flat(&flat_session(Vec::new())).describe();
        assert!(
            described.contains("reasoning ?"),
            "an unknown count must render as ?, not as a number: {described}"
        );
        assert!(
            !described.contains("tool_call 0"),
            "a real zero carries nothing a reader can act on: {described}"
        );
    }

    // -----------------------------------------------------------------------
    // ListEnvelope serialization
    // -----------------------------------------------------------------------

    #[test]
    fn list_envelope_empty_items_serializes() {
        let envelope = ListEnvelope::new(vec![], vec![]);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 5);
        assert!(json["items"].as_array().unwrap().is_empty());
        // Present and empty, not absent: "nothing was skipped" is a
        // measurement and has to look different from a build that never made
        // one.
        assert!(json["skipped"].as_array().unwrap().is_empty());
    }

    /// A skipped file keeps its provider, its path and the reader's sentence.
    #[test]
    fn list_envelope_carries_skipped_sessions() {
        let envelope = ListEnvelope::new(
            vec![],
            vec![SkippedSession {
                provider: "gemini".to_string(),
                path: "/tmp/chats/session-1.json".to_string(),
                error: "failed to parse JSON /tmp/chats/session-1.json".to_string(),
            }],
        );
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["skipped"].as_array().unwrap().len(), 1);
        assert_eq!(json["skipped"][0]["provider"], "gemini");
        assert_eq!(json["skipped"][0]["path"], "/tmp/chats/session-1.json");
        assert_eq!(
            json["skipped"][0]["error"],
            "failed to parse JSON /tmp/chats/session-1.json"
        );
    }

    #[test]
    fn list_envelope_with_items_serializes() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid-1".to_string(),
            provider: "claude-code".to_string(),
            title: Some("Test".to_string()),
            native_name: Some("Renamed Session".to_string()),
            messages: 10,
            workspace: Some("/data/projects/test".to_string()),
            started_at: Some(1_700_000_000_000),
            last_active_at: Some(1_700_001_000_000),
            file_size_bytes: 4096,
            file_size_kb: 4,
            unique_user_messages: 3,
            avg_agent_response_chars: 500.5,
            avg_agent_response_chars_rounded: 501,
            tool_uses: Some(7),
            path: "/tmp/session.jsonl".to_string(),
            workspace_name: Some("test".to_string()),
            workspace_name_source: Some("session_workspace_path".to_string()),
            repo_name: None,
        };
        let envelope = ListEnvelope::new(vec![item], vec![]);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 5);
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        let first = &json["items"][0];
        assert_eq!(first["schema_version"], 5);
        assert_eq!(first["session_id"], "sid-1");
        assert_eq!(first["provider"], "claude-code");
        assert_eq!(first["native_name"], "Renamed Session");
        assert_eq!(first["messages"], 10);
        assert_eq!(first["workspace_name"], "test");
        assert_eq!(first["workspace_name_source"], "session_workspace_path");
    }

    // -----------------------------------------------------------------------
    // InfoResponse serialization
    // -----------------------------------------------------------------------

    #[test]
    fn info_response_serializes_all_fields() {
        let info = InfoResponse {
            schema_version: SCHEMA_VERSION,
            session_id: "sid-info".to_string(),
            provider: "codex".to_string(),
            detected_format: "codex".to_string(),
            title: None,
            native_name: None,
            workspace: None,
            messages: 5,
            summary: EventSummary::of_ir(&ir_with_one_of_each()),
            live_summary: EventSummary::of_live(&ir_with_one_of_each()),
            started_at: None,
            ended_at: None,
            model_name: Some("gpt-4".to_string()),
            source_path: "/tmp/session.jsonl".to_string(),
            metadata: serde_json::json!({"key": "value"}),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
            transcript_tail: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["schema_version"], 5);
        assert_eq!(json["session_id"], "sid-info");
        assert_eq!(json["provider"], "codex");
        assert!(json["title"].is_null());
        assert!(json["native_name"].is_null());
        assert!(
            !json.as_object().unwrap().contains_key("transcript_tail"),
            "transcript_tail must be omitted when not peeking"
        );
        assert!(json["workspace"].is_null());
        assert_eq!(json["messages"], 5);
        assert_eq!(json["model_name"], "gpt-4");
        assert!(json["workspace_name"].is_null());
        assert_eq!(json["workspace_name_source"], "none");
        assert_eq!(json["detected_format"], "codex");
        assert_eq!(json["summary"]["message"], 1);
        assert_eq!(json["summary"]["tool_call"], 1);
    }

    // -----------------------------------------------------------------------
    // ProviderInfo serialization
    // -----------------------------------------------------------------------

    #[test]
    fn provider_info_serializes() {
        let pi = ProviderInfo {
            name: "Claude Code".to_string(),
            slug: "claude-code".to_string(),
            alias: "cc".to_string(),
            installed: true,
            version: Some("1.0".to_string()),
            evidence: vec!["found binary".to_string()],
        };
        let json = serde_json::to_value(&pi).unwrap();
        assert_eq!(json["name"], "Claude Code");
        assert_eq!(json["slug"], "claude-code");
        assert_eq!(json["alias"], "cc");
        assert_eq!(json["installed"], true);
        assert_eq!(json["version"], "1.0");
        assert_eq!(json["evidence"][0], "found binary");
    }

    // -----------------------------------------------------------------------
    // ResumeSuccess serialization
    // -----------------------------------------------------------------------

    #[test]
    fn resume_success_dry_run_serializes() {
        let rs = ResumeSuccess {
            ok: true,
            source_provider: "claude-code".to_string(),
            target_provider: "codex".to_string(),
            source_session_id: "sid-src".to_string(),
            target_session_id: None,
            written_paths: None,
            resume_command: None,
            dry_run: true,
            fidelity: Fidelity::ConversationOnly,
            verified_fidelity: None,
            losses: vec![],
            launch_command: None,
            launch_targets_session: None,
            launch_error: None,
            warnings: vec![],
            source_selection: None,
        };
        let json = serde_json::to_value(&rs).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["dry_run"], true);
        assert!(json["target_session_id"].is_null());
        assert!(json["written_paths"].is_null());
        assert!(json["resume_command"].is_null());
        assert_eq!(json["fidelity"], "conversation_only");
        // Not `false`: no launch was asked for, so there is nothing to target.
        assert!(json["launch_command"].is_null());
        assert!(json["launch_targets_session"].is_null());
    }

    #[test]
    fn resume_success_actual_write_serializes() {
        let rs = ResumeSuccess {
            ok: true,
            source_provider: "codex".to_string(),
            target_provider: "claude-code".to_string(),
            source_session_id: "sid-src".to_string(),
            target_session_id: Some("sid-tgt".to_string()),
            written_paths: Some(vec!["/tmp/written.jsonl".to_string()]),
            resume_command: Some("claude --resume sid-tgt".to_string()),
            dry_run: false,
            fidelity: Fidelity::HistoryIncomplete,
            verified_fidelity: Some(Fidelity::HistoryIncomplete),
            losses: vec![crate::ir::Loss {
                kind: crate::ir::LossKind::SealedContext,
                events: 1,
                capsules: 1,
                bytes: 87_000,
                grade: Fidelity::HistoryIncomplete,
                note: "one sealed capsule could not cross".to_string(),
            }],
            launch_command: Some("claude --resume sid-tgt".to_string()),
            launch_targets_session: Some(true),
            launch_error: None,
            warnings: vec!["missing workspace".to_string()],
            source_selection: None,
        };
        let json = serde_json::to_value(&rs).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["dry_run"], false);
        assert_eq!(json["target_session_id"], "sid-tgt");
        assert_eq!(json["written_paths"][0], "/tmp/written.jsonl");
        assert_eq!(json["resume_command"], "claude --resume sid-tgt");
        assert_eq!(json["warnings"][0], "missing workspace");
        assert_eq!(json["fidelity"], "history_incomplete");
        assert_eq!(json["verified_fidelity"], "history_incomplete");
        // The counts a machine consumer could not previously reach: a grade on
        // its own cannot distinguish one 87 kB capsule from four hundred
        // deleted turns.
        assert_eq!(json["losses"][0]["kind"], "sealed_context");
        assert_eq!(json["losses"][0]["capsules"], 1);
        assert_eq!(json["losses"][0]["bytes"], 87_000);
        assert_eq!(json["losses"][0]["grade"], "history_incomplete");
        assert!(json["launch_error"].is_null());
        assert_eq!(json["launch_command"], "claude --resume sid-tgt");
        assert_eq!(json["launch_targets_session"], true);
    }

    // -----------------------------------------------------------------------
    // ErrorEnvelope serialization
    // -----------------------------------------------------------------------

    #[test]
    fn list_item_repo_name_omitted_when_none() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            title: None,
            native_name: None,
            messages: 0,
            workspace: None,
            started_at: None,
            last_active_at: None,
            file_size_bytes: 0,
            file_size_kb: 0,
            unique_user_messages: 0,
            avg_agent_response_chars: 0.0,
            avg_agent_response_chars_rounded: 0,
            tool_uses: None,
            path: "/tmp/x".to_string(),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("repo_name"),
            "repo_name should be omitted from JSON when None"
        );
        assert!(
            json.as_object().unwrap().contains_key("native_name"),
            "native_name is always present (null when absent)"
        );
    }

    #[test]
    fn list_item_repo_name_present_when_set() {
        let item = ListItem {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            title: None,
            native_name: None,
            messages: 0,
            workspace: Some("/data/projects/my_repo".to_string()),
            started_at: None,
            last_active_at: None,
            file_size_bytes: 0,
            file_size_kb: 0,
            unique_user_messages: 0,
            avg_agent_response_chars: 0.0,
            avg_agent_response_chars_rounded: 0,
            tool_uses: None,
            path: "/tmp/x".to_string(),
            workspace_name: Some("my_repo".to_string()),
            workspace_name_source: Some("session_workspace_path".to_string()),
            repo_name: Some("my_repo".to_string()),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["repo_name"], "my_repo");
    }

    #[test]
    fn info_response_repo_name_omitted_when_none() {
        let info = InfoResponse {
            schema_version: SCHEMA_VERSION,
            session_id: "sid".to_string(),
            provider: "test".to_string(),
            detected_format: "test".to_string(),
            title: None,
            native_name: None,
            workspace: None,
            messages: 0,
            summary: EventSummary::of_flat(&flat_session(Vec::new())),
            live_summary: EventSummary::of_flat(&flat_session(Vec::new())),
            started_at: None,
            ended_at: None,
            model_name: None,
            source_path: "/tmp/x".to_string(),
            metadata: serde_json::json!(null),
            workspace_name: None,
            workspace_name_source: Some("none".to_string()),
            repo_name: None,
            transcript_tail: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("repo_name"),
            "repo_name should be omitted from info JSON when None"
        );
    }

    #[test]
    fn error_envelope_serializes() {
        let ee = ErrorEnvelope::new("SessionNotFound", "not found".to_string());
        let json = serde_json::to_value(&ee).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error_type"], "SessionNotFound");
        assert_eq!(json["message"], "not found");
    }

    // -----------------------------------------------------------------------
    // workspace_name_from_path
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_name_from_some_path() {
        let ws = PathBuf::from("/data/projects/my_project");
        let (name, source) = workspace_name_from_path(Some(&ws));
        assert_eq!(name.as_deref(), Some("my_project"));
        assert_eq!(source.as_deref(), Some("session_workspace_path"));
    }

    #[test]
    fn workspace_name_from_none() {
        let (name, source) = workspace_name_from_path(None);
        assert!(name.is_none());
        assert_eq!(source.as_deref(), Some("none"));
    }

    #[test]
    fn workspace_name_from_root_path() {
        // Root path "/" has no file_name, so name should be None.
        let ws = PathBuf::from("/");
        let (name, source) = workspace_name_from_path(Some(&ws));
        assert!(name.is_none());
        assert_eq!(source.as_deref(), Some("none"));
    }
}
