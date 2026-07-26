//! Did the session survive the file it was written to?
//!
//! The flat track verifies itself: it reads its own output back with
//! [`crate::providers::Provider::read_session`] and compares it against the
//! [`crate::model::CanonicalSession`] it wrote. The structured track cannot
//! borrow that oracle. A structured write legitimately preserves *more* than
//! the flat projection, so the flat comparison would fail the better
//! conversion — which is why the structured track shipped with nothing checked
//! at all, and why the high-fidelity conversions were the unverified ones.
//!
//! This module is the oracle it can use: IR in, IR back off disk, compared
//! structurally.
//!
//! # Lost is not the same as deliberately not carried
//!
//! A cross-agent write is *supposed* to lose material.
//! [`crate::ir::Capsule::fits`] says in advance which sealed blobs cannot cross
//! a vendor boundary, and a write that loses exactly those and nothing else is
//! correct rather than degraded. A comparator that cannot say so would flag
//! every correct cross-agent conversion as damage, and a verifier that cries
//! wolf on the case it was built for gets switched off — which is the same as
//! not having one.
//!
//! So the report has three buckets rather than a boolean:
//!
//! - [`Comparison::predicted`] — `fits()` said it could not cross, and it did
//!   not. Correct.
//! - [`Comparison::degraded`] — the shape changed and the content did not: a
//!   `developer` message folded to `user`, a freeform tool call rewritten as
//!   JSON arguments, a structured companion with nowhere to live. Exactly the
//!   line [`Fidelity::ConversationOnly`] draws.
//! - [`Comparison::unexplained`] — content that is simply gone, with nothing
//!   predicting it. Damage, and the only bucket that fails a conversion.
//!
//! # Two-sided, on purpose
//!
//! [`Comparison::carried_foreign`] is the other half of the prediction. A
//! capsule `fits()` said could not cross and which crossed anyway is as much a
//! finding as one that vanished for no reason: the resumed session will hand an
//! Anthropic signature to OpenAI, or the reverse, and the provider will reject
//! the replayed history. An allowance that stops being exercised does not stay
//! neutral — it silently widens until it covers a real regression, which is the
//! hazard `assert_roundtrip_lossless_except` guards against on the flat side.
//! Predicting a loss and then not observing it has to be a finding too.
//!
//! # Compared over `model_visible`, not `events`
//!
//! [`crate::replay::resolve`] decides what the model is shown; compaction,
//! rollback, aborts and abandoned forks all edit history after the fact.
//! Re-deriving any of that here would be the second answer to "what does the
//! model see" that the resolver exists to remove.
//!
//! # What is compared, and what is not
//!
//! An event is reduced to its [`crate::ir::Body`] — serialised, so a body
//! variant added later is covered without touching this file. Ids, parents,
//! timestamps and turn ids are absent by construction: the target session is a
//! new session and re-mints all four.
//!
//! The comparison is *conservation*, not equality. Every source event and every
//! source capsule must be accounted for; a target that holds something extra is
//! counted in [`Comparison::added_events`] and reported, but not failed. One
//! writer is deliberately louder than its source — the `[converted by casr]`
//! marker that stands where a dropped sealed compaction used to be — and a hole
//! that announces itself is the behaviour the corpus tests demand.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::ir::{Block, Body, CapsuleFit, CapsuleKind, Event, Fidelity, Loss, LossKind, SessionIr};

// ---------------------------------------------------------------------------
// Vendors
// ---------------------------------------------------------------------------

