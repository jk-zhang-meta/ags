//! What the target model should actually be shown.
//!
//! [`SessionIr::events`] is the capture, in file order. It is not the model's
//! context, because four independent mechanisms edit history after the fact:
//!
//! - **Compaction.** The agent rewrote its own model-visible history. Roughly
//!   three quarters of real Codex rollouts do this at least once.
//! - **Rollback.** [`Body::Rollback`] removes the last N typed turns; 714 of
//!   them across the corpus, all from Codex `thread_rolled_back`.
//! - **Abort.** [`Body::Abort`] interrupts a turn, but the partial output
//!   *stays* in context. Of the 2,304 aborts in the corpus, 1,587 had no
//!   following rollback and 286 of those had already produced real output, so
//!   treating an abort as a removal deletes work the model saw.
//! - **Forks.** Claude's DAG branches when a user edits or retries a message,
//!   and the abandoned branches are still in the file.
//!
//! Replaying the flat event list ignores all four. This module folds them into
//! one answer, and [`SessionIr::model_visible`] is a view over it rather than a
//! second implementation.
//!
//! # This module knows about no provider
//!
//! Every rule below reads a typed [`Body`] variant or [`SessionIr::live_head`].
//! There is no agent check, and no provider's wire vocabulary — no
//! `"thread_rolled_back"`, no `"last-prompt"`, no `data["num_turns"]` — appears
//! anywhere in this file. Each reader already understands its own format and
//! emits the typed form from there, so a third provider gets correct rollback,
//! abort and fork handling without this file learning anything new. When the
//! resolver instead matched wire strings, a provider it had not been taught
//! about got none of it and nothing said so: its control events fell through a
//! wildcard arm and became ordinary conversation content.
//!
//! # Why compaction is a state assignment
//!
//! The fold does `live := context`; it never diffs. An earlier design modelled
//! compaction as "remove these ids", which cannot express Codex at all: its
//! replacement history is a *new* list of events that shares no id with what
//! came before. Both readers normalize onto [`Body::Compaction::context`], so
//! nothing here branches on [`crate::ir::Origin::agent`].
//!
//! The assignment carries the *complete, ordered* post-operation context, and
//! everything after it in this file defers to that. The replay follows the
//! compaction's order rather than the file's; the fork prune may narrow what
//! came before a boundary but never what the newest compaction placed after
//! one. Both used to be quietly overruled — by a final sort back into file
//! order, and by a fork walk that kept the checkpoint only if it happened to
//! reach it — and neither showed up on the corpus, because both readers record
//! their context in file order and their leaves reach their boundaries. A rule
//! that is only correct on the sessions that exist is not the rule the type is
//! claiming.

use std::collections::{HashMap, HashSet};

use crate::ir::{Body, SessionIr, Visibility};

/// The resolved replay: what to show, what was dropped, and where the
/// checkpoints were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayPlan {
    /// Ids, in order, the target model should be shown. Each appears once.
    ///
    /// Not yet guaranteed to *resolve*: a [`Body::Compaction`] naming context
    /// this session does not contain still puts that id here, and
    /// [`SessionIr::model_visible`] drops it — turning a lookup miss into a
    /// shorter replay, which nothing downstream can tell from a shorter
    /// conversation. Both current readers build `context` out of ids they have
    /// already emitted (4,831 corpus compactions, 0 dangling), so this is
    /// latent, and closing it needs a paired change in
    /// `conformance::invariants`, which is the only thing that reports it and
    /// reads it out of this field.
    pub events: Vec<String>,
    /// What the fold removed and why. Ordered, for the fidelity report.
    pub excluded: Vec<Excluded>,
    /// Checkpoints applied, oldest first.
    pub checkpoints: Vec<String>,
}

/// One event the fold dropped, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub id: String,
    pub reason: ExclusionReason,
}

