//! Context budget for the structured track.
//!
//! The flat track budgets a [`crate::model::CanonicalSession`] in the pipeline,
//! before the writer ever sees it (`pipeline::apply_context_budget`). The
//! structured track cannot borrow that: the thing it writes is the IR, and by
//! the time a writer is called there is no canonical projection left to trim.
//! So the budget lives here, is applied by both structured writers, and is
//! applied to the same list the writers emit — [`SessionIr::model_visible`].
//!
//! # Over the replay, never over `events`
//!
//! [`crate::replay::resolve`] has already applied compaction, rollback, aborts
//! and fork pruning. Trimming [`crate::ir::SessionIr::events`] instead would
//! count — and "save" — history the agent itself had already superseded: on the
//! local corpus that is 492,429 captured Codex model events against 94,478
//! resolved ones, so a budget measured against the capture would be measuring
//! something no model will ever be shown. A cap enforced against the wrong
//! total is not a cap.
//!
//! # Trimming is a loss, and it is reported as one
//!
//! Everything this module removes comes back as a [`Loss`] with real counts, and
//! each writer folds those into the same list its own vendor-boundary losses go
//! into. The reported grade is derived from that list and from nothing else —
//! see `codex_ir_write::Writer::summarise` for the bug that rule exists to
//! prevent. There is deliberately no grade accumulator here.
//!
//! One consequence is worth stating because it is visible to users: a dropped
//! message is [`LossKind::Conversation`], which grades
//! [`Fidelity::HistoryIncomplete`], and the launcher refuses to start an agent
//! on a `HistoryIncomplete` session without `--launch-anyway`. That is the
//! honest outcome. A budget that removed conversation and then reported
//! "conversation preserved" would be the flat track's silence with extra steps.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ir::{Block, Body, Event, Fidelity, Loss, LossKind};

/// How much context the target session may carry.
///
/// The three CLI flags, in one value, so that both writers take one parameter
/// rather than three and adding a fourth knob does not re-break two signatures.
/// Field names and units match `pipeline::ConvertOptions`, which is where they
/// come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Rough cap on the whole replay, in tokens. `0` means unlimited.
    pub max_context_tokens: usize,
    /// Cap on one tool observation, in characters. `0` means unlimited.
    pub max_tool_output: usize,
    /// Keep reasoning that the target could actually replay.
    pub keep_reasoning: bool,
}

impl ContextBudget {
    /// No budget at all: every event, whole, in order.
    ///
    /// The value the writers must be handed when no flag was given, and the
    /// reason [`ContextBudget::apply`] short-circuits on it: the codex→codex
    /// round trip is verified event-for-event, and "the budget was off" has to
    /// mean the bytes are the ones the writer would have produced before this
    /// module existed, not merely equivalent ones.
    pub const UNLIMITED: ContextBudget = ContextBudget {
        max_context_tokens: 0,
        max_tool_output: 0,
        keep_reasoning: true,
    };

    /// Whether this budget can remove anything at all.
    pub fn is_unlimited(&self) -> bool {
        *self == Self::UNLIMITED
    }