/// The vendor whose sealed formats an agent can replay.
///
/// `None` means "this version does not know", which is emphatically not "no
/// vendor": a caller that guessed would classify every capsule as foreign and
/// turn a correct conversion into a verification failure. Skip the comparison
/// instead, and say that it was skipped.
///
/// The same two facts live as private `TARGET_VENDOR` consts inside
/// [`crate::providers::codex_ir_write`] and
/// [`crate::providers::claude_code_ir_write`], where each writer needs its own.
/// The alternative to naming them again here is a third `Provider` method whose
/// only caller would be this module.
pub fn vendor_of(agent: &str) -> Option<&'static str> {
    match agent {
        "codex" => Some("openai"),
        "claude-code" => Some("anthropic"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Sealed material that crossed a boundary [`crate::ir::Capsule::fits`] said it
/// could not.
///
/// Not a [`Loss`]: nothing was lost. [`LossKind`] is a vocabulary for things
/// that went missing and has no variant for carrying too much, so this does not
/// pretend to be one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignCarry {
    /// Whose format the blob is in — the format, not the endpoint that served
    /// it, exactly as [`CapsuleKind::vendor`] decides.
    pub kind: CapsuleKind,
    /// How many events carried one.
    pub events: usize,
    /// How many capsules crossed.
    pub capsules: usize,
    /// How many bytes of them.
    pub bytes: usize,
    pub note: String,
}

/// What a written session kept, and what it did not.
///
/// Serialisable in full: the point of a structured report is that a caller can
/// filter on [`LossKind`] and compare counts instead of parsing prose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    /// Model-visible events in the source.
    pub source_events: usize,
    /// Model-visible events in the session read back off disk.
    pub target_events: usize,
    /// Target events built entirely out of content the source never had.
    ///
    /// Reported, not failed. A writer is allowed to be louder than its source
    /// and one is: the `[converted by casr]` marker that stands where a dropped
    /// sealed compaction used to be, which the corpus tests require to be
    /// visible rather than silent. A writer that starts inventing conversation
    /// shows up here rather than nowhere.
    ///
    /// A *reshaped* event is not an added one. Counting every target event
    /// whose shape had no source counterpart would have counted the 19,400
    /// role-folds and protocol downgrades crossing the corpus, which says
    /// nothing about whether anything was invented.
    pub added_events: usize,
    /// Capsules attached to model-visible source events.
    pub source_capsules: usize,
    /// Capsules attached to model-visible target events.
    pub target_capsules: usize,
    /// Losses [`crate::ir::Capsule::fits`] predicted. Correct, not damage.
    pub predicted: Vec<Loss>,
    /// Structure the target could not reproduce, with the content still there.
    pub degraded: Vec<Loss>,
    /// Content that is gone with nothing predicting it. Damage.
    pub unexplained: Vec<Loss>,
    /// Sealed bytes `fits()` forbade that crossed anyway. Also damage.
    pub carried_foreign: Vec<ForeignCarry>,
}

impl Comparison {
    /// Nothing went missing that `fits()` did not predict, and nothing crossed
    /// that it forbade.
    pub fn is_clean(&self) -> bool {
        self.unexplained.is_empty() && self.carried_foreign.is_empty()
    }

    /// The grade this comparison's own evidence supports.
    ///
    /// Deliberately not the writer's grade. Two independently derived answers
    /// to "how much survived" are worth more than one, and the interesting case
    /// is the writer claiming better than the file can support.
    pub fn fidelity(&self) -> Fidelity {
        self.predicted
            .iter()
            .chain(&self.degraded)
            .chain(&self.unexplained)
            .fold(Fidelity::ContextComplete, |worst, loss| {
                worst.worse_of(loss.grade)
            })
    }