/// Why an event that is in the capture is not in the replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionReason {
    /// A compaction displaced it.
    Superseded { by: String },
    /// A [`Body::Rollback`] undid the turn it belonged to.
    RolledBack { by: String },
    /// Not on the branch the live leaf sits on: an edited or retried message.
    AbandonedFork,
    /// [`Visibility::Unclassified`] — this version cannot tell whether the
    /// model saw it. Ui and Telemetry are not losses and are not recorded
    /// here, because a report that lists every rendering artifact hides the
    /// entries that matter.
    NotModelVisible,
}

/// Fold the capture into the context the target should be handed.
pub fn resolve(ir: &SessionIr) -> ReplayPlan {
    // `Event::id` is documented unique within the session and every map here
    // keys on it, so a repeat means one copy is unreachable by id and which one
    // is arbitrary. First-wins, stated rather than left to whatever
    // `Iterator::collect` does with a duplicate key, and the replay below is
    // deduplicated so the target is never shown the same event twice.
    //
    // This used to be a `debug_assert_eq!` — free in release, which is where
    // every real conversion runs, and where the fold was pushing both copies of
    // a repeated id straight into the replay. A panic is the wrong repair: no
    // parse failure may fail a conversion, and the resolver can still answer
    // correctly for every *other* event in the session. Detection is not lost
    // by dropping the assert, because it never lived here — `conformance::
    // invariants` counts duplicates from the event list itself and reports
    // them, and `CaptureReport::id_collisions` records the ones a reader
    // resolved. See `claude_code_ir::Sink::emit`, which makes them
    // unrepresentable on the one provider that emits them.
    let mut position: HashMap<&str, usize> = HashMap::with_capacity(ir.events.len());
    for (index, event) in ir.events.iter().enumerate() {
        position.entry(event.id.as_str()).or_insert(index);
    }

    let mut live: Vec<String> = Vec::new();
    let mut excluded: Vec<Excluded> = Vec::new();
    let mut checkpoints: Vec<String> = Vec::new();
    // The newest checkpoint's surviving context, carried out of the fold rather
    // than looked up again afterwards. `prune_forks` needs it, and recovering
    // it from `checkpoints.last()` meant an id lookup that could miss and a
    // `match` on the body it found — two ways to silently decide the newest
    // compaction preserved nothing.
    let mut checkpoint_context: Vec<String> = Vec::new();

    for event in &ir.events {
        // The visibility gate, with the history directives exempted. Codex
        // writes rollback and abort as `event_msg`, which the reader correctly
        // files as `Ui` because it is rendering rather than context — but they
        // are directives to this fold rather than content, so they are read
        // *before* the gate. Behind it the rollback rule fires on zero of 714
        // real rollbacks. A compaction marker is the third of them, for the same
        // reason: it is an instruction about model content, not model content.
        //
        // The exemption goes through `Body::is_history_directive`, which is an
        // exhaustive `match` rather than a `matches!` here, so that a fourth
        // directive variant cannot be omitted from it without a compile error.
        // Inline, this was the one hole left in the retype — and the list itself
        // still had one, because it left the compaction marker out.
        if event.visibility != Visibility::Model && !event.body.is_history_directive() {
            if event.visibility == Visibility::Unclassified {
                excluded.push(Excluded {
                    id: event.id.clone(),
                    reason: ExclusionReason::NotModelVisible,
                });
            }
            continue;
        }

        // NO WILDCARD ARM. Every variant is named, and the last arm lists the
        // ones that are ordinary content rather than collapsing into `_`. That
        // is the whole point: adding a `Body` variant must be a compile error
        // here — one that names this file as the place that has to decide
        // whether the new thing edits history — instead of compiling clean and
        // being silently replayed to the target as conversation. Do not "tidy"
        // the list back into a wildcard.
        match &event.body {
            Body::Rollback { turns } => {
                roll_back(ir, &position, *turns, &event.id, &mut live, &mut excluded);
            }
            Body::Abort { .. } => {
                // Annotate only. See the module docs: the interrupted turn's
                // partial output stayed in the model's context, so removing it
                // here loses work rather than recovering fidelity.
            }
            Body::Compaction { context, .. } => {
                let kept: HashSet<&str> = context.iter().map(String::as_str).collect();
                for id in &live {
                    if !kept.contains(id.as_str()) {
                        excluded.push(Excluded {
                            id: id.clone(),
                            reason: ExclusionReason::Superseded {
                                by: event.id.clone(),
                            },
                        });
                    }
                }
                // The marker itself never joins `live`: it is a boundary, not
                // replayable content.
                checkpoint_context = context.clone();
                live = context.clone();
                checkpoints.push(event.id.clone());
            }
            // Ordinary content: model-visible, and it edits no history. Several
            // of these are chrome that only reaches this arm when a reader
            // marks it `Model`, which is the reader's call to make and not
            // something to second-guess here.
            Body::Message { .. }
            | Body::Reasoning { .. }
            | Body::ToolCall { .. }
            | Body::ToolResult { .. }
            | Body::SealedContext { .. }
            | Body::TurnConfig { .. }
            | Body::EnvSnapshot { .. }
            | Body::Attachment { .. }
            | Body::Control { .. }
            | Body::Unknown { .. } => live.push(event.id.clone()),
        }
    }

    let live = prune_forks(
        ir,
        &checkpoint_context,
        checkpoints.last().map(String::as_str),
        live,
        &mut excluded,
    );

    // `live` is already in conversation order and nothing here re-derives it:
    // content joins it in file order, a compaction replaces it wholesale with
    // its own ordered context, and rollback and the fork prune only ever
    // remove. So the one place the order departs from the file is the one place
    // it is meant to — `Body::Compaction::context` is the COMPLETE *ordered*
    // post-operation context, and that is the whole claim the state assignment
    // makes.
    //
    // Sorting by file position here undid exactly that claim, and nothing
    // caught it: both current readers record their context in file order (4,831
    // corpus compactions, 0 that do not), so the sort was a no-op on every real
    // session while making the fold's authority fictional for the next reader.
    //
    // The `filter` is the id-uniqueness guarantee. A repeated `Event::id` puts
    // both copies in `live`, and one of the two events is unreachable by id
    // anyway — every map here and in `model_visible` keys on it. Nothing can
    // recover the shadowed event at this point, but the target must at least
    // not be shown the survivor twice.
    let mut seen: HashSet<&str> = HashSet::with_capacity(live.len());
    let events: Vec<String> = live
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect();

    ReplayPlan {
        events,
        excluded,
        checkpoints,
    }
}