    /// Fit `visible` into this budget, reporting everything removed.
    ///
    /// `visible` is [`SessionIr::model_visible`]'s output, oldest first, and the
    /// result keeps that order.
    ///
    /// # The order of operations, and why it is this one
    ///
    /// 1. **Reasoning**, when `keep_reasoning` is false. Removed outright, and
    ///    first, so that what it frees is available to the conversation rather
    ///    than to more reasoning.
    /// 2. **Tool observations**, truncated to `max_tool_output`. Before the
    ///    token cap, not after: tool output is the dominant byte source in a
    ///    real session, and eliding the middle of a few observations routinely
    ///    removes the need to drop any turn at all. Truncation keeps the event,
    ///    its `call_id` and its outcome; dropping a turn does not.
    /// 3. **The token cap**, by dropping from the oldest end.
    /// 4. **Pairing repair**, because step 3 can cut between a tool call and its
    ///    result.
    ///
    /// # Precedence: `--keep-reasoning` against the cap
    ///
    /// The cap wins. `max_context_tokens` describes something physical — a
    /// context window the resumed session has to fit inside — while
    /// `keep_reasoning` is a preference about *what to sacrifice first*. So
    /// `keep_reasoning` is not an exemption from the cap: it says only that
    /// reasoning must not be sacrificed *ahead of* conversation. Reasoning that
    /// belongs to a turn the cap removes goes with that turn, and is reported
    /// as a reasoning loss when it does.
    ///
    /// This is the interaction that matters, because the two flags pull in
    /// opposite directions on purpose. Reasoning capsules are the largest and
    /// least human-legible part of a session, which is why the cross-agent
    /// default drops them — but they are also exactly what makes a same-vendor
    /// resume high-fidelity, and a same-vendor writer keeps them. Letting
    /// `keep_reasoning` veto the cap would let a flag produce a session the
    /// target cannot load; letting the cap ignore `keep_reasoning` would throw
    /// away the one thing the user asked to protect while cheaper material
    /// survived.
    ///
    /// # Oldest end first
    ///
    /// A resumable session needs its most recent turns: the tail is what the
    /// next prompt continues from, and the head is what a compaction would have
    /// summarised away anyway. So the kept set is a suffix, and the newest event
    /// is kept even when it alone exceeds the cap — a budget that returns
    /// nothing has not budgeted a session, it has deleted one.
    ///
    /// Note the flat track pins its first message and keeps a suffix after it.
    /// That is deliberately *not* copied here. Its first message is the user's
    /// task; the first model-visible event of a structured session is as often a
    /// `TurnConfig`, an `EnvSnapshot` or a harness preamble, and pinning one
    /// event across a hole asserts a continuity with what follows that no longer
    /// exists.
    pub fn apply<'a>(&self, visible: Vec<&'a Event>) -> Budgeted<'a> {
        // Byte-identical when no flag was given: same events, same order, not a
        // single clone.
        if self.is_unlimited() {
            return Budgeted {
                events: visible.into_iter().map(Cow::Borrowed).collect(),
                losses: Vec::new(),
            };
        }

        let mut events: Vec<Cow<'a, Event>> = visible.into_iter().map(Cow::Borrowed).collect();
        let mut keep = vec![true; events.len()];

        if !self.keep_reasoning {
            for (index, event) in events.iter().enumerate() {
                if matches!(event.body, Body::Reasoning { .. }) {
                    keep[index] = false;
                }
            }
        }

        let mut truncated = Tally::default();
        if self.max_tool_output > 0 {
            for (index, event) in events.iter_mut().enumerate() {
                if keep[index] {
                    truncate_tool_output(event, self.max_tool_output, &mut truncated);
                }
            }
        }

        if self.max_context_tokens > 0 {
            self.drop_oldest(&events, &mut keep);
        }
        repair_pairing(&events, &mut keep);

        let mut dropped = Dropped::default();
        for (index, event) in events.iter().enumerate() {
            if !keep[index] {
                dropped.record(event);
            }
        }

        let mut retained = Vec::with_capacity(keep.iter().filter(|kept| **kept).count());
        for (index, event) in events.into_iter().enumerate() {
            if keep[index] {
                retained.push(event);
            }
        }

        Budgeted {
            events: retained,
            losses: dropped.losses(truncated, self.max_tool_output),
        }
    }

    /// Clear `keep` for the oldest events until the rest fits the token cap.
    ///
    /// Costs are computed once, over the events as they stand *after*
    /// truncation, so an elided observation is charged what it now costs rather
    /// than what it used to.
    fn drop_oldest(&self, events: &[Cow<'_, Event>], keep: &mut [bool]) {
        let live: Vec<usize> = (0..events.len()).filter(|index| keep[*index]).collect();
        let costs: Vec<usize> = live.iter().map(|index| tokens(&events[*index])).collect();
        if costs.iter().sum::<usize>() <= self.max_context_tokens {
            return;
        }
        // Walk back from the newest, keeping what fits. `first` starts at the
        // newest so that an event too large for the whole cap still survives on
        // its own: see "Oldest end first".
        let mut spent = 0usize;
        let mut first = live.len().saturating_sub(1);
        for position in (0..live.len()).rev() {
            if spent + costs[position] > self.max_context_tokens && position != live.len() - 1 {
                break;
            }
            spent += costs[position];
            first = position;
        }
        for position in 0..first {
            keep[live[position]] = false;
        }
    }
}