    /// Every damaging finding, one sentence each. Empty when
    /// [`Comparison::is_clean`].
    pub fn damage_detail(&self) -> String {
        self.unexplained
            .iter()
            .map(|loss| loss.note.clone())
            .chain(self.carried_foreign.iter().map(|carry| carry.note.clone()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Compare what was written against what should have been.
///
/// `target_vendor` is the vendor of the agent the session was written *for*,
/// from [`vendor_of`]. It is a parameter rather than something read out of
/// `target` because [`crate::ir::Origin::provider`] records the endpoint a
/// session was served by, and 109 of the 591 rollouts in the corpus were served
/// by a gateway under its own name — gating on that string is the mistake
/// [`crate::ir::Capsule::fits`] documents at length.
pub fn compare(source: &SessionIr, target: &SessionIr, target_vendor: &str) -> Comparison {
    compare_replays(
        &source.model_visible(),
        &target.model_visible(),
        target_vendor,
    )
}

/// [`compare`], for a caller that already has the two replays.
///
/// The two exist because one caller does not have a source `SessionIr` whose
/// `model_visible` is the thing it wrote: a conversion under a context budget
/// writes the *budgeted* replay, so the pipeline's verifier trims the source's
/// replay with [`crate::budget::ContextBudget::apply`] and compares that. Asking
/// it to build a trimmed `SessionIr` instead would mean re-deriving what
/// [`crate::replay::resolve`] already decided, which is the second answer to
/// "what does the model see" that this crate keeps refusing to have.
pub fn compare_replays(src: &[&Event], tgt: &[&Event], target_vendor: &str) -> Comparison {
    let mut report = Comparison {
        source_events: src.len(),
        target_events: tgt.len(),
        source_capsules: src.iter().map(|event| event.capsules.len()).sum(),
        target_capsules: tgt.iter().map(|event| event.capsules.len()).sum(),
        ..Comparison::default()
    };

    // Three multisets of what the target holds, drawn down as the source is
    // walked. What is left over at the end is what the target added.
    let mut shapes = counted(tgt.iter().map(|event| shape(event)));
    let mut texts = counted(tgt.iter().flat_map(|event| substance(event)));
    let mut sealed = counted(
        tgt.iter()
            .flat_map(|event| event.capsules.iter())
            .map(|capsule| digest(&capsule.sealed)),
    );

    // Pass one: which source events have no counterpart at all. Events that do
    // have one spend their text, so a later missing event cannot be explained
    // by content that is already accounted for.
    let mut missing = vec![false; src.len()];
    for (index, event) in src.iter().enumerate() {
        if take(&mut shapes, shape(event)) {
            for text in substance(event) {
                take(&mut texts, text);
            }
        } else {
            missing[index] = true;
        }
    }

    // Pass two: classify, one event at a time so that an event contributes at
    // most one to each bucket's event count however many capsules it lost.
    let mut tallies: Vec<(Bucket, LossKind, Tally)> = Vec::new();
    let mut carried: Vec<(CapsuleKind, Tally)> = Vec::new();

    for (index, event) in src.iter().enumerate() {
        let mut seen: Vec<(Bucket, LossKind)> = Vec::new();

        for capsule in &event.capsules {
            let crossed = take(&mut sealed, digest(&capsule.sealed));
            let bytes = capsule.sealed.len();
            match (capsule.fits(target_vendor), crossed) {
                // The ordinary same-agent case: the bytes are where they were.
                (CapsuleFit::SameVendor, true) => {}
                // The target speaks this format and the blob is gone anyway.
                (CapsuleFit::SameVendor, false) => hit(
                    &mut tallies,
                    &mut seen,
                    Bucket::Unexplained,
                    capsule_loss_kind(capsule.kind),
                    bytes,
                ),
                // Predicted, and observed. This is the crossing working.
                (CapsuleFit::ForeignVendor, false) => hit(
                    &mut tallies,
                    &mut seen,
                    Bucket::Predicted,
                    capsule_loss_kind(capsule.kind),
                    bytes,
                ),
                // Predicted and *not* observed: the writer smuggled a blob
                // across a boundary the issuing vendor is the only reader of.
                (CapsuleFit::ForeignVendor, true) => {
                    let tally = entry_kind(&mut carried, capsule.kind);
                    tally.capsules += 1;
                    tally.bytes += bytes;
                    if tally.last_event != Some(index) {
                        tally.last_event = Some(index);
                        tally.events += 1;
                    }
                }
            }
        }

        if !missing[index] {
            continue;
        }
        let content = substance(event);

        // The event existed to carry material bound to a vendor this target is
        // not, and it went with it. Its capsules are already counted above;
        // counting the disappearance again would report one loss as two.
        //
        // Reasoning and sealed context qualify whatever they hold: both are
        // documented as droppable across a vendor boundary, and the writers drop
        // the whole event rather than leave an empty husk that costs context
        // window while telling the model its own thinking was truncated. Any
        // other body qualifies only if it had no readable content of its own to
        // lose — a message whose text vanished alongside a foreign capsule has
        // lost the text too, and that is not predicted by anything.
        if !event.capsules.is_empty()
            && event
                .capsules
                .iter()
                .all(|capsule| capsule.fits(target_vendor) == CapsuleFit::ForeignVendor)
            && (content.is_empty()
                || matches!(
                    &event.body,
                    Body::Reasoning { .. } | Body::SealedContext { .. }
                ))
        {
            continue;
        }

        // The event holds nothing. A tool that printed no output leaves a result
        // whose only block is the empty string; a reasoning step that has lost
        // both its text and its blob is a husk. Neither can have deleted
        // anything the model reads, because neither contained anything. The
        // writers drop them, and there is no loss here to report.
        if content.is_empty() && event.capsules.is_empty() {
            continue;
        }

        // `filter` drives the closure for every element, so every text this
        // event needs is spent whether or not the check ends up passing.
        let kept = content
            .iter()
            .filter(|text| take(&mut texts, **text))
            .count();
        let bucket = if !content.is_empty() && kept == content.len() {
            Bucket::Degraded
        } else {
            Bucket::Unexplained
        };
        hit(
            &mut tallies,
            &mut seen,
            bucket,
            loss_kind(event, bucket == Bucket::Degraded),
            0,
        );
    }

    // Pass three: what the target says that the source never did. Whatever is
    // left in `texts` was claimed by no source event, so an event built
    // entirely out of it is the target's own invention — the
    // `[converted by casr]` marker, or a writer that has started making things
    // up. Counting leftover *shapes* instead would have counted every reshaped
    // event, which is 19,400 of them crossing the corpus and tells nobody
    // anything.
    for event in tgt {
        let content = substance(event);
        if content.is_empty() {
            continue;
        }
        let fresh = content
            .iter()
            .filter(|text| take(&mut texts, **text))
            .count();
        if fresh == content.len() {
            report.added_events += 1;
        }
    }

    tallies.sort_by_key(|(bucket, kind, _)| (*bucket as u8, format!("{kind:?}")));
    for (bucket, kind, tally) in tallies {
        let loss = render(bucket, kind, tally, target_vendor);
        match bucket {
            Bucket::Predicted => report.predicted.push(loss),
            Bucket::Degraded => report.degraded.push(loss),
            Bucket::Unexplained => report.unexplained.push(loss),
        }
    }

    carried.sort_by_key(|(kind, _)| format!("{kind:?}"));
    report.carried_foreign = carried
        .into_iter()
        .map(|(kind, tally)| ForeignCarry {
            kind,
            events: tally.events,
            capsules: tally.capsules,
            bytes: tally.bytes,
            note: format!(
                "{} {:?} capsule(s) totalling {} bytes were written into a {target_vendor} \
                 session. `Capsule::fits` says only {} can read them, so the resumed session \
                 will replay bytes its own provider must reject.",
                tally.capsules,
                kind,
                tally.bytes,
                kind.vendor(),
            ),
        })
        .collect();

    report
}

// ---------------------------------------------------------------------------
// Buckets and tallies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Predicted = 0,
    Degraded = 1,
    Unexplained = 2,
}

#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    events: usize,
    capsules: usize,
    bytes: usize,
    /// Index of the last source event counted, so one event never adds two to
    /// the same bucket.
    last_event: Option<usize>,
}

fn hit(
    tallies: &mut Vec<(Bucket, LossKind, Tally)>,
    seen: &mut Vec<(Bucket, LossKind)>,
    bucket: Bucket,
    kind: LossKind,
    bytes: usize,
) {
    let index = tallies
        .iter()
        .position(|(other, other_kind, _)| *other == bucket && *other_kind == kind)
        .unwrap_or_else(|| {
            tallies.push((bucket, kind, Tally::default()));
            tallies.len() - 1
        });
    let tally = &mut tallies[index].2;
    if bytes > 0 {
        tally.capsules += 1;
        tally.bytes += bytes;
    }
    if !seen.contains(&(bucket, kind)) {
        seen.push((bucket, kind));
        tally.events += 1;
    }
}

fn entry_kind(tallies: &mut Vec<(CapsuleKind, Tally)>, kind: CapsuleKind) -> &mut Tally {
    let index = tallies
        .iter()
        .position(|(other, _)| *other == kind)
        .unwrap_or_else(|| {
            tallies.push((kind, Tally::default()));
            tallies.len() - 1
        });
    &mut tallies[index].1
}

/// The grade one loss forces on its own.
///
/// `content_survived` separates a reshaping from a deletion, which is the whole
/// difference between [`Fidelity::ConversationOnly`] and
/// [`Fidelity::HistoryIncomplete`]: a `developer` message that arrived as a
/// `user` message still says what it said, and a message that did not arrive is
/// a hole the resumed session will not know about.
fn grade_of(kind: LossKind, content_survived: bool) -> Fidelity {
    match kind {
        LossKind::SealedContext | LossKind::Conversation => Fidelity::HistoryIncomplete,
        LossKind::Reasoning => Fidelity::ContextNoReasoning,
        _ if content_survived => Fidelity::ConversationOnly,
        _ => Fidelity::HistoryIncomplete,
    }
}

fn render(bucket: Bucket, kind: LossKind, tally: Tally, target_vendor: &str) -> Loss {
    let Tally {
        events,
        capsules,
        bytes,
        ..
    } = tally;
    let note = match bucket {
        Bucket::Predicted => format!(
            "{events} event(s) carrying {capsules} sealed {kind:?} capsule(s), {bytes} bytes, \
             could not be written into a {target_vendor} session. `Capsule::fits` predicted \
             the drop before the write and the write lost exactly them: this is the vendor \
             boundary working, not damage."
        ),
        Bucket::Degraded => format!(
            "{events} event(s) changed shape crossing into {target_vendor} — a role folded, a \
             tool protocol downgraded, or a structured companion with nowhere to live — and \
             arrived with everything the model reads still intact."
        ),
        Bucket::Unexplained => format!(
            "{events} event(s) and {capsules} capsule(s) totalling {bytes} bytes are in the \
             source and not in the written {target_vendor} session, and nothing predicted the \
             loss. That is a bug in the writer, not a property of the crossing."
        ),
    };
    Loss {
        kind,
        events,
        capsules,
        bytes,
        grade: grade_of(kind, bucket == Bucket::Degraded),
        note,
    }
}

// ---------------------------------------------------------------------------
// Reducing an event
// ---------------------------------------------------------------------------

/// Everything about an event a conversion must preserve, as one hash.
///
/// The body is serialised rather than matched on, so a [`Body`] variant added
/// later is covered without an arm here — and so that no field is silently
/// left out of the comparison by an incomplete destructuring.
///
/// Hashed rather than kept: the multiset of every tool result in a 281 MiB
/// rollout is the rollout again, and the comparison never needs to read a key
/// back — a missing event is identified by the source event that produced it.
fn shape(event: &Event) -> u64 {
    digest(&serde_json::to_string(&event.body).unwrap_or_default())
}

/// The part of an event a downgrade may reshape but must never delete.
///
/// Text, wherever the body keeps it, and tool calls by name. A `developer`
/// message folded to `user`, a freeform call rewritten as JSON arguments, a
/// thinking string that arrives in Codex's `summary` because Codex reasoning
/// has no `text` field — all three are legitimate crossings and all three
/// change the shape. What none of them is allowed to do is drop what the model
/// reads.
fn substance(event: &Event) -> Vec<u64> {
    let blocks = match &event.body {
        Body::Message { blocks, .. } => blocks,
        Body::ToolResult { output, .. } => output,
        // Arguments are the part that gets rewritten; the identity of the call
        // is the part that cannot go missing without orphaning its result.
        Body::ToolCall { name, .. } => return vec![digest(&format!("call:{name}"))],
        // The two vendors disagree on which field readable reasoning lives in,
        // so both are one pool.
        Body::Reasoning { text, summary } => {
            return text
                .iter()
                .map(String::as_str)
                .chain(summary.iter().map(String::as_str))
                .filter(|line| !line.trim().is_empty())
                .map(digest)
                .collect();
        }
        // Every other body: this version has no model of which part of it the
        // model reads, so the whole body counts as its content. Strict on
        // purpose — "I cannot tell what would have been lost" must never be
        // filed as "there was nothing to lose". `Body::Unknown` is the case that
        // makes this necessary and a variant added tomorrow is the case that
        // makes it matter.
        other => {
            return vec![digest(&serde_json::to_string(other).unwrap_or_default())];
        }
    };
    blocks
        .iter()
        .map(block_payload)
        .filter(|payload| !payload.trim().is_empty())
        .map(|payload| digest(&payload))
        .collect()
}

/// A block's content, without the wrapper that says what kind of block it is.
///
/// Text was not enough. A `tool_result` whose output is a single
/// [`Block::Unknown`] — a Claude `tool_reference`, of which the corpus has
/// plenty — has no text at all, so a comparator that only counted text saw an
/// event with nothing in it, and any reshaping of such an event looked like a
/// deletion. Images are the same story from the other direction: a base64
/// screenshot is content the model was shown, and it is the only content in the
/// event that carries it.
///
/// The kind is deliberately not part of the key. What matters is whether the
/// payload arrived, not whether the target filed it under the same block type;
/// a block that arrives reclassified shows up as a shape change, which is what
/// it is.
fn block_payload(block: &Block) -> String {
    match block {
        Block::Text { text } => text.clone(),
        Block::Image { url, .. } => url.clone(),
        Block::Document { data } => data.to_string(),
        // A redaction records that content was withheld, not any content. There
        // is nothing here that a crossing could lose.
        Block::Redacted { .. } => String::new(),
        Block::Unknown { raw, .. } => raw.to_string(),
    }
}

/// Which kind of loss the disappearance of this event is.
///
/// `content_survived` is the same distinction [`grade_of`] draws, and it has to
/// be drawn here too: a message whose text arrived intact but whose role folded
/// changed *shape*, while a message that is simply gone deleted conversation.
/// Both are `Body::Message`, and calling them the same thing was a real
/// misclassification — the first version of this filed the folded role as
/// [`LossKind::Conversation`] and `a_folded_role_is_degraded_not_lost` caught
/// it.
fn loss_kind(event: &Event, content_survived: bool) -> LossKind {
    match &event.body {
        Body::Reasoning { .. } => LossKind::Reasoning,
        Body::SealedContext { .. } => LossKind::SealedContext,
        Body::ToolCall { .. } | Body::ToolResult { .. } => LossKind::ToolProtocol,
        Body::Message { blocks, .. }
            if blocks
                .iter()
                .any(|block| matches!(block, Block::Image { .. } | Block::Document { .. })) =>
        {
            LossKind::Media
        }
        // A message the model was shown, gone. The most serious thing this
        // comparison can find, so it gets the name that says so rather than
        // hiding behind `Metadata` with the severity carried only by the grade
        // — a consumer filtering on `kind` would walk straight past it.
        Body::Message { .. } if !content_survived => LossKind::Conversation,
        // Chrome, per-turn scaffolding, and messages that only changed shape.
        _ => LossKind::Metadata,
    }
}

fn capsule_loss_kind(kind: CapsuleKind) -> LossKind {
    match kind {
        CapsuleKind::OpenaiCompactedContext => LossKind::SealedContext,
        CapsuleKind::AnthropicThinkingSignature
        | CapsuleKind::AnthropicRedactedThinking
        | CapsuleKind::OpenaiReasoningEncryptedContent => LossKind::Reasoning,
    }
}

// ---------------------------------------------------------------------------
// Multiset plumbing
// ---------------------------------------------------------------------------

fn digest(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn counted(values: impl Iterator<Item = u64>) -> HashMap<u64, usize> {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
}

/// Spend one occurrence of `key`. `false` when there was none left.
fn take(counts: &mut HashMap<u64, usize>, key: u64) -> bool {
    match counts.get_mut(&key) {
        Some(remaining) if *remaining > 0 => {
            *remaining -= 1;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Branch, Capsule, CapsuleBinding, Role, SourceRef, ToolInput, Visibility};

    fn event(id: &str, body: Body) -> Event {
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

    fn message(id: &str, role: Role, text: &str) -> Event {
        event(
            id,
            Body::Message {
                role,
                blocks: vec![Block::Text {
                    text: text.to_string(),
                }],
            },
        )
    }

    fn reasoning(id: &str, kind: CapsuleKind, sealed: &str) -> Event {
        let mut event = event(
            id,
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        event.capsules.push(Capsule {
            kind,
            bound: CapsuleBinding {
                provider: kind.vendor().to_string(),
                model: None,
            },
            sealed: sealed.to_string(),
        });
        event
    }

    fn ir(agent: &str, events: Vec<Event>) -> SessionIr {
        let mut ir = SessionIr::new(agent, "s1");
        ir.events = events;
        ir
    }

    #[test]
    fn an_identical_session_is_clean() {
        let source = ir(
            "codex",
            vec![
                message("a", Role::User, "hello"),
                reasoning("r", CapsuleKind::OpenaiReasoningEncryptedContent, "RRRR"),
                message("b", Role::Assistant, "hi"),
            ],
        );
        let report = compare(&source, &source, "openai");

        assert!(report.is_clean());
        assert_eq!(report.fidelity(), Fidelity::ContextComplete);
        assert_eq!(report.source_events, 3);
        assert_eq!(report.target_events, 3);
        assert_eq!(report.source_capsules, 1);
        assert_eq!(report.target_capsules, 1);
        assert_eq!(report.added_events, 0);
        assert!(report.predicted.is_empty(), "{:?}", report.predicted);
        assert!(report.degraded.is_empty());
    }

    #[test]
    fn a_capsule_that_cannot_cross_is_predicted_not_damage() {
        let source = ir(
            "codex",
            vec![
                message("a", Role::User, "hello"),
                reasoning("r", CapsuleKind::OpenaiReasoningEncryptedContent, "RRRR"),
            ],
        );
        // Crossing into Claude: the reasoning event has nothing left once its
        // blob is dropped, so the whole event is absent from the target.
        let target = ir("claude-code", vec![message("a", Role::User, "hello")]);

        let report = compare(&source, &target, "anthropic");

        assert!(
            report.is_clean(),
            "a predicted drop is not damage: {:?}",
            report.unexplained
        );
        assert_eq!(report.predicted.len(), 1);
        let loss = &report.predicted[0];
        assert_eq!(loss.kind, LossKind::Reasoning);
        assert_eq!(loss.events, 1);
        assert_eq!(loss.grade, Fidelity::ContextNoReasoning);
        assert_eq!(report.fidelity(), Fidelity::ContextNoReasoning);
        assert!(loss.note.contains("4 bytes"), "{}", loss.note);
    }

    #[test]
    fn a_dropped_sealed_context_grades_worse_than_dropped_reasoning() {
        let mut sealed = event(
            "cmp",
            Body::SealedContext {
                native_id: Some("cmp_1".into()),
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
        let source = ir("codex", vec![message("a", Role::User, "hi"), sealed]);
        let target = ir("claude-code", vec![message("a", Role::User, "hi")]);

        let report = compare(&source, &target, "anthropic");

        assert!(report.is_clean());
        assert_eq!(report.predicted.len(), 1);
        assert_eq!(report.predicted[0].kind, LossKind::SealedContext);
        assert_eq!(report.fidelity(), Fidelity::HistoryIncomplete);
    }

    #[test]
    fn a_missing_message_is_damage_and_nothing_predicted_it() {
        let source = ir(
            "codex",
            vec![
                message("a", Role::User, "kept"),
                message("b", Role::Assistant, "deleted"),
            ],
        );
        let target = ir("codex", vec![message("a", Role::User, "kept")]);

        let report = compare(&source, &target, "openai");

        assert!(!report.is_clean());
        assert_eq!(report.unexplained.len(), 1);
        assert_eq!(report.unexplained[0].events, 1);
        assert_eq!(
            report.unexplained[0].grade,
            Fidelity::HistoryIncomplete,
            "a message that did not arrive is a hole, not a downgrade"
        );
        assert!(report.damage_detail().contains("nothing predicted"));
    }

    #[test]
    fn a_folded_role_is_degraded_not_lost() {
        // Claude Code has only `user` and `assistant`; Codex's `developer`
        // arrives as `user` with its text intact.
        let source = ir("codex", vec![message("d", Role::Developer, "instruction")]);
        let target = ir("claude-code", vec![message("d", Role::User, "instruction")]);

        let report = compare(&source, &target, "anthropic");

        assert!(report.is_clean());
        assert_eq!(report.degraded.len(), 1);
        assert_eq!(report.degraded[0].kind, LossKind::Metadata);
        assert_eq!(report.degraded[0].grade, Fidelity::ConversationOnly);
        assert_eq!(report.fidelity(), Fidelity::ConversationOnly);
    }

    #[test]
    fn a_downgraded_tool_protocol_is_degraded_not_lost() {
        let call = |input: ToolInput| {
            event(
                "c",
                Body::ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    namespace: None,
                    input,
                },
            )
        };
        let source = ir(
            "codex",
            vec![call(ToolInput::Freeform {
                text: "ls -la".into(),
            })],
        );
        let target = ir(
            "claude-code",
            vec![call(ToolInput::Json {
                value: serde_json::json!({"command": "ls -la"}),
                original: None,
            })],
        );

        let report = compare(&source, &target, "anthropic");

        assert!(report.is_clean());
        assert_eq!(report.degraded.len(), 1);
        assert_eq!(report.degraded[0].kind, LossKind::ToolProtocol);
    }

    /// The other half of the prediction, and the reason it has to be two-sided.
    #[test]
    fn a_foreign_capsule_that_survived_is_a_finding() {
        let source = ir(
            "codex",
            vec![reasoning(
                "r",
                CapsuleKind::OpenaiReasoningEncryptedContent,
                "RRRR",
            )],
        );
        // The writer wrote the OpenAI blob into a Claude transcript anyway. The
        // reader tags it by the field it came out of, so nothing but the bytes
        // identifies it — which is why the check is on the bytes.
        let mut smuggled = event(
            "r",
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        smuggled.capsules.push(Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".into(),
                model: None,
            },
            sealed: "RRRR".into(),
        });
        let target = ir("claude-code", vec![smuggled]);

        let report = compare(&source, &target, "anthropic");

        assert!(
            !report.is_clean(),
            "an allowance that stops being exercised silently widens"
        );
        assert_eq!(report.carried_foreign.len(), 1);
        assert_eq!(report.carried_foreign[0].capsules, 1);
        assert_eq!(report.carried_foreign[0].bytes, 4);
        assert!(report.predicted.is_empty(), "nothing was actually dropped");
        assert!(report.damage_detail().contains("must reject"));
    }

    #[test]
    fn a_same_vendor_capsule_that_vanished_is_damage() {
        let source = ir(
            "codex",
            vec![reasoning(
                "r",
                CapsuleKind::OpenaiReasoningEncryptedContent,
                "RRRR",
            )],
        );
        let target = ir(
            "codex",
            vec![event(
                "r",
                Body::Reasoning {
                    text: None,
                    summary: Vec::new(),
                },
            )],
        );

        let report = compare(&source, &target, "openai");

        assert!(!report.is_clean());
        assert_eq!(report.unexplained.len(), 1);
        assert_eq!(report.unexplained[0].kind, LossKind::Reasoning);
        assert!(report.predicted.is_empty());
    }

    #[test]
    fn an_added_marker_is_counted_and_not_failed() {
        let source = ir("codex", vec![message("a", Role::User, "hi")]);
        let target = ir(
            "claude-code",
            vec![
                message("a", Role::User, "hi"),
                message("m", Role::Assistant, "[converted by casr] history was sealed"),
            ],
        );

        let report = compare(&source, &target, "anthropic");

        assert!(report.is_clean(), "a louder target is not a lossy one");
        assert_eq!(report.added_events, 1);
    }

    #[test]
    fn the_comparison_runs_over_the_replay_not_the_capture() {
        // A compacted-away message is not in `model_visible`, so a target that
        // does not carry it has lost nothing.
        let mut source = ir(
            "codex",
            vec![
                message("old", Role::User, "superseded"),
                message("sum", Role::User, "summary"),
            ],
        );
        source.events.push(event(
            "c",
            Body::Compaction {
                context: vec!["sum".into()],
                supersedes: vec!["old".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));
        let target = ir("codex", vec![message("sum", Role::User, "summary")]);

        let report = compare(&source, &target, "openai");

        assert_eq!(report.source_events, 1, "the marker is not content");
        assert!(report.is_clean());
    }

    #[test]
    fn vendor_of_knows_the_two_structured_agents_and_admits_the_rest() {
        assert_eq!(vendor_of("codex"), Some("openai"));
        assert_eq!(vendor_of("claude-code"), Some("anthropic"));
        assert_eq!(vendor_of("gemini"), None);
    }
}