/// Undo the last `turns` typed turns of the live set.
///
/// `turns` is 1 in all 714 corpus occurrences. Zero is read as one rather than
/// as "nothing", to match the reader's rule that a rollback which cannot say
/// how far it goes still goes exactly one turn: neither end of the range may be
/// allowed to eat the session or to quietly do nothing.
fn roll_back(
    ir: &SessionIr,
    position: &HashMap<&str, usize>,
    turns: u32,
    by: &str,
    live: &mut Vec<String>,
    excluded: &mut Vec<Excluded>,
) {
    let num_turns = turns.max(1) as usize;

    let turn_of = |id: &str| -> Option<&str> {
        position
            .get(id)
            .and_then(|index| ir.events[*index].turn.as_deref())
    };

    // Distinct turns in order of last appearance, so "the last N turns" means
    // the N that most recently produced a live event rather than the N that
    // started most recently.
    let mut order: Vec<&str> = Vec::new();
    for id in live.iter() {
        let Some(turn) = turn_of(id) else { continue };
        if let Some(seen) = order.iter().position(|other| *other == turn) {
            order.remove(seen);
        }
        order.push(turn);
    }
    let undone: HashSet<&str> = order.iter().rev().take(num_turns).copied().collect();

    live.retain(|id| {
        // An event with no turn is never dropped: a rollback that cannot
        // identify which turn an event belongs to must not guess.
        match turn_of(id) {
            Some(turn) if undone.contains(turn) => {
                excluded.push(Excluded {
                    id: id.clone(),
                    reason: ExclusionReason::RolledBack { by: by.to_string() },
                });
                false
            }
            _ => true,
        }
    });
}