/// One replay, fitted to a budget.
pub struct Budgeted<'a> {
    /// The events to write, oldest first. Borrowed unless the budget had to
    /// rewrite one, which only truncation does.
    pub events: Vec<Cow<'a, Event>>,
    /// What the fitting removed. Merged by each writer into the list its grade
    /// is folded from.
    pub losses: Vec<Loss>,
}

impl Budgeted<'_> {
    /// The events as plain references, for a consumer that only reads them.
    pub fn as_events(&self) -> Vec<&Event> {
        self.events.iter().map(|event| &**event).collect()
    }
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// Elide the middle of every oversized text block in a tool result.
///
/// Text only. An image or document block in a tool result is content the model
/// was *shown* rather than an observation it can re-read a fragment of, and half
/// a base64 payload is not a smaller image — it is a corrupt one. The flat
/// track's `max_tool_output` truncates its text field and nothing else, so this
/// matches it rather than inventing a second rule.
fn truncate_tool_output(event: &mut Cow<'_, Event>, max: usize, tally: &mut Tally) {
    let Body::ToolResult { output, .. } = &event.body else {
        return;
    };
    if !output
        .iter()
        .any(|block| matches!(block, Block::Text { text } if text.chars().count() > max))
    {
        return;
    }
    // First mutation of this event: `to_mut` clones out of the IR, and only
    // here. Everything the budget keeps whole stays borrowed.
    let Body::ToolResult { output, .. } = &mut event.to_mut().body else {
        return;
    };
    let mut removed = 0usize;
    for block in output.iter_mut() {
        if let Block::Text { text } = block
            && let Some(short) = crate::pipeline::elide_middle(text, max)
        {
            removed += text.len().saturating_sub(short.len());
            *text = short;
        }
    }
    tally.events += 1;
    tally.bytes += removed;
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

/// Drop the other half of every tool call/result pair the budget broke.
///
/// A `tool_use` with no `tool_result` — or the reverse — is not a smaller
/// session, it is an invalid one: the Anthropic API rejects the unmatched
/// block outright, and Codex pairs its `function_call_output` back to a call
/// that is no longer there. So a pair is all-or-nothing.
///
/// Only pairs are touched. A call the *source* left unanswered — the transcript
/// ended inside a tool loop, which the corpus has — is left exactly as it was
/// found: repairing a hole the budget did not make would be this function
/// quietly editing the conversation on its own initiative.
///
/// # A pair is one call and one result, not a bag of records sharing a string
///
/// Calls and results are collected apart and matched one to one, rather than
/// dropped into a single bucket per `call_id` and treated as a pair whenever the
/// bucket holds more than one member. A `call_id` is a string that arrives from
/// a file; nothing in the IR makes it unique, and an agent that recorded none
/// leaves it empty. Two *unanswered* calls both carrying `""` formed a bucket of
/// two, so the cap dropping the older dropped the newer with it — which returned
/// an empty replay, reported a broken pair that never existed, and deleted the
/// newest event the cap had explicitly retained. An empty id identifies nothing
/// and is therefore left alone: a record that cannot say which call it belongs
/// to must not be used to remove one that can.
fn repair_pairing(events: &[Cow<'_, Event>], keep: &mut [bool]) {
    let mut calls: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut results: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        match &event.body {
            Body::ToolCall { call_id, .. } if !call_id.is_empty() => {
                calls.entry(call_id).or_default().push(index);
            }
            Body::ToolResult { call_id, .. } if !call_id.is_empty() => {
                results.entry(call_id).or_default().push(index);
            }
            // The unidentifiable halves, and everything that is not tool
            // traffic. NO WILDCARD ARM over [`Body`].
            Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::Compaction { .. }
            | Body::SealedContext { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => {}
        }
    }
    for (call_id, call_indices) in &calls {
        let Some(result_indices) = results.get(call_id) else {
            continue;
        };
        // In document order, so an id an agent reused across turns pairs each
        // call with the result that followed it. A leftover call or result on
        // either end is an imbalance the source already had.
        for (call, result) in call_indices.iter().zip(result_indices) {
            if !keep[*call] || !keep[*result] {
                keep[*call] = false;
                keep[*result] = false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

/// Rough token cost of one event, at ~4 characters per token.
///
/// The same crude ratio the flat track's `estimate_message_tokens` uses, so the
/// two tracks answer "is this session over budget" the same way; a cap is a
/// coarse instrument and a tokenizer per vendor would be a precise answer to a
/// question nobody asked.
///
/// Measured over the serialised body rather than over a match on it, which is
/// what `compare::shape` does and for the same reason: a [`Body`] variant added
/// later is charged for automatically, where an arm-per-variant estimator would
/// charge the new one nothing and quietly let it in over the cap. Capsule bytes
/// are added because sealed reasoning occupies the target's context window like
/// anything else.
fn tokens(event: &Event) -> usize {
    let body = serde_json::to_string(&event.body).map_or(0, |json| json.len());
    let sealed: usize = event
        .capsules
        .iter()
        .map(|capsule| capsule.sealed.len())
        .sum();
    (body + sealed) / 4 + 1
}

// ---------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    events: usize,
    capsules: usize,
    bytes: usize,
}

impl Tally {
    fn add(&mut self, event: &Event) {
        self.events += 1;
        self.capsules += event.capsules.len();
        self.bytes += event
            .capsules
            .iter()
            .map(|capsule| capsule.sealed.len())
            .sum::<usize>();
    }
}

/// Everything the budget removed, bucketed by the kind of loss it is.
#[derive(Debug, Default)]
struct Dropped {
    conversation: Tally,
    reasoning: Tally,
    tool: Tally,
    sealed: Tally,
    metadata: Tally,
    /// Which tool calls lost their result, or the reverse, so the note can say
    /// so — the pairing repair is the one removal a user did not ask for.
    broken_pairs: HashSet<String>,
}

impl Dropped {
    /// File one removed event under the kind of loss its disappearance is.
    ///
    /// Exhaustive, with no `_` arm: a new [`Body`] variant must be classified
    /// here rather than silently inheriting whichever bucket happened to be
    /// last. The mapping follows `compare::loss_kind`, so the writer's grade and
    /// the comparator's independently derived one agree about what a missing
    /// event of each kind costs.
    fn record(&mut self, event: &Event) {
        match &event.body {
            // Conversation the model was shown, gone. The most serious thing a
            // budget can do, and it gets the name that says so rather than
            // hiding under `Metadata` with the severity carried only by the
            // grade — the launch refusal filters on `kind`.
            Body::Message { .. } => self.conversation.add(event),
            Body::Reasoning { .. } => self.reasoning.add(event),
            Body::ToolCall { call_id, .. } | Body::ToolResult { call_id, .. } => {
                self.broken_pairs.insert(call_id.clone());
                self.tool.add(event);
            }
            // Compacted history. Not "metadata about" the conversation — for
            // three quarters of real Codex rollouts it *is* the earlier
            // conversation, which is why it ranks worst of all.
            Body::SealedContext { .. } => self.sealed.add(event),
            // Per-turn scaffolding and chrome that reached the replay. Cheap,
            // but not free: the model was shown it.
            Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Compaction { .. }
            | Body::Rollback { .. }
            | Body::Abort { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => self.metadata.add(event),
        }
    }

    fn losses(&self, truncated: Tally, max_tool_output: usize) -> Vec<Loss> {
        let mut losses = Vec::new();
        let mut push = |kind, tally: Tally, grade, note| {
            if tally.events > 0 {
                losses.push(Loss {
                    kind,
                    events: tally.events,
                    capsules: tally.capsules,
                    bytes: tally.bytes,
                    grade,
                    note,
                });
            }
        };
        push(
            LossKind::SealedContext,
            self.sealed,
            Fidelity::HistoryIncomplete,
            format!(
                "The context budget dropped {} compacted-history event(s) carrying {} sealed \
                 bytes. That is the earlier conversation itself: the resumed session is missing \
                 history and will not know it.",
                self.sealed.events, self.sealed.bytes,
            ),
        );
        push(
            LossKind::Conversation,
            self.conversation,
            Fidelity::HistoryIncomplete,
            format!(
                "The context budget dropped the {} oldest message(s) to fit the transferred \
                 history inside the cap. The most recent turns were kept; the resumed session is \
                 missing the earlier ones.",
                self.conversation.events,
            ),
        );
        push(
            LossKind::ToolProtocol,
            self.tool,
            Fidelity::HistoryIncomplete,
            format!(
                "The context budget dropped {} tool call/result event(s), covering {} tool \
                 call(s) whose call and result had to go together to stay replayable.",
                self.tool.events,
                self.broken_pairs.len(),
            ),
        );
        push(
            LossKind::Reasoning,
            self.reasoning,
            Fidelity::ContextNoReasoning,
            format!(
                "The context budget dropped {} reasoning event(s) totalling {} sealed bytes. Pass \
                 --keep-reasoning to spend the budget on them instead of on older turns.",
                self.reasoning.events, self.reasoning.bytes,
            ),
        );
        push(
            LossKind::Metadata,
            self.metadata,
            Fidelity::HistoryIncomplete,
            format!(
                "The context budget dropped {} model-visible scaffolding event(s) — per-turn \
                 configuration, environment snapshots, attachments — along with the turns they \
                 belonged to.",
                self.metadata.events,
            ),
        );
        push(
            LossKind::ToolProtocol,
            truncated,
            Fidelity::ConversationOnly,
            format!(
                "The context budget elided the middle of {} tool observation(s), {} bytes in \
                 total, to ~{max_tool_output} characters each. Every result kept its event, its \
                 call_id and its outcome, and each elision says so in place.",
                truncated.events, truncated.bytes,
            ),
        );
        losses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Branch, Capsule, CapsuleBinding, CapsuleKind, Role, SessionIr, SourceRef, ToolInput,
        ToolOutcome, Visibility,
    };

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

    fn message(id: &str, text: &str) -> Event {
        event(
            id,
            Body::Message {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: text.to_string(),
                }],
            },
        )
    }

    fn reasoning(id: &str, sealed: &str) -> Event {
        let mut event = event(
            id,
            Body::Reasoning {
                text: None,
                summary: Vec::new(),
            },
        );
        event.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiReasoningEncryptedContent,
            bound: CapsuleBinding {
                provider: "openai".to_string(),
                model: None,
            },
            sealed: sealed.to_string(),
        });
        event
    }

    fn call(id: &str, call_id: &str) -> Event {
        event(
            id,
            Body::ToolCall {
                call_id: call_id.to_string(),
                name: "shell".to_string(),
                namespace: None,
                input: ToolInput::Freeform {
                    text: "ls".to_string(),
                },
            },
        )
    }

    fn result(id: &str, call_id: &str, text: &str) -> Event {
        event(
            id,
            Body::ToolResult {
                call_id: call_id.to_string(),
                outcome: ToolOutcome::Unknown,
                output: vec![Block::Text {
                    text: text.to_string(),
                }],
                structured: None,
            },
        )
    }

    fn ir(events: Vec<Event>) -> SessionIr {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events = events;
        ir
    }

    fn ids(out: &Budgeted<'_>) -> Vec<String> {
        out.events.iter().map(|event| event.id.clone()).collect()
    }

    #[test]
    fn no_flags_borrows_every_event_untouched() {
        let source = ir(vec![
            message("a", "one"),
            reasoning("r", "SEALED"),
            message("b", "two"),
        ]);
        let out = ContextBudget::UNLIMITED.apply(source.model_visible());

        assert_eq!(ids(&out), ["a", "r", "b"]);
        assert!(out.losses.is_empty(), "an absent budget loses nothing");
        assert!(
            out.events
                .iter()
                .all(|event| matches!(event, Cow::Borrowed(_))),
            "an absent budget must not even clone: the codex→codex round trip is \
             verified event-for-event"
        );
    }

    #[test]
    fn the_cap_drops_the_oldest_and_keeps_the_tail() {
        let source = ir(vec![
            message("oldest", &"x".repeat(400)),
            message("middle", &"y".repeat(400)),
            message("newest", &"z".repeat(400)),
        ]);
        // Each message costs ~100 tokens, so a 250-token cap fits two.
        let out = ContextBudget {
            max_context_tokens: 250,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(
            ids(&out),
            ["middle", "newest"],
            "a resumable session needs its most recent turns"
        );
        let loss = out
            .losses
            .iter()
            .find(|loss| loss.kind == LossKind::Conversation)
            .expect("a dropped message is a reported loss");
        assert_eq!(loss.events, 1);
        assert_eq!(loss.grade, Fidelity::HistoryIncomplete);
    }

    #[test]
    fn the_newest_event_survives_a_cap_it_cannot_fit() {
        let source = ir(vec![message("a", "hi"), message("b", &"z".repeat(4000))]);
        let out = ContextBudget {
            max_context_tokens: 1,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(
            ids(&out),
            ["b"],
            "a budget that returns nothing has deleted a session, not budgeted one"
        );
    }

    #[test]
    fn dropping_a_tool_call_takes_its_result_with_it() {
        let source = ir(vec![
            call("c1", "call-1"),
            result("r1", "call-1", "output"),
            message("m", &"z".repeat(400)),
        ]);
        // Only the last message fits, which cuts between the call and its result.
        let out = ContextBudget {
            max_context_tokens: 110,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(
            ids(&out),
            ["m"],
            "an orphaned tool_result is rejected at replay, so the pair goes together"
        );
        let loss = out
            .losses
            .iter()
            .find(|loss| loss.kind == LossKind::ToolProtocol)
            .expect("the dropped pair is reported");
        assert_eq!(loss.events, 2);
    }

    #[test]
    fn an_unanswered_call_the_source_left_is_not_repaired_away() {
        let source = ir(vec![message("m", "hi"), call("c1", "call-1")]);
        let out = ContextBudget {
            max_context_tokens: 1_000,
            max_tool_output: 10,
            keep_reasoning: false,
        }
        .apply(source.model_visible());

        assert_eq!(
            ids(&out),
            ["m", "c1"],
            "the transcript ended inside a tool loop; the budget did not break that"
        );
        assert!(out.losses.is_empty());
    }

    #[test]
    fn keep_reasoning_off_drops_reasoning_and_says_how_much() {
        let source = ir(vec![message("a", "hi"), reasoning("r", "SEALEDBYTES")]);
        let out = ContextBudget {
            max_context_tokens: 0,
            max_tool_output: 0,
            keep_reasoning: false,
        }
        .apply(source.model_visible());

        assert_eq!(ids(&out), ["a"]);
        assert_eq!(out.losses.len(), 1);
        assert_eq!(out.losses[0].kind, LossKind::Reasoning);
        assert_eq!(out.losses[0].capsules, 1);
        assert_eq!(out.losses[0].bytes, "SEALEDBYTES".len());
        assert_eq!(out.losses[0].grade, Fidelity::ContextNoReasoning);
    }

    /// The precedence the doc comment states: the cap is physical, the flag is a
    /// preference about what to give up first.
    #[test]
    fn keep_reasoning_does_not_exempt_reasoning_from_the_cap() {
        let source = ir(vec![
            reasoning("r", &"S".repeat(400)),
            message("newest", "hi"),
        ]);
        let out = ContextBudget {
            max_context_tokens: 20,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(ids(&out), ["newest"]);
        assert_eq!(
            out.losses[0].kind,
            LossKind::Reasoning,
            "reasoning lost with the turn it belonged to is still a reasoning loss"
        );
    }

    #[test]
    fn keep_reasoning_on_spends_the_budget_on_reasoning_rather_than_on_turns() {
        let events = vec![
            message("oldest", &"x".repeat(200)),
            reasoning("r", &"S".repeat(200)),
            message("newest", &"z".repeat(200)),
        ];
        // Measured with `tokens`: each message costs 68, the reasoning event 62.
        // 150 fits reasoning + the newest turn, or the two turns without it.
        let cap = 150;
        let kept_with = ids(&ContextBudget {
            max_context_tokens: cap,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(ir(events.clone()).model_visible()));
        let kept_without = ids(&ContextBudget {
            max_context_tokens: cap,
            max_tool_output: 0,
            keep_reasoning: false,
        }
        .apply(ir(events).model_visible()));

        assert_eq!(kept_with, ["r", "newest"], "the flag protects reasoning");
        assert_eq!(
            kept_without,
            ["oldest", "newest"],
            "without it, the same budget buys an older turn instead"
        );
    }

    #[test]
    fn an_oversized_tool_result_is_elided_not_dropped() {
        let source = ir(vec![result("r1", "call-1", &"z".repeat(5_000))]);
        let out = ContextBudget {
            max_context_tokens: 0,
            max_tool_output: 100,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(
            ids(&out),
            ["r1"],
            "the event, its call_id and its outcome stay"
        );
        let Body::ToolResult { output, .. } = &out.events[0].body else {
            panic!("still a tool result");
        };
        let Block::Text { text } = &output[0] else {
            panic!("still text");
        };
        assert!(
            text.contains("elided"),
            "the elision announces itself in place"
        );
        assert!(text.chars().count() < 200);
        assert_eq!(out.losses.len(), 1);
        assert_eq!(out.losses[0].kind, LossKind::ToolProtocol);
        assert_eq!(out.losses[0].grade, Fidelity::ConversationOnly);
        assert!(out.losses[0].bytes > 4_000, "{:?}", out.losses[0]);
    }

    /// Truncation runs before the cap, so eliding observations can remove the
    /// need to drop a turn at all.
    #[test]
    fn truncation_can_save_a_turn_the_cap_would_have_dropped() {
        let events = vec![
            message("oldest", "keep me"),
            result("r1", "call-1", &"z".repeat(4_000)),
        ];
        let cap = 300;
        let with_truncation = ids(&ContextBudget {
            max_context_tokens: cap,
            max_tool_output: 200,
            keep_reasoning: true,
        }
        .apply(ir(events.clone()).model_visible()));
        let without = ids(&ContextBudget {
            max_context_tokens: cap,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(ir(events).model_visible()));

        assert_eq!(with_truncation, ["oldest", "r1"]);
        assert_eq!(without, ["r1"]);
    }

    /// F7. Two calls the source never answered, both recorded with no `call_id`
    /// — an id is a string and nothing makes it unique. Grouping on it alone
    /// fuses them into one "pair", so removing the older removes the newer with
    /// it, and the newest event the cap explicitly retains disappears.
    #[test]
    fn an_empty_call_id_does_not_fuse_two_unrelated_calls() {
        let source = ir(vec![
            call("old", ""),
            message("mid", &"x".repeat(400)),
            call("newest", ""),
        ]);
        let out = ContextBudget {
            max_context_tokens: 40,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert!(
            ids(&out).contains(&"newest".to_string()),
            "the newest event is kept even when it alone exceeds the cap; pairing repair may \
             not delete it: {:?}",
            ids(&out)
        );
    }

    /// F7, the other side: a real pair still goes together.
    #[test]
    fn a_shared_real_call_id_still_pairs_one_to_one() {
        let source = ir(vec![
            call("c1", "call-1"),
            result("r1", "call-1", "output"),
            message("m", &"z".repeat(400)),
        ]);
        let out = ContextBudget {
            max_context_tokens: 110,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(ids(&out), ["m"]);
    }

    #[test]
    fn a_dropped_sealed_compaction_outranks_a_dropped_message() {
        let mut sealed = event(
            "cmp",
            Body::SealedContext {
                native_id: None,
                meta: serde_json::Value::Null,
            },
        );
        sealed.capsules.push(Capsule {
            kind: CapsuleKind::OpenaiCompactedContext,
            bound: CapsuleBinding {
                provider: "openai".to_string(),
                model: None,
            },
            sealed: "C".repeat(400),
        });
        let source = ir(vec![sealed, message("newest", "hi")]);
        let out = ContextBudget {
            max_context_tokens: 20,
            max_tool_output: 0,
            keep_reasoning: true,
        }
        .apply(source.model_visible());

        assert_eq!(ids(&out), ["newest"]);
        assert_eq!(out.losses.len(), 1);
        assert_eq!(out.losses[0].kind, LossKind::SealedContext);
        assert_eq!(out.losses[0].capsules, 1);
        assert_eq!(out.losses[0].grade, Fidelity::HistoryIncomplete);
    }
}