/// Drop live events that belong to an abandoned branch of Claude's DAG.
///
/// The live branch is the newest checkpoint's context, plus the leaf's
/// ancestors, plus the leaf's descendants. All three parts are load-bearing.
/// On corpus transcript `2d68b149` the composed rule keeps 1,311 events; the
/// ancestors alone keep 1,309, and a plain ancestor walk with no checkpoint at
/// all keeps 3, because compaction re-roots the graph and the head sits at
/// chain depth 3 behind it.
///
/// `checkpoint_context` is the newest compaction's surviving context and is
/// passed in rather than looked back up from the marker id, so that the one
/// thing this walk must not lose cannot go missing in a lookup.
///
/// Codex names no [`SessionIr::live_head`], so this returns `live` untouched —
/// the no-op is structural rather than an agent check.
fn prune_forks(
    ir: &SessionIr,
    checkpoint_context: &[String],
    checkpoint_marker: Option<&str>,
    live: Vec<String>,
    excluded: &mut Vec<Excluded>,
) -> Vec<String> {
    let Some(leaf) = ir.live_head.as_deref() else {
        return live;
    };

    let mut records: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for event in &ir.events {
        let record = record_of(&event.id);
        records.entry(record).or_default().push(&event.id);
        if let Some(parent) = &event.parent
            && !parent_of.contains_key(record)
        {
            let parent_record = record_of(parent);
            parent_of.insert(record, parent_record);
            children_of.entry(parent_record).or_default().push(record);
        }
    }
    // An unknown leaf is not a licence to truncate.
    if !records.contains_key(leaf) {
        return live;
    }

    let checkpoint_ids: HashSet<&str> = checkpoint_context.iter().map(String::as_str).collect();
    // The marker is the boundary as much as its context is, and sometimes it
    // is the only part of the boundary the walk can reach. Corpus transcript
    // `55f695db` compacts 20 lines from the end and gives that
    // `compact_boundary` a `logicalParentUuid` naming a record that is not in
    // the file, so a walk that recognised only context ids died on the marker
    // and stopped one record short of the preserved segment.
    let checkpoint_marker = checkpoint_marker.map(record_of);

    // The newest compaction already decided this: after the boundary the
    // model's context *is* `context`, assigned rather than diffed. This prune
    // is a membership test over a DAG that compaction re-roots, so whether the
    // leaf's parent chain happens to reach the boundary is a fact about the
    // transcript graph and not a verdict on the preserved history. Seeded
    // unconditionally, because the alternative is that an unreachable
    // checkpoint deletes every event the compaction preserved and reports the
    // lot as `AbandonedFork` — the whole post-compaction session, with the
    // fidelity report calling it a pruned branch.
    let mut keep: HashSet<&str> = checkpoint_ids.iter().copied().collect();
    let mut walked: HashSet<&str> = HashSet::new();
    let mut cursor = Some(leaf);
    while let Some(record) = cursor {
        // A malformed transcript can point a record at its own ancestor.
        if !walked.insert(record) {
            break;
        }
        let Some(ids) = records.get(record) else { break };
        let reached_checkpoint = checkpoint_marker == Some(record)
            || ids.iter().any(|id| checkpoint_ids.contains(id));
        keep.extend(ids.iter().copied());
        // Stop *at* the boundary. Everything the compaction kept is already in
        // `keep`; everything above it was superseded and must not be walked
        // back into the replay.
        if reached_checkpoint {
            break;
        }
        cursor = parent_of.get(record).copied();
    }

    // The head is recorded when the user submits, so the leaf it names is the
    // message the turn was attached to, not the head of the conversation.
    // Everything the agent produced in reply descends from that leaf and is as
    // live as the leaf itself; an ancestors-only walk silently truncates the
    // final turn of every transcript that has one.
    let mut descended: HashSet<&str> = HashSet::new();
    let mut frontier = vec![leaf];
    while let Some(record) = frontier.pop() {
        if !descended.insert(record) {
            continue;
        }
        if let Some(ids) = records.get(record) {
            keep.extend(ids.iter().copied());
        }
        if let Some(children) = children_of.get(record) {
            frontier.extend(children.iter().copied());
        }
    }

    let (kept, abandoned): (Vec<String>, Vec<String>) =
        live.into_iter().partition(|id| keep.contains(id.as_str()));
    excluded.extend(abandoned.into_iter().map(|id| Excluded {
        id,
        reason: ExclusionReason::AbandonedFork,
    }));
    kept
}

/// Ids of split blocks are `<uuid>#<slot>`, while parent links and `leafUuid`
/// name the record. The DAG walk is therefore over records, not events.
fn record_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Body, Branch, Event, Role, SourceRef, Visibility};
    use serde_json::Value;

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

    fn message(id: &str) -> Event {
        event(
            id,
            Visibility::Model,
            Body::Message {
                role: Role::User,
                blocks: vec![Block::Text { text: id.into() }],
            },
        )
    }

    fn turned(id: &str, turn: &str) -> Event {
        let mut event = message(id);
        event.turn = Some(turn.to_string());
        event
    }

    fn compaction(id: &str, context: &[&str], supersedes: &[&str]) -> Event {
        event(
            id,
            Visibility::Model,
            Body::Compaction {
                context: context.iter().map(|id| id.to_string()).collect(),
                supersedes: supersedes.iter().map(|id| id.to_string()).collect(),
                note: None,
                window_from: None,
                window_to: None,
            },
        )
    }

    /// A rollback as a real reader emits one: `Visibility::Ui`, because that is
    /// what Codex records. Every rollback test below therefore also pins that
    /// the fold reads the directive *before* its visibility gate.
    fn rollback(id: &str, turns: u32) -> Event {
        event(id, Visibility::Ui, Body::Rollback { turns })
    }

    /// Likewise `Ui`, for the same reason. `turn` goes on the event, which is
    /// where the reader puts it and the only place it lives.
    fn abort(id: &str, turn: &str) -> Event {
        let mut event = event(id, Visibility::Ui, Body::Abort {});
        event.turn = Some(turn.to_string());
        event
    }

    fn ir(events: Vec<Event>) -> SessionIr {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events = events;
        ir
    }

    /// A session whose live branch head is known, as Claude Code records one.
    fn forked(events: Vec<Event>, live_head: &str) -> SessionIr {
        let mut ir = ir(events);
        ir.live_head = Some(live_head.to_string());
        ir
    }

    #[test]
    fn compaction_assigns_rather_than_diffs() {
        // The Codex shape: the replacement shares no id with what came before.
        let plan = resolve(&ir(vec![
            message("a"),
            message("b"),
            message("summary"),
            compaction("c", &["summary"], &["a", "b"]),
            message("after"),
        ]));

        assert_eq!(plan.events, ["summary", "after"]);
        assert_eq!(plan.checkpoints, ["c"]);
        assert_eq!(
            plan.excluded,
            vec![
                Excluded {
                    id: "a".into(),
                    reason: ExclusionReason::Superseded { by: "c".into() }
                },
                Excluded {
                    id: "b".into(),
                    reason: ExclusionReason::Superseded { by: "c".into() }
                },
            ]
        );
    }

    #[test]
    fn a_later_compaction_supersedes_the_earlier_context() {
        let plan = resolve(&ir(vec![
            message("old"),
            message("sum1"),
            compaction("c1", &["sum1"], &["old"]),
            message("mid"),
            message("sum2"),
            compaction("c2", &["sum2"], &["sum1", "mid"]),
        ]));

        assert_eq!(plan.events, ["sum2"]);
        assert_eq!(plan.checkpoints, ["c1", "c2"]);
    }

    #[test]
    fn unclassified_is_reported_and_chrome_is_not() {
        let plan = resolve(&ir(vec![
            message("a"),
            event(
                "ui",
                Visibility::Ui,
                Body::Control {
                    control_kind: "mode".into(),
                    data: Value::Null,
                },
            ),
            event(
                "tel",
                Visibility::Telemetry,
                Body::Control {
                    control_kind: "token_count".into(),
                    data: Value::Null,
                },
            ),
            event(
                "huh",
                Visibility::Unclassified,
                Body::Unknown {
                    native_type: None,
                    raw: Value::Null,
                },
            ),
        ]));

        assert_eq!(plan.events, ["a"]);
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                id: "huh".into(),
                reason: ExclusionReason::NotModelVisible
            }]
        );
    }

    #[test]
    fn abort_removes_nothing() {
        // 286 corpus aborts had already produced output that stayed in the
        // model's context, so the partial turn is kept verbatim.
        let plan = resolve(&ir(vec![
            turned("a", "t1"),
            turned("partial", "t2"),
            abort("abort", "t2"),
            turned("b", "t3"),
        ]));

        assert_eq!(plan.events, ["a", "partial", "b"]);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn rollback_drops_the_last_turn_only() {
        let plan = resolve(&ir(vec![
            turned("a", "t1"),
            turned("b", "t2"),
            turned("c", "t2"),
            rollback("rb", 1),
            turned("d", "t3"),
        ]));

        assert_eq!(plan.events, ["a", "d"]);
        assert_eq!(
            plan.excluded,
            vec![
                Excluded {
                    id: "b".into(),
                    reason: ExclusionReason::RolledBack { by: "rb".into() }
                },
                Excluded {
                    id: "c".into(),
                    reason: ExclusionReason::RolledBack { by: "rb".into() }
                },
            ]
        );
    }

    /// The rollback directive is read before the visibility gate.
    ///
    /// Codex writes `thread_rolled_back` as an `event_msg`, so the reader files
    /// it as `Ui` — correctly, it *is* chrome to render. Behind the gate the
    /// rule fired on zero of 714 real rollbacks while every unit test passed,
    /// because a fixture is free to mark the marker `Model` and a rollout is
    /// not. This states the gate order directly rather than relying on the
    /// other tests' choice of helper.
    #[test]
    fn a_ui_rollback_still_fires() {
        let directive = event("rb", Visibility::Ui, Body::Rollback { turns: 1 });
        let plan = resolve(&ir(vec![turned("a", "t1"), turned("b", "t2"), directive]));

        assert_eq!(plan.events, ["a"]);
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                id: "b".into(),
                reason: ExclusionReason::RolledBack { by: "rb".into() }
            }]
        );
    }

    /// A rollback of no turns is a rollback of one.
    ///
    /// `Rollback { turns }` cannot express "absent" and should not: the readers
    /// resolve a missing count to 1 at the point they read it. Zero is the one
    /// value that can still arrive here, and it means the same thing — a
    /// rollback that cannot say how far it goes goes exactly one turn.
    #[test]
    fn rollback_of_zero_turns_undoes_one_turn() {
        let plan = resolve(&ir(vec![
            turned("a", "t1"),
            turned("b", "t2"),
            rollback("rb", 0),
        ]));

        assert_eq!(plan.events, ["a"]);
    }

    #[test]
    fn rollback_never_drops_an_event_with_no_turn() {
        let plan = resolve(&ir(vec![
            message("untyped"),
            turned("b", "t2"),
            rollback("rb", 5),
        ]));

        assert_eq!(plan.events, ["untyped"]);
    }

    #[test]
    fn no_live_head_means_no_fork_prune() {
        // Codex names no head. The prune must then be inert, structurally,
        // rather than because the resolver recognised an agent.
        let a = message("a");
        let mut b = message("b");
        b.parent = Some("a".into());
        let mut retry = message("retry");
        retry.parent = Some("a".into());

        let plan = resolve(&ir(vec![a, b, retry]));

        assert_eq!(plan.events, ["a", "b", "retry"]);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn a_leaf_that_names_nothing_does_not_truncate() {
        let mut a = message("a");
        a.parent = None;
        let mut b = message("b");
        b.parent = Some("a".into());
        let plan = resolve(&forked(vec![a, b], "nobody"));

        assert_eq!(plan.events, ["a", "b"]);
    }

    #[test]
    fn an_abandoned_branch_is_pruned() {
        // The user edited `a`'s reply: `retry` re-parents onto `a`, and the
        // original `b` stays in the file on a dead branch.
        let a = message("a");
        let mut b = message("b");
        b.parent = Some("a".into());
        let mut retry = message("retry");
        retry.parent = Some("a".into());
        let plan = resolve(&forked(vec![a, b, retry], "retry"));

        assert_eq!(plan.events, ["a", "retry"]);
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                id: "b".into(),
                reason: ExclusionReason::AbandonedFork
            }]
        );
    }

    #[test]
    fn the_reply_to_the_last_prompt_survives() {
        // The head names the leaf the turn was attached to, so the whole reply
        // is a descendant of it rather than an ancestor.
        let a = message("a");
        let mut leaf = message("leaf");
        leaf.parent = Some("a".into());
        let mut reply = message("reply");
        reply.parent = Some("leaf".into());
        let mut tool = message("tool");
        tool.parent = Some("reply".into());

        let plan = resolve(&forked(vec![a, leaf, reply, tool], "leaf"));

        assert_eq!(plan.events, ["a", "leaf", "reply", "tool"]);
        assert!(plan.excluded.is_empty());
    }

    #[test]
    fn the_walk_stops_at_the_newest_checkpoint() {
        // The re-rooting case. `kept1`/`kept2` survive the boundary but the
        // parent chain from the leaf reaches only `kept2`; a pure ancestor
        // walk would return two events out of four.
        let a = message("a");
        let mut kept1 = message("kept1");
        kept1.parent = Some("a".into());
        let mut kept2 = message("kept2");
        kept2.parent = Some("kept1".into());
        let mut boundary = compaction("cb", &["kept1", "kept2"], &["a"]);
        boundary.parent = Some("kept2".into());
        let mut after = message("after");
        after.parent = Some("kept2".into());

        let plan = resolve(&forked(vec![a, kept1, kept2, boundary, after], "after"));

        assert_eq!(plan.events, ["kept1", "kept2", "after"]);
    }

    /// Reaching the newest checkpoint's *marker* counts as reaching the
    /// checkpoint.
    ///
    /// Corpus transcript `55f695db` compacts 20 lines from the end and gives
    /// that `compact_boundary` a `logicalParentUuid` naming a record that is
    /// not in the file, so the marker is the only part of the boundary the walk
    /// can reach. Recognising context ids alone dropped the preserved segment:
    /// 9 events instead of 11.
    #[test]
    fn reaching_the_checkpoint_marker_counts_as_reaching_the_checkpoint() {
        let kept1 = message("kept1");
        let mut kept2 = message("kept2");
        kept2.parent = Some("kept1".into());
        // The marker's own parent is a record the file does not contain.
        let mut boundary = compaction("cb", &["kept1", "kept2"], &[]);
        boundary.parent = Some("not-in-this-file".into());
        let mut after = message("after");
        after.parent = Some("cb".into());

        let plan = resolve(&forked(vec![kept1, kept2, boundary, after], "after"));

        assert_eq!(
            plan.events,
            ["kept1", "kept2", "after"],
            "the walk reached the marker and must take the whole checkpoint \
             context with it, not stop one record short of the preserved segment"
        );
    }

    /// The compaction's own order is the replay's order.
    ///
    /// `Body::Compaction::context` is the COMPLETE ordered post-operation
    /// context, so an order that differs from the file's is the compaction's to
    /// choose. Both current readers happen to emit it in file order, which is
    /// why re-sorting by file position looked harmless — it made the state
    /// assignment's authority fictional without any corpus symptom to say so.
    #[test]
    fn compaction_context_order_is_the_replay_order() {
        let plan = resolve(&ir(vec![
            message("a"),
            message("b"),
            compaction("c", &["b", "a"], &[]),
            message("after"),
        ]));

        assert_eq!(plan.events, ["b", "a", "after"]);
        assert!(plan.excluded.is_empty());
    }

    /// A repeated id is replayed once, in release as well as in debug.
    ///
    /// `Event::id` is documented unique and every map here keys on it, but this
    /// used to be a `debug_assert` — free in release, which is where every real
    /// conversion runs. A hand-built, deserialized or future-provider IR that
    /// repeats an id would then have the fold push both copies and hand the
    /// target the same message twice.
    #[test]
    fn a_repeated_id_is_replayed_once() {
        let plan = resolve(&ir(vec![message("a"), message("a"), message("b")]));

        assert_eq!(plan.events, ["a", "b"]);
    }

    /// The fork prune may not contradict the newest compaction.
    ///
    /// The compaction assigned the model's context; `prune_forks` is a
    /// membership test over a DAG that compaction re-roots, so a leaf whose
    /// parent chain never reaches the boundary must not be read as "none of the
    /// preserved history is live". Reachability is a property of the transcript
    /// graph, not a licence to delete the post-compaction session.
    #[test]
    fn the_checkpoint_context_survives_a_leaf_that_cannot_reach_it() {
        let a = message("a");
        let mut kept = message("kept");
        kept.parent = Some("a".into());
        // The user retried from `a`, so the live leaf sits on a sibling branch
        // that neither descends from nor leads to the preserved segment.
        let mut other = message("other");
        other.parent = Some("a".into());
        let mut boundary = compaction("cb", &["kept"], &["a", "other"]);
        boundary.parent = Some("kept".into());

        let plan = resolve(&forked(vec![a, kept, other, boundary], "other"));

        assert_eq!(
            plan.events,
            ["kept"],
            "the fold placed `kept` in the model's context; the fork prune \
             cannot take it back out"
        );
    }

    /// A compaction marker is read before the visibility gate, like every other
    /// history directive.
    ///
    /// Both current readers file the marker `Model`, so this is latent — but a
    /// marker is not model content, it is an instruction about model content,
    /// and a provider that records it as chrome (Codex records its other two
    /// directives exactly that way) would have the whole rewrite skipped.
    #[test]
    fn a_ui_compaction_still_fires() {
        let mut boundary = compaction("cb", &["summary"], &["old"]);
        boundary.visibility = Visibility::Ui;
        let plan = resolve(&ir(vec![message("old"), message("summary"), boundary]));

        assert_eq!(plan.events, ["summary"]);
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                id: "old".into(),
                reason: ExclusionReason::Superseded { by: "cb".into() }
            }]
        );
    }

    /// KNOWN GAP, pinned deliberately: a context id naming nothing is replayed
    /// as an absence.
    ///
    /// This asserts what the fold does *today*, not what it should do, so that
    /// closing the gap breaks this test and lands on this comment.
    ///
    /// The defect is the shape this codebase keeps getting bitten by — a lookup
    /// miss becoming an absence. `SessionIr::model_visible` resolves plan ids
    /// against the event list, so an id naming no event vanishes there: a
    /// compaction whose whole `context` is absent supersedes the real history
    /// and replays as an *empty* model context, which compares clean against an
    /// empty file and grades `Fidelity::ContextComplete`.
    ///
    /// Latent — both readers build `context` from ids they have already
    /// emitted, 0 dangling across 4,831 corpus compactions — and it cannot be
    /// fixed here alone. The repair is for the fold to keep unresolvable ids out
    /// of `ReplayPlan::events` and report them separately, but
    /// `conformance::invariants` is the only thing that reports this today and
    /// it detects it by scanning `plan.events` for ids absent from the capture.
    /// A resolver that stops emitting them silences that detector and fails
    /// `conformance::tests::a_replay_naming_an_absent_event_is_a_finding`. Both
    /// files have to move together.
    #[test]
    fn a_context_id_naming_nothing_currently_replays_as_an_absence() {
        let session = ir(vec![message("a"), compaction("c", &["missing", "a"], &[])]);
        let plan = resolve(&session);

        assert_eq!(plan.events, ["missing", "a"]);
        assert_eq!(
            session.model_visible().len(),
            1,
            "KNOWN GAP: the plan names two ids and only one resolves, so the \
             replay is silently one event short"
        );
    }

    #[test]
    fn split_block_ids_walk_by_record() {
        // One native record expands into `<uuid>` and `<uuid>#1`; the parent
        // link names the record, so both slots must survive the walk.
        let a = message("a");
        let mut b = message("b");
        b.parent = Some("a".into());
        let mut b1 = message("b#1");
        b1.parent = Some("a".into());
        let plan = resolve(&forked(vec![a, b, b1], "b"));

        assert_eq!(plan.events, ["a", "b", "b#1"]);
    }
}
