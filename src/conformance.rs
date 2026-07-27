//! One battery every provider on the structured track has to pass.
//!
//! Adding a provider to the high-fidelity track used to mean remembering to
//! write N separate ad-hoc tests, scattered across `tests/roundtrip_ir_test.rs`,
//! `tests/compare_test.rs` and `tests/corpus_test.rs`, each with its own corpus
//! discovery and its own idea of what "lossless" means. Nothing enforced that
//! the list was complete, and a provider that opted in without them would have
//! looked exactly like one that was verified.
//!
//! So the list of providers under test is not a list at all: it is
//! [`crate::discovery::ProviderRegistry::default_registry`] filtered by
//! [`Provider::supports_structured_write`]. A provider that overrides that
//! method is covered by everything below on the next `cargo test`, and cannot
//! be forgotten, because forgetting would mean not opting in.
//!
//! # Why this is a module and not a test file
//!
//! Three reasons, in order of weight.
//!
//! 1. A provider author has to be able to *run* it — against one file, before
//!    the corpus, from their own scratch test — rather than read a 900-line
//!    integration test to find out what the contract is. [`run`] takes paths
//!    and returns a [`Report`].
//! 2. The battery needs the crate's internals ([`crate::replay`],
//!    [`crate::compare`], the [`Provider`] trait) and nothing else. Living in
//!    `src/` means it is compiled by `cargo check` and typechecked against
//!    every IR change, instead of only when the test target is built.
//! 3. `tests/conformance_test.rs` then holds what only a test can hold: the
//!    corpus discovery, the process-wide environment sandbox, the tier choice,
//!    and the assertions.
//!
//! The contract is deliberately two items wide — [`run`] and [`Report`] — so
//! that everything about *how* conformance is measured stays behind it. What
//! goes in is a list of files and a scratch directory; what comes out is every
//! count the battery took and every objection it has.
//!
//! The file is long and that is the right shape for it: the interface is two
//! items, and the length is the checks themselves plus the printer. Splitting
//! the checks across files would spread one contract — "what a structured
//! provider has to satisfy" — over several places to read, which is the cost the
//! ad-hoc tests already had. Nothing here is reachable except through [`run`],
//! so the surface a caller has to understand does not grow with the body.
//!
//! # Counts, not just verdicts
//!
//! Every defect this crate has had was silent, and every one was found by
//! counting something against the corpus rather than by a failing assertion
//! (see the table in `docs/EXTENDING.md`). So [`Report::print`] emits the
//! tallies for every check whether or not it objects, and [`Report::findings`]
//! is what fails the build. A suite that only says PASS is half a suite.
//!
//! # What is checked
//!
//! Per source session, for every structured provider as the write target:
//!
//! - **Attribution.** Some structured reader claims the file and stamps
//!   [`crate::ir::Origin::agent`] with its own slug — [`crate::compare::vendor_of`]
//!   and the writers' same-agent test both key on that string.
//! - **Replay closure.** `captured == replayed + excluded + markers + chrome`
//!   exactly, no id in both `events` and `excluded`, every id naming a real
//!   event, and [`crate::ir::SessionIr::live_head`] naming a real record.
//! - **Conservation, same agent.** read → resolve → write → read → resolve
//!   conserves model-visible events *and* capsules exactly, adds nothing,
//!   claims [`Fidelity::ContextComplete`] or better, and carries no losses.
//! - **Prediction, cross agent.** The comparator finds nothing unexplained and
//!   nothing carried across a vendor boundary [`crate::ir::Capsule::fits`]
//!   forbade, and the writer's claimed grade is no better than the grade the
//!   written file independently supports.
//! - **Grade derivation.** The claimed [`Fidelity`] equals the worst grade in
//!   the writer's own loss list. Both current writers derive it that way; this
//!   verifies the property rather than assuming it.
//! - **`Body` coverage.** Every [`Body`] variant a reader emits into a replay
//!   is reproduced by that agent's own writer. A variant added to a reader and
//!   forgotten in its writer is named, per variant, rather than showing up as
//!   an event-count mismatch.
//!
//! # Two-sided
//!
//! Every allowance below is checked from both ends, because an allowance that
//! stops being exercised does not stay neutral — it silently widens until it
//! covers a regression. A cross-agent crossing whose source carried sealed
//! material must both lose it (`target_capsules == 0`,
//! `carried_foreign` empty) *and* report having lost it (`predicted`
//! non-empty). `tests/real_world_roundtrip_test.rs::assert_roundtrip_lossless_except`
//! makes the same argument on the flat side.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::budget::ContextBudget;
use crate::compare::{Comparison, compare, vendor_of};
use crate::discovery::ProviderRegistry;
use crate::ir::{Body, Event, Fidelity, Loss, SessionIr, Visibility};
use crate::pipeline::{ConversionPipeline, ConvertOptions};
use crate::providers::{Provider, WriteOptions};
use crate::replay::{ExclusionReason, ReplayPlan, resolve};
use crate::store::Store;

/// How many example paths a finding list keeps before it starts counting only.
const EXAMPLES: usize = 5;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Put every file in `files` through the battery and report what happened.
///
/// `tier` names what is being run — `"fixtures"`, `"corpus"` — and is printed,
/// so that "the corpus was absent" can never read as "the suite passed".
///
/// `sandbox` is a writable scratch directory. Every structured write goes
/// through [`Provider::write_session_ir`], which places files under the
/// provider's own session root, so the caller **must** have redirected those
/// roots into `sandbox` (by pointing `HOME` at it) before calling. The battery
/// verifies every written path lands inside `sandbox` and panics if one does
/// not: a session corpus is read-only, and a sandbox that silently is not one
/// would write into it.
///
/// Files no structured reader claims are counted and reported, never failed —
/// a fixtures directory holds other providers' formats too.
pub fn run(tier: &str, files: &[PathBuf], sandbox: &Path) -> Report {
    let registry = ProviderRegistry::default_registry();
    let providers: Vec<&dyn Provider> = registry
        .all_providers()
        .into_iter()
        .filter(|provider| provider.supports_structured_write())
        .collect();

    let mut report = Report::new(tier, files.len(), &providers);
    if providers.is_empty() {
        report.finding(
            "the registry declares no provider with `supports_structured_write`; \
             the battery had nothing to check",
        );
        return report;
    }

    let mut preferred = 0usize;
    for path in files {
        let Some((index, ir)) = attribute(&providers, &mut preferred, path, &mut report) else {
            continue;
        };
        let source = providers[index];
        let plan = resolve(&ir);

        let closure = invariants(&mut report, path, &ir, &plan);
        report.resolve_tally(source.slug()).add(&closure);
        let source_kinds = kinds_of(&ir, &plan);

        for &target in &providers {
            cross(&mut report, source, target, path, &ir, &source_kinds, sandbox);
        }
    }

    report.finish();
    report
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// Which structured provider's reader claims `path`, and the IR it produced.
///
/// A provider claims a file when its own reader parses it into a non-empty
/// replay. `preferred` remembers the last claimant and is tried first: corpora
/// are grouped by provider, so this is one read per file instead of one read
/// per provider per file — and the largest rollout in the corpus is 281 MiB.
fn attribute(
    providers: &[&dyn Provider],
    preferred: &mut usize,
    path: &Path,
    report: &mut Report,
) -> Option<(usize, SessionIr)> {
    let order = std::iter::once(*preferred).chain(0..providers.len());
    let mut tried: HashSet<usize> = HashSet::new();
    let mut empty = false;

    for index in order {
        if !tried.insert(index) {
            continue;
        }
        let provider = providers[index];
        match provider.read_session_ir(path) {
            Ok(Some(ir)) => {
                if resolve(&ir).events.is_empty() {
                    empty = true;
                    continue;
                }
                if ir.origin.agent != provider.slug() {
                    report.finding(&format!(
                        "{}: reader stamped `origin.agent` as {:?} but the provider's slug is \
                         {:?}; `compare::vendor_of` and every writer's same-agent test key on \
                         that string, so they will both take the wrong branch",
                        path.display(),
                        ir.origin.agent,
                        provider.slug(),
                    ));
                }
                *preferred = index;
                report.claimed(provider.slug());
                return Some((index, ir));
            }
            Ok(None) => report.missing_reader(provider.slug()),
            Err(_) => report.probe_errors += 1,
        }
    }

    if empty {
        report.empty_replays += 1;
    } else {
        report.unattributed(path);
    }
    None
}

// ---------------------------------------------------------------------------
// Replay invariants
// ---------------------------------------------------------------------------

/// Where a captured event ends up once [`resolve`] has folded the session.
///
/// The four slots are exhaustive over the resolver's own control flow, which is
/// what makes the arithmetic below a closure rather than an inequality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Ordinary content. The fold pushes it, so it ends up either replayed or
    /// excluded with a reason — never neither, and never both.
    Replayable,
    /// A directive or a compaction boundary. The fold consumes it and it is
    /// never replayed, but it is not a loss either.
    Marker,
    /// Rendering or accounting the visibility gate drops without recording,
    /// because a report listing every rendering artifact hides the entries that
    /// matter.
    Chrome,
    /// [`Visibility::Unclassified`]: recorded as
    /// [`ExclusionReason::NotModelVisible`].
    Unclassified,
}

/// NO WILDCARD ARM over [`Body`]. A new variant must be a compile error naming
/// this file, so that whoever adds it decides whether it is content, a
/// directive, or chrome — the same rule `replay.rs` documents at its own match,
/// and for the same reason: a variant that falls into a wildcard here would be
/// counted as ordinary content and its absence from a replay would balance the
/// books.
fn slot(event: &Event) -> Slot {
    if event.visibility != Visibility::Model && !event.body.is_history_directive() {
        return match event.visibility {
            Visibility::Unclassified => Slot::Unclassified,
            Visibility::Ui | Visibility::Telemetry => Slot::Chrome,
            // Excluded by the condition above; spelled out so that a new
            // visibility forces a decision here rather than defaulting.
            Visibility::Model => Slot::Replayable,
        };
    }
    match &event.body {
        Body::Rollback { .. } | Body::Abort { .. } | Body::Compaction { .. } => Slot::Marker,
        Body::Message { .. }
        | Body::Reasoning { .. }
        | Body::ToolCall { .. }
        | Body::ToolResult { .. }
        | Body::SealedContext { .. }
        | Body::TurnConfig { .. }
        | Body::EnvSnapshot { .. }
        | Body::Attachment { .. }
        | Body::Control { .. }
        | Body::Unknown { .. } => Slot::Replayable,
    }
}

/// One session's replay accounting.
#[derive(Debug, Clone, Copy, Default)]
struct Closure {
    sessions: usize,
    captured: usize,
    replayed: usize,
    superseded: usize,
    rolled_back: usize,
    fork: usize,
    unclassified: usize,
    chrome: usize,
    markers: usize,
    checkpoints: usize,
    closed: usize,
    live_head: usize,
    duplicate_ids: usize,
}

/// Check every replay invariant on one session, and return its accounting.
///
/// The identity is
/// `captured == replayed + superseded + rolled_back + fork + unclassified + chrome + markers`.
/// The first four terms are the ones a reader is likely to break; the last
/// three are there because the identity has to be an equality to be worth
/// anything — leaving chrome out would let a replayed event go missing and be
/// absorbed by the slack.
fn invariants(report: &mut Report, path: &Path, ir: &SessionIr, plan: &ReplayPlan) -> Closure {
    let mut counts = Closure {
        sessions: 1,
        captured: ir.events.len(),
        replayed: plan.events.len(),
        checkpoints: plan.checkpoints.len(),
        ..Closure::default()
    };

    let mut slots: HashMap<&str, Slot> = HashMap::with_capacity(ir.events.len());
    let mut records: HashSet<&str> = HashSet::with_capacity(ir.events.len());
    let mut duplicate_ids = 0usize;
    for event in &ir.events {
        let slot = slot(event);
        match slot {
            Slot::Marker => counts.markers += 1,
            Slot::Chrome => counts.chrome += 1,
            // Counted from the resolver's own output instead — which is what
            // makes the identity below a cross-check rather than a restatement
            // of this loop.
            Slot::Replayable | Slot::Unclassified => {}
        }
        if slots.insert(event.id.as_str(), slot).is_some() {
            duplicate_ids += 1;
        }
        records.insert(record_of(&event.id));
    }
    counts.duplicate_ids = duplicate_ids;
    if duplicate_ids > 0 {
        report.finding(&format!(
            "{}: {duplicate_ids} event id(s) occur more than once, but `Event::id` is documented \
             unique within the session and every map in `replay.rs` and `ir.rs` keys on it — \
             `resolve`'s `position` and `model_visible`'s `by_id` keep one of the copies and \
             which one is arbitrary. The known cause is a provider re-appending records it \
             preserves across a compaction: Claude Code does this before `compact_boundary`, so \
             a reader that mints one event per line emits the same id twice. Note that such a \
             re-append is NOT byte-identical — Claude re-stamps `slug`, `promptId` and `cwd`, so \
             comparing raw records recognises none of them; compare the built `Event` minus \
             `source` and `turn`, keep the first occurrence, and mint a counted distinct id only \
             when the content genuinely differs. See `claude_code_ir::is_restatement`",
            path.display()
        ));
    }

    for excluded in &plan.excluded {
        match excluded.reason {
            ExclusionReason::Superseded { .. } => counts.superseded += 1,
            ExclusionReason::RolledBack { .. } => counts.rolled_back += 1,
            ExclusionReason::AbandonedFork => counts.fork += 1,
            ExclusionReason::NotModelVisible => counts.unclassified += 1,
        }
    }

    let replayed: BTreeSet<&str> = plan.events.iter().map(String::as_str).collect();
    if replayed.len() != plan.events.len() {
        report.finding(&format!(
            "{}: the replay names {} id(s) but only {} distinct one(s); the target would be \
             shown the same event twice",
            path.display(),
            plan.events.len(),
            replayed.len()
        ));
    }
    let excluded_ids: BTreeSet<&str> = plan.excluded.iter().map(|e| e.id.as_str()).collect();
    if excluded_ids.len() != plan.excluded.len() {
        report.finding(&format!(
            "{}: {} exclusion(s) name only {} distinct id(s); an event dropped twice is \
             counted twice in every fidelity report",
            path.display(),
            plan.excluded.len(),
            excluded_ids.len()
        ));
    }

    let both: Vec<&str> = replayed.intersection(&excluded_ids).copied().collect();
    if !both.is_empty() {
        report.finding(&format!(
            "{}: {} event(s) are both replayed and excluded, e.g. {:?}",
            path.display(),
            both.len(),
            &both[..both.len().min(3)]
        ));
    }

    let unknown_replayed = replayed
        .iter()
        .filter(|id| !slots.contains_key(**id))
        .count();
    if unknown_replayed > 0 {
        report.finding(&format!(
            "{}: the replay names {unknown_replayed} id(s) that are not events in the capture; \
             a compaction `context` naming something absent replays nothing while looking full",
            path.display()
        ));
    }
    let unknown_excluded = excluded_ids
        .iter()
        .filter(|id| !slots.contains_key(**id))
        .count();
    if unknown_excluded > 0 {
        report.finding(&format!(
            "{}: {unknown_excluded} exclusion(s) name an id that is not an event in the capture",
            path.display()
        ));
    }

    let chrome_replayed = replayed
        .iter()
        .filter(|id| matches!(slots.get(**id), Some(Slot::Chrome | Slot::Unclassified)))
        .count();
    if chrome_replayed > 0 {
        report.finding(&format!(
            "{}: {chrome_replayed} replayed id(s) belong to events the visibility gate dropped; \
             chrome in the model's context is the failure `Visibility` exists to prevent",
            path.display()
        ));
    }

    let orphaned = slots
        .iter()
        .filter(|(id, kind)| {
            **kind == Slot::Replayable && !replayed.contains(*id) && !excluded_ids.contains(*id)
        })
        .count();
    if orphaned > 0 {
        report.finding(&format!(
            "{}: {orphaned} model-visible event(s) are neither replayed nor excluded with a \
             reason; they left the fold with nothing to show they were dropped",
            path.display()
        ));
    }

    if let Some(head) = &ir.live_head {
        counts.live_head = 1;
        if !records.contains(record_of(head)) {
            report.finding(&format!(
                "{}: `live_head` names {head:?}, which is not a record in the capture; the \
                 resolver then prunes no forks at all and says nothing",
                path.display()
            ));
        }
    }

    let accounted = counts.replayed
        + counts.superseded
        + counts.rolled_back
        + counts.fork
        + counts.unclassified
        + counts.chrome
        + counts.markers;
    if accounted == counts.captured {
        counts.closed += 1;
    } else {
        report.finding(&format!(
            "{}: the replay does not close — {} captured event(s) against {accounted} accounted \
             for (replayed {} + superseded {} + rolled back {} + fork {} + unclassified {} + \
             chrome {} + markers {})",
            path.display(),
            counts.captured,
            counts.replayed,
            counts.superseded,
            counts.rolled_back,
            counts.fork,
            counts.unclassified,
            counts.chrome,
            counts.markers,
        ));
    }
    counts
}

/// Ids of split blocks are `<uuid>#<slot>`, while `live_head` names the record.
fn record_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

/// Model-visible events by [`Body::kind`], from a plan already resolved.
fn kinds_of(ir: &SessionIr, plan: &ReplayPlan) -> BTreeMap<String, usize> {
    let by_id: HashMap<&str, &Event> = ir
        .events
        .iter()
        .map(|event| (event.id.as_str(), event))
        .collect();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for id in &plan.events {
        if let Some(event) = by_id.get(id.as_str()) {
            *counts.entry(event.body.kind().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// One crossing: source session, target provider
// ---------------------------------------------------------------------------

fn cross(
    report: &mut Report,
    source: &dyn Provider,
    target: &dyn Provider,
    path: &Path,
    ir: &SessionIr,
    source_kinds: &BTreeMap<String, usize>,
    sandbox: &Path,
) {
    let same_agent = source.slug() == target.slug();
    let Some(vendor) = vendor_of(target.slug()) else {
        report.finding(&format!(
            "{} is on the structured track but `compare::vendor_of` does not know its vendor, \
             so no crossing into it can be verified; add the arm",
            target.slug()
        ));
        return;
    };

    // `UNLIMITED`, explicitly, because this battery verifies the *conversion* and
    // not the budget policy. Same-agent conservation is only meaningful when
    // nothing is allowed to be trimmed: under any binding cap a long session
    // would legitimately lose its oldest turns, and the check could no longer
    // tell that from a writer dropping them by mistake. It is also what a plain
    // `resume` now passes — the caps are opt-in — so this is the ordinary path
    // and not a special case. The budget has its own tests, including that
    // `UNLIMITED` writes byte-identical output.
    let written = match target.write_session_ir(ir, &WriteOptions { force: false }, &ContextBudget::UNLIMITED) {
        Ok(Some(written)) => written,
        Ok(None) => {
            report.finding(&format!(
                "{}: {} declined to write a session whose replay is not empty; \
                 `write_session_ir` may only return `Ok(None)` when there is nothing to write",
                path.display(),
                target.slug()
            ));
            return;
        }
        Err(error) => {
            report.finding(&format!(
                "{}: {} failed to write: {error}",
                path.display(),
                target.slug()
            ));
            return;
        }
    };

    // A corpus is read-only. If the caller's sandbox is not in force, stop the
    // whole run rather than keep writing into somebody's real session store.
    for produced in &written.written.paths {
        assert!(
            produced.starts_with(sandbox),
            "{} wrote {} outside the conformance sandbox {}. The provider's session root is \
             not redirected — point `HOME` at the sandbox and clear any provider-specific \
             home override before running the battery.",
            target.slug(),
            produced.display(),
            sandbox.display()
        );
    }

    let Some(produced) = written.written.paths.first() else {
        report.finding(&format!(
            "{}: {} reported a successful write with no paths",
            path.display(),
            target.slug()
        ));
        return;
    };
    let read_back = target.read_session_ir(produced);
    // The corpus is 3.5 GB and every source session is written once per target,
    // so the output goes as soon as it has been read. A sandbox that grows to
    // twice the corpus is a suite nobody runs.
    for produced in &written.written.paths {
        let _ = std::fs::remove_file(produced);
    }
    let back = match read_back {
        Ok(Some(back)) => back,
        Ok(None) | Err(_) => {
            report.finding(&format!(
                "{}: {} produced a session its own structured reader will not read back",
                path.display(),
                target.slug()
            ));
            return;
        }
    };

    // The written session has to satisfy the same replay invariants as a native
    // one: it is about to be resumed by the real agent.
    let back_plan = resolve(&back);
    let back_closure = invariants(report, produced, &back, &back_plan);
    report.written_checked += 1;
    report.written_closed += back_closure.closed;

    let comparison = compare(ir, &back, vendor);
    let claimed = written.fidelity;
    let derived = comparison.fidelity();
    let key = (source.slug().to_string(), target.slug().to_string());
    let tally = report.crossings.entry(key).or_default();
    tally.add(&comparison, claimed, &written.losses, source_kinds, &kinds_of(&back, &back_plan));

    // The comparator's own two findings, in both directions.
    if !comparison.unexplained.is_empty() {
        report.pending.push(format!(
            "{} -> {} {}: content is missing that nothing predicted: {}",
            source.slug(),
            target.slug(),
            path.display(),
            comparison.damage_detail()
        ));
    }
    if !comparison.carried_foreign.is_empty() {
        report.pending.push(format!(
            "{} -> {} {}: sealed material crossed a boundary `Capsule::fits` forbade: {}",
            source.slug(),
            target.slug(),
            path.display(),
            comparison.damage_detail()
        ));
    }
    // Both directions, because a disagreement is a disagreement. `derived >
    // claimed` alone — the writer claiming better than its own output supports —
    // let the opposite through untouched, and the opposite is not benign: it is
    // the comparator grading a loss more kindly than the writer that performed
    // it, which is the comparator losing the ability to see that loss at all.
    // Readable reasoning reshaped into assistant text was exactly that, graded
    // `ConversationOnly` by the writer and `ContextNoReasoning` here.
    //
    // One asymmetry is intentional and it is enumerated rather than tolerated:
    // `ByteIdentical` and `NativeEquivalent` are claims about the *bytes*, and
    // this comparator only ever compares structure — `Comparison::fidelity`
    // folds from `ContextComplete` and cannot reach either. A writer making one
    // of those two claims is making a claim this check has no evidence about.
    if claimed >= Fidelity::ContextComplete && derived != claimed {
        report.pending.push(format!(
            "{} -> {} {}: graded {claimed:?}, but the written file independently supports \
             {derived:?} — the writer's loss list and the comparator disagree about what the \
             crossing cost, and one of the two has stopped seeing something",
            source.slug(),
            target.slug(),
            path.display()
        ));
    }

    // The grade is derived from the loss list in both writers. Verified, not
    // assumed: a writer that accumulates a grade beside its losses can report
    // one rung better than its own evidence, which is exactly what
    // `codex_ir_write::summarise` used to do on 126 corpus sessions.
    let folded = written
        .losses
        .iter()
        .fold(Fidelity::ContextComplete, |worst, loss| {
            worst.worse_of(loss.grade)
        });
    if folded != claimed {
        report.pending.push(format!(
            "{} -> {} {}: claimed {claimed:?}, but the worst grade in its own loss list is \
             {folded:?}; the grade must be derived from the losses, not accumulated beside them",
            source.slug(),
            target.slug(),
            path.display()
        ));
    }

    if same_agent {
        if claimed > Fidelity::ContextComplete {
            report.pending.push(format!(
                "{} -> itself {}: graded {claimed:?}; a same-agent write loses nothing and must \
                 claim ContextComplete or better",
                source.slug(),
                path.display()
            ));
        }
        if !written.losses.is_empty() {
            report.pending.push(format!(
                "{} -> itself {}: reported {} loss(es) on a same-agent write: {}",
                source.slug(),
                path.display(),
                written.losses.len(),
                written.losses.first().map(|l| l.note.as_str()).unwrap_or("")
            ));
        }
        if comparison.source_events != comparison.target_events
            || comparison.source_capsules != comparison.target_capsules
            || comparison.added_events != 0
        {
            report.pending.push(format!(
                "{} -> itself {}: {} model events and {} capsules went in, {} and {} came out, \
                 with {} event(s) invented",
                source.slug(),
                path.display(),
                comparison.source_events,
                comparison.source_capsules,
                comparison.target_events,
                comparison.target_capsules,
                comparison.added_events
            ));
        }
        if !comparison.predicted.is_empty() || !comparison.degraded.is_empty() {
            report.pending.push(format!(
                "{} -> itself {}: nothing crosses a vendor boundary and no shape needs \
                 reshaping same-agent, yet {} predicted and {} degraded loss(es) were reported",
                source.slug(),
                path.display(),
                comparison.predicted.len(),
                comparison.degraded.len()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// One crossing's totals, over every session in the tier.
#[derive(Debug, Clone, Default)]
struct CrossTally {
    sessions: usize,
    source_events: usize,
    target_events: usize,
    added_events: usize,
    source_capsules: usize,
    target_capsules: usize,
    predicted: BTreeMap<String, usize>,
    degraded: BTreeMap<String, usize>,
    unexplained: BTreeMap<String, usize>,
    carried_foreign: usize,
    claimed: BTreeMap<String, usize>,
    derived: BTreeMap<String, usize>,
    loss_events: usize,
    sessions_without_losses: usize,
    kinds_before: BTreeMap<String, usize>,
    kinds_after: BTreeMap<String, usize>,
}

impl CrossTally {
    fn add(
        &mut self,
        comparison: &Comparison,
        claimed: Fidelity,
        losses: &[Loss],
        source_kinds: &BTreeMap<String, usize>,
        target_kinds: &BTreeMap<String, usize>,
    ) {
        self.sessions += 1;
        self.source_events += comparison.source_events;
        self.target_events += comparison.target_events;
        self.added_events += comparison.added_events;
        self.source_capsules += comparison.source_capsules;
        self.target_capsules += comparison.target_capsules;
        for (bucket, losses) in [
            (&mut self.predicted, &comparison.predicted),
            (&mut self.degraded, &comparison.degraded),
            (&mut self.unexplained, &comparison.unexplained),
        ] {
            for loss in losses {
                *bucket.entry(format!("{:?}", loss.kind)).or_insert(0) += loss.events;
            }
        }
        self.carried_foreign += comparison
            .carried_foreign
            .iter()
            .map(|carry| carry.capsules)
            .sum::<usize>();
        *self.claimed.entry(format!("{claimed:?}")).or_insert(0) += 1;
        *self
            .derived
            .entry(format!("{:?}", comparison.fidelity()))
            .or_insert(0) += 1;
        self.loss_events += losses.iter().map(|loss| loss.events).sum::<usize>();
        if losses.is_empty() {
            self.sessions_without_losses += 1;
        }
        for (kinds, into) in [
            (source_kinds, &mut self.kinds_before),
            (target_kinds, &mut self.kinds_after),
        ] {
            for (kind, count) in kinds {
                *into.entry(kind.clone()).or_insert(0) += count;
            }
        }
    }
}

/// One source provider's replay accounting, over every session in the tier.
#[derive(Debug, Clone, Copy, Default)]
struct ResolveTally {
    counts: Closure,
}

impl ResolveTally {
    fn add(&mut self, closure: &Closure) {
        let into = &mut self.counts;
        into.sessions += closure.sessions;
        into.captured += closure.captured;
        into.replayed += closure.replayed;
        into.superseded += closure.superseded;
        into.rolled_back += closure.rolled_back;
        into.fork += closure.fork;
        into.unclassified += closure.unclassified;
        into.chrome += closure.chrome;
        into.markers += closure.markers;
        into.checkpoints += closure.checkpoints;
        into.closed += closure.closed;
        into.live_head += closure.live_head;
        into.duplicate_ids += closure.duplicate_ids;
    }
}

/// Everything the battery measured, and everything it objects to.
///
/// Objections are [`Report::findings`] and are what a caller asserts on. The
/// counts are printed by [`Report::print`] whether or not anything objected,
/// because five separate silent defects on this codebase were found by counting
/// and none by a failing assertion.
pub struct Report {
    tier: String,
    files: usize,
    structured: Vec<String>,
    claimed: BTreeMap<String, usize>,
    missing_readers: BTreeSet<String>,
    unattributed_examples: Vec<PathBuf>,
    unattributed_count: usize,
    empty_replays: usize,
    probe_errors: usize,
    written_checked: usize,
    written_closed: usize,
    resolves: BTreeMap<String, ResolveTally>,
    crossings: BTreeMap<(String, String), CrossTally>,
    /// Per-session objections, collected before the aggregate ones.
    pending: Vec<String>,
    findings: Vec<String>,
    suppressed: usize,
}

/// Whether any loss this crossing reported could be about `kind`.
///
/// Keyed on [`Body::kind`]'s slug rather than on [`Body`] itself, because
/// that is what the tallies are keyed on. The mapping is the one
/// `compare::loss_kind` draws, and a slug this function has never been
/// taught maps to nothing: an unrecognised body kind that reaches none of
/// the written sessions is reported rather than excused, which is the same
/// default the crate's no-wildcard rule exists to force.
fn explains(kind: &str, tally: &CrossTally) -> bool {
    let accepted: &[&str] = match kind {
        "message" => &["Conversation", "Media", "Metadata"],
        "reasoning" => &["Reasoning"],
        "tool_call" | "tool_result" => &["ToolProtocol"],
        "sealed_context" => &["SealedContext"],
        "compaction" | "turn_config" | "env_snapshot" | "attachment" | "rollback" | "abort"
        | "control" | "unknown" => &["Metadata"],
        _ => &[],
    };
    [&tally.predicted, &tally.degraded, &tally.unexplained]
        .into_iter()
        .flat_map(|bucket| bucket.keys())
        .any(|reported| accepted.contains(&reported.as_str()))
}

impl Report {
    fn new(tier: &str, files: usize, providers: &[&dyn Provider]) -> Self {
        Self {
            tier: tier.to_string(),
            files,
            structured: providers
                .iter()
                .map(|provider| provider.slug().to_string())
                .collect(),
            claimed: BTreeMap::new(),
            missing_readers: BTreeSet::new(),
            unattributed_examples: Vec::new(),
            unattributed_count: 0,
            empty_replays: 0,
            probe_errors: 0,
            written_checked: 0,
            written_closed: 0,
            resolves: BTreeMap::new(),
            crossings: BTreeMap::new(),
            pending: Vec::new(),
            findings: Vec::new(),
            suppressed: 0,
        }
    }

    /// One objection, capped so that a single systematic break does not bury
    /// the aggregate findings under one line per corpus session.
    fn finding(&mut self, note: &str) {
        const CAP: usize = 64;
        if self.findings.len() < CAP {
            self.findings.push(note.to_string());
        } else {
            self.suppressed += 1;
        }
    }

    fn claimed(&mut self, slug: &str) {
        *self.claimed.entry(slug.to_string()).or_insert(0) += 1;
    }

    fn missing_reader(&mut self, slug: &str) {
        self.missing_readers.insert(slug.to_string());
    }

    fn unattributed(&mut self, path: &Path) {
        self.unattributed_count += 1;
        if self.unattributed_examples.len() < EXAMPLES {
            self.unattributed_examples.push(path.to_path_buf());
        }
    }

    fn resolve_tally(&mut self, slug: &str) -> &mut ResolveTally {
        self.resolves.entry(slug.to_string()).or_default()
    }

    /// Fold the per-session objections in, then add the aggregate ones — the
    /// checks that can only be made once every session has been seen.
    fn finish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let total = pending.len();
        for note in pending.iter().take(EXAMPLES) {
            self.finding(note);
        }
        if total > EXAMPLES {
            self.finding(&format!(
                "… and {} further per-session objection(s)",
                total - EXAMPLES
            ));
        }

        for slug in self.missing_readers.clone() {
            self.finding(&format!(
                "{slug} answers `supports_structured_write` but its `read_session_ir` returned \
                 `Ok(None)`; a provider on the structured track needs both halves or its own \
                 output can never be verified"
            ));
        }

        for slug in self.structured.clone() {
            if self.claimed.get(&slug).copied().unwrap_or(0) == 0 {
                self.finding(&format!(
                    "no session in the {} tier was claimed by {slug}, so every check below ran \
                     on somebody else's sessions; a provider on the structured track has to \
                     bring at least one session its own reader parses",
                    self.tier
                ));
            }
        }

        let crossings = self.crossings.clone();
        for ((from, to), tally) in &crossings {
            let same_agent = from == to;
            if tally.sessions == 0 {
                continue;
            }

            // Two-sided, and it activates on measurement rather than on a
            // declaration: only a source that actually carried sealed material
            // can say anything about whether the boundary held.
            if tally.source_capsules > 0 {
                if same_agent && tally.target_capsules != tally.source_capsules {
                    self.finding(&format!(
                        "{from} -> {to}: {} capsule(s) went in and {} came out; a same-vendor \
                         capsule must survive verbatim",
                        tally.source_capsules, tally.target_capsules
                    ));
                }
                if !same_agent {
                    if tally.target_capsules != 0 {
                        self.finding(&format!(
                            "{from} -> {to}: {} capsule(s) reached a session whose provider \
                             cannot read them",
                            tally.target_capsules
                        ));
                    }
                    if tally.predicted.is_empty() {
                        self.finding(&format!(
                            "{from} -> {to}: {} capsule(s) crossed a vendor boundary and not one \
                             was reported as predicted-not-carried; the classification the \
                             comparator exists to draw is no longer being exercised",
                            tally.source_capsules
                        ));
                    }
                }
            }

            // Body coverage closure. Same-agent, a variant the reader emits and
            // the writer does not reproduce is the hole this check exists for,
            // and naming the variant is the difference between a fixable report
            // and an event-count mismatch.
            for (kind, before) in &tally.kinds_before {
                let after = tally.kinds_after.get(kind).copied().unwrap_or(0);
                if same_agent && after != *before {
                    self.finding(&format!(
                        "{from} -> {to}: `Body::{kind}` appears {before} time(s) in the replays \
                         read and {after} time(s) in the replays written back; a variant its own \
                         writer does not reproduce is a hole in every conversion"
                    ));
                }
                // Cross-agent, the same closure with the allowance the boundary
                // really buys. The guard used to be `predicted.is_empty() &&
                // degraded.is_empty()`, and both are tier-wide totals over the
                // whole crossing — so one predicted reasoning capsule anywhere
                // in the corpus switched the check off for *every* body kind,
                // and a variant a writer drops whole could never be reported.
                // A loss only explains the variant it could actually be about.
                if !same_agent && *before > 0 && after == 0 && !explains(kind, tally) {
                    self.finding(&format!(
                        "{from} -> {to}: `Body::{kind}` appears {before} time(s) in the replays \
                         read and never in the written sessions, and no loss of a kind that \
                         could describe it was reported"
                    ));
                }
            }

            // "The comparator found losses the writer's own loss list does not
            // mention" used to be checked here as
            // `loss_events == 0 && !predicted.is_empty()`. Both operands are
            // tier-wide totals over every session in the crossing, and any real
            // corpus makes `loss_events` five figures — codex -> claude-code
            // reports 49,701 — so the condition was false on every run this
            // suite has ever had. It is gone rather than commented, because a
            // check nobody can trigger reads as coverage.
            //
            // What replaces it is stronger and per-session: `cross` requires the
            // writer's claimed grade and the comparator's derived grade to be
            // *equal*, in both directions. A writer whose loss list is empty
            // claims `ContextComplete`, so a comparator that found anything at
            // all now disagrees with it, on the session, by name.
        }
    }

    /// Objections. Empty means every check in the battery passed.
    pub fn findings(&self) -> &[String] {
        &self.findings
    }

    /// Sessions a structured reader claimed and the battery therefore checked.
    pub fn sessions(&self) -> usize {
        self.claimed.values().sum()
    }

    /// Every count the battery took, on stderr, per provider.
    ///
    /// `eprintln!` rather than `println!` so the tallies survive a passing test
    /// under `--nocapture` and appear next to a failing one.
    pub fn print(&self) {
        eprintln!(
            "\n════ conformance tier {:?}: {} file(s) offered, {} session(s) checked",
            self.tier,
            self.files,
            self.sessions()
        );
        eprintln!(
            "  structured providers: {}",
            if self.structured.is_empty() {
                "none".to_string()
            } else {
                self.structured.join(", ")
            }
        );
        eprintln!(
            "  attribution: {:?}; {} unattributed, {} empty replay(s), {} read error(s)",
            self.claimed, self.unattributed_count, self.empty_replays, self.probe_errors
        );
        for path in &self.unattributed_examples {
            eprintln!("    unattributed e.g. {}", path.display());
        }
        eprintln!(
            "  written sessions re-resolved: {} of which {} closed exactly",
            self.written_checked, self.written_closed
        );

        for (slug, tally) in &self.resolves {
            let counts = &tally.counts;
            eprintln!("\n  ── {slug} as source: {} session(s)", counts.sessions);
            eprintln!(
                "     captured {} = replayed {} + superseded {} + rolled back {} + fork {} \
                 + unclassified {} + chrome {} + markers {}",
                counts.captured,
                counts.replayed,
                counts.superseded,
                counts.rolled_back,
                counts.fork,
                counts.unclassified,
                counts.chrome,
                counts.markers
            );
            eprintln!(
                "     closed exactly {}/{}   checkpoints {}   live_head named {}   \
                 duplicate ids {}",
                counts.closed,
                counts.sessions,
                counts.checkpoints,
                counts.live_head,
                counts.duplicate_ids
            );
        }

        for ((from, to), tally) in &self.crossings {
            eprintln!(
                "\n  ── {from} -> {to}: {} session(s){}",
                tally.sessions,
                if from == to { "  [same agent]" } else { "" }
            );
            eprintln!(
                "     model events {} -> {} (+{} invented)   capsules {} -> {}",
                tally.source_events,
                tally.target_events,
                tally.added_events,
                tally.source_capsules,
                tally.target_capsules
            );
            for (name, bucket) in [
                ("predicted", &tally.predicted),
                ("degraded", &tally.degraded),
                ("UNEXPLAINED", &tally.unexplained),
            ] {
                if bucket.is_empty() {
                    eprintln!("     {name:<12} none");
                } else {
                    eprintln!("     {name:<12} {bucket:?}");
                }
            }
            eprintln!(
                "     foreign capsules carried across: {}   writer loss events: {}   \
                 sessions with no loss: {}",
                tally.carried_foreign, tally.loss_events, tally.sessions_without_losses
            );
            eprintln!("     grade claimed  {:?}", tally.claimed);
            eprintln!("     grade supported {:?}", tally.derived);
            let mut kinds: Vec<&String> = tally
                .kinds_before
                .keys()
                .chain(tally.kinds_after.keys())
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            for kind in kinds {
                eprintln!(
                    "     body {kind:<16} {:>8} -> {:>8}",
                    tally.kinds_before.get(kind).copied().unwrap_or(0),
                    tally.kinds_after.get(kind).copied().unwrap_or(0)
                );
            }
        }

        if self.findings.is_empty() {
            eprintln!("\n  findings: none\n");
        } else {
            eprintln!("\n  findings ({}):", self.findings.len());
            for finding in &self.findings {
                eprintln!("    ✗ {finding}");
            }
            if self.suppressed > 0 {
                eprintln!("    ✗ … and {} more, suppressed", self.suppressed);
            }
            eprintln!();
        }
    }
}

// ---------------------------------------------------------------------------
// Synthesising the work an agent does in an intermediate session
// ---------------------------------------------------------------------------

/// Append `turns` synthesised user/assistant exchanges to a session `provider`
/// owns, each carrying `marker` in its text, and report the bytes added.
///
/// # Why this exists
///
/// A chained measurement with nothing appended to the intermediate is degenerate:
/// the intermediate is then a lossy projection of the origin and *returning the
/// origin is trivially correct*, which validates the mechanism on the one case
/// where the mechanism has no value — the user already had the origin. The case
/// that matters is the one where the user worked in the intermediate, and that
/// case only exists if something appends to it.
///
/// # Why appended and not rewritten
///
/// The store detects that a session moved on by growth: `(size, mtime)` first,
/// then the recorded prefix hash as the confirming read. A synthesised turn has
/// to leave that prefix alone or it reads as *diverged* rather than *advanced* —
/// which is also what a real agent does, since both structured session formats
/// are append-only JSONL logs.
///
/// # Why the shapes are named rather than derived
///
/// The turn is cloned from a line already in the file — so `cwd`, `sessionId`,
/// `version` and the rest come from the session itself — and only the identity
/// and the text are patched. What cannot be cloned is *where the text lives*,
/// which is per-format. A provider this function has never been taught is an
/// error rather than a silent no-op, because a chain measured without an append
/// is exactly the degenerate chain above.
pub fn append_turns(
    path: &Path,
    provider: &str,
    marker: &str,
    turns: usize,
) -> anyhow::Result<u64> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    let shape = TurnShape::of(provider).ok_or_else(|| {
        anyhow::anyhow!(
            "no synthesised turn shape for '{provider}', so a chain through it would measure the \
             degenerate case where nothing was appended"
        )
    })?;
    let user = shape.template(&lines, "user").ok_or_else(|| {
        anyhow::anyhow!("{} holds no {provider} user turn to clone", path.display())
    })?;
    let assistant = shape.template(&lines, "assistant").ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds no {provider} assistant turn to clone",
            path.display()
        )
    })?;

    let mut parent = lines
        .iter()
        .rev()
        .find_map(|line| line.get("uuid").and_then(|id| id.as_str()))
        .map(str::to_string);
    let mut appended = String::new();
    for turn in 0..turns {
        for (role, template) in [("user", &user), ("assistant", &assistant)] {
            let mut line = template.clone();
            let uuid = uuid::Uuid::new_v4().as_hyphenated().to_string();
            shape.patch(
                &mut line,
                role,
                &format!("{marker} — appended {role} turn {}", turn + 1),
                &uuid,
                parent.as_deref(),
            );
            parent = Some(uuid);
            appended.push_str(&serde_json::to_string(&line)?);
            appended.push('\n');
        }
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(appended.as_bytes())?;
    file.flush()?;
    Ok(appended.len() as u64)
}

/// Where one native session format keeps a turn's role, text and identity.
///
/// Two variants because two providers write structured sessions. A third would
/// add a variant here and be covered by everything that calls
/// [`append_turns`] — the same shape as the rest of this module, where the list
/// under test is the registry and not a list.
#[derive(Debug, Clone, Copy)]
enum TurnShape {
    /// `{"type": "user"|"assistant", "message": {...}, "uuid": …, "parentUuid": …}`
    ClaudeCode,
    /// `{"type": "response_item", "payload": {"role": …, "content": [{"text": …}]}}`,
    /// or a user turn as `{"type": "event_msg", "payload": {"type":
    /// "user_message", "message": …}}` — the rollout writer emits both shapes
    /// depending on the history mode, and a template has to accept whichever the
    /// file actually holds.
    Codex,
}

impl TurnShape {
    fn of(provider: &str) -> Option<Self> {
        match provider {
            "claude-code" => Some(TurnShape::ClaudeCode),
            "codex" => Some(TurnShape::Codex),
            _ => None,
        }
    }

    /// The last line in the file that carries a turn in `role`, to clone.
    fn template(&self, lines: &[serde_json::Value], role: &str) -> Option<serde_json::Value> {
        let matches = |line: &&serde_json::Value| match self {
            TurnShape::ClaudeCode => line.get("type").and_then(|t| t.as_str()) == Some(role),
            TurnShape::Codex => {
                let kind = line.get("type").and_then(|t| t.as_str());
                let response_item = kind == Some("response_item")
                    && line.pointer("/payload/role").and_then(|r| r.as_str()) == Some(role)
                    && line.pointer("/payload/content/0/text").is_some();
                let event_msg = role == "user"
                    && kind == Some("event_msg")
                    && line.pointer("/payload/type").and_then(|t| t.as_str())
                        == Some("user_message")
                    && line.pointer("/payload/message").is_some();
                response_item || event_msg
            }
        };
        lines.iter().rev().find(matches).cloned()
    }

    /// Give the cloned line a fresh identity and `text` as its only content.
    fn patch(
        &self,
        line: &mut serde_json::Value,
        role: &str,
        text: &str,
        uuid: &str,
        parent: Option<&str>,
    ) {
        match self {
            TurnShape::ClaudeCode => {
                line["uuid"] = serde_json::Value::String(uuid.to_string());
                line["parentUuid"] = match parent {
                    Some(parent) => serde_json::Value::String(parent.to_string()),
                    None => serde_json::Value::Null,
                };
                if role == "assistant" {
                    line["message"]["content"] =
                        serde_json::json!([{ "type": "text", "text": text }]);
                    line["message"]["id"] = serde_json::Value::String(format!(
                        "msg_casr_{}",
                        uuid::Uuid::new_v4().as_simple()
                    ));
                } else {
                    line["message"]["content"] = serde_json::Value::String(text.to_string());
                }
            }
            TurnShape::Codex => {
                // Whichever slot the cloned line actually uses; see the variant.
                if line.pointer("/payload/message").is_some() {
                    line["payload"]["message"] = serde_json::Value::String(text.to_string());
                } else {
                    line["payload"]["content"][0]["text"] =
                        serde_json::Value::String(text.to_string());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The second hop
// ---------------------------------------------------------------------------

/// Measure what a second conversion hop costs, with the store and without it.
///
/// This is a sibling of [`run`] rather than a check inside it, and the split is
/// deliberate. [`run`]'s subject is one provider's writer against its own
/// reader, per file per target; it needs no store, no pipeline and no discovery.
/// The second hop's subject is the pipeline's *source selection*: it needs a
/// writable store root, it drives [`crate::pipeline::ConversionPipeline`] end to
/// end, and its unit is a chain of two conversions rather than one crossing.
/// Folding it into [`Report`] would have made that type a grab bag of two
/// unrelated measurements.
///
/// # What it measures
///
/// For each source session claimed by structured provider `A`, and each other
/// structured provider `B`:
///
/// 1. `A → B` with a store, so the store learns that both sessions are the same
///    conversation.
/// 2. **The untouched arm.** `B → A` naming the session step 1 wrote, with the
///    store and again without it.
/// 3. [`append_turns`] puts synthesised work into that intermediate.
/// 4. **The appended arm.** `B → A` again, both ways.
///
/// Then it counts the capsules in the session each of those hops actually
/// delivers: the file that was written, or — when the store chose a source that
/// was already in `A`'s format and so needed no conversion at all — the file the
/// resume command points at. Both are "the session the user ends up in", which
/// is the only number the user experiences.
///
/// The ceiling is the source session's own capsule count, and it is reported
/// beside the two results rather than assumed: a hop that recovers everything
/// and a hop that recovers nothing look identical without it.
///
/// # Why there are two arms
///
/// Because one of them measures nothing. With nothing appended to the
/// intermediate, the intermediate is a lossy projection of the origin and
/// returning the origin is *trivially* correct — and trivially worthless, since
/// the user already had the origin. That arm is kept because it is the store's
/// payoff and losing it would be a silent regression, but on its own it validated
/// the mechanism on exactly the case where the mechanism has no value, and it is
/// the reason a real defect went unmeasured: for two hours of work appended to the
/// intermediate the same ranking returned the origin, wrote nothing, and handed
/// back the file the user already had.
///
/// # What it asserts, and what it deliberately no longer asserts
///
/// It used to assert that consulting the store never delivers less sealed
/// material than not consulting it. That is **false** once the ranking is right:
/// correctly preferring an advanced derivative over an older-but-richer origin
/// delivers *fewer* capsules on purpose. The property that is both true and wanted
/// is narrower and stronger:
///
/// > **The store may never deliver an outcome the user would not have got without
/// > it.** `--no-store` is the baseline for both halves of what is delivered,
/// > because without a store the user gets exactly the session they named,
/// > converted.
/// >
/// > - **Conversation content is a floor, never a trade.** Anything the
/// >   `--no-store` arm delivered must be in the store arm's session too. Turns
/// >   exist in one incarnation only and no conversion can rebuild them, so losing
/// >   one is unrecoverable and never justified by anything.
/// > - **Sealed material is a floor unless it is bought.** The store arm may
/// >   deliver fewer capsules than the `--no-store` arm only where it delivered
/// >   content the `--no-store` arm did not. Capsules a derivative lacks are
/// >   content its origin still holds; that makes them recoverable, and
/// >   recoverable loss may be traded for unrecoverable loss avoided.
/// >
/// > And where nothing has advanced at all — the untouched arm — neither clause
/// > can bite, so the old floor still holds there and is still asserted: the store
/// > delivers at least what `--no-store` does.
///
/// `sandbox` carries the same contract as [`run`]'s: every provider session root
/// must already be redirected into it, every written path is asserted to land
/// inside it, and the store roots this creates are inside it too.
pub fn second_hop(tier: &str, files: &[PathBuf], sandbox: &Path) -> HopReport {
    let registry = ProviderRegistry::default_registry();
    let structured: Vec<String> = registry
        .all_providers()
        .into_iter()
        .filter(|provider| provider.supports_structured_write())
        .map(|provider| provider.slug().to_string())
        .collect();

    let mut report = HopReport {
        tier: tier.to_string(),
        files: files.len(),
        ..HopReport::default()
    };
    // No store, ever. Built once: it is the control arm for every chain below,
    // and `None` is the whole of what `--no-store` gives the pipeline.
    let control = ConversionPipeline {
        registry: ProviderRegistry::default_registry(),
        store: None,
    };

    for (index, path) in files.iter().enumerate() {
        let Some((source_slug, source_capsules)) = claimant(&registry, &structured, path) else {
            continue;
        };
        for target_slug in &structured {
            if target_slug == &source_slug {
                continue;
            }
            chain(
                &mut report,
                &control,
                path,
                &source_slug,
                target_slug,
                source_capsules,
                &sandbox.join(format!("hop-store-{index}-{target_slug}")),
                sandbox,
            );
        }
    }
    report
}

/// Which structured provider's reader claims `path`, and how many capsules its
/// model-visible replay holds.
fn claimant(
    registry: &ProviderRegistry,
    structured: &[String],
    path: &Path,
) -> Option<(String, usize)> {
    for slug in structured {
        let provider = registry.find_by_slug(slug)?;
        if let Ok(Some(ir)) = provider.read_session_ir(path)
            && !resolve(&ir).events.is_empty()
        {
            return Some((slug.clone(), capsules_of(&ir)));
        }
    }
    None
}

/// Capsules on the events the model would be shown.
///
/// Over [`SessionIr::model_visible`] and not every captured event, for the same
/// reason [`crate::store`] counts it that way: a capsule on an event no replay
/// includes is never handed to the target and is worth nothing to it.
fn capsules_of(ir: &SessionIr) -> usize {
    ir.model_visible()
        .iter()
        .map(|event| event.capsules.len())
        .sum()
}

/// The stem of the text a synthesised turn carries; a fresh uuid is appended per
/// chain.
///
/// "The work survived" is then a substring test on the bytes the chain delivered,
/// which is decidable. An event count is not: two formats legitimately split one
/// native line into a different number of events, so a count that went down could
/// always be explained away as structural — which is how content loss stays
/// invisible.
///
/// The uuid is not decoration. This suite runs against a live corpus of the
/// author's own sessions, and one of those sessions is the one in which this
/// constant was written — a fixed marker is a string the corpus already contains,
/// so "the delivered session holds the appended work" would sometimes be true of
/// a session nothing was appended to.
const APPENDED_MARKER: &str = "agsx-second-hop-appended-work";

/// Synthesised user/assistant exchanges appended to each intermediate.
///
/// Three is enough to be found again and small enough that the corpus tier stays
/// a suite someone runs: this appends to, re-reads and re-converts every
/// intermediate in the corpus.
const APPENDED_TURNS: usize = 3;

/// One `A → B → A` chain, measured with and without the store, before and after
/// work is appended to the intermediate.
#[allow(clippy::too_many_arguments)]
fn chain(
    report: &mut HopReport,
    control: &ConversionPipeline,
    path: &Path,
    source_slug: &str,
    target_slug: &str,
    source_capsules: usize,
    store_root: &Path,
    sandbox: &Path,
) {
    let store = match Store::open_at(store_root) {
        Ok(store) => store,
        Err(error) => {
            report.findings.push(format!(
                "could not create a store under the sandbox: {error}"
            ));
            return;
        }
    };
    let stored = ConversionPipeline {
        registry: ProviderRegistry::default_registry(),
        store: Some(store),
    };

    // Unlimited, and not the CLI's defaults, for the reason `cross` gives: this
    // measures the conversion chain and not the budget policy. A budget that
    // legitimately trims the oldest turns would take capsules with it, and the
    // check could no longer tell that from a hop asking the wrong source.
    let opts = |hint: &Path| ConvertOptions {
        source_hint: Some(hint.display().to_string()),
        ..ConvertOptions::default()
    };
    let named = |path: &Path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // Hop one: out of the provider that owns the session.
    let first = match stored.convert(target_slug, &named(path), opts(path)) {
        Ok(result) => result,
        Err(error) => {
            let message = format!(
                "{}: {source_slug} -> {target_slug} did not convert, so no chain starts here: \
                 {error}",
                path.display()
            );
            if is_write_defect(&error) {
                report.finding(message);
            } else {
                report.note(message);
            }
            return;
        }
    };
    let Some(intermediate) = first
        .written
        .as_ref()
        .and_then(|w| w.paths.first())
        .cloned()
    else {
        report.note(format!(
            "{}: {source_slug} -> {target_slug} wrote no file, so there is no session for a \
             second hop to name",
            path.display()
        ));
        return;
    };
    assert_inside(sandbox, &intermediate, target_slug);

    // The untouched arm: nothing has moved since the store recorded either
    // session, which is the store's payoff and the degenerate case both.
    hop_two(
        report,
        control,
        &stored,
        path,
        source_slug,
        target_slug,
        source_capsules,
        &intermediate,
        sandbox,
        Arm::Untouched,
        None,
    );

    // Now the case that matters: the user worked in the intermediate. Appended
    // rather than rewritten, so the store sees growth and not divergence — which
    // is also what an agent working in an append-only log does.
    let marker = format!("{APPENDED_MARKER}-{}", uuid::Uuid::new_v4().as_hyphenated());
    match append_turns(&intermediate, target_slug, &marker, APPENDED_TURNS) {
        Ok(added) => {
            report
                .chains
                .entry((source_slug.to_string(), target_slug.to_string()))
                .or_default()
                .appended_bytes += added;
            hop_two(
                report,
                control,
                &stored,
                path,
                source_slug,
                target_slug,
                source_capsules,
                &intermediate,
                sandbox,
                Arm::Appended,
                Some(&marker),
            );
        }
        Err(error) => report.note(format!(
            "{}: nothing could be appended to the {target_slug} intermediate, so this chain only \
             measured the degenerate case: {error}",
            path.display()
        )),
    }

    let _ = std::fs::remove_file(&intermediate);
}

/// Whether a refused hop is this crate's bug rather than a fact about the input.
///
/// A hop can fail for two unrelated reasons and they deserve opposite
/// treatment. A session that is genuinely unusable — the corpus holds one-sided
/// transcripts, and `validate_session` is right to refuse them — is worth
/// counting but not worth failing. Everything else happens *after* a structured
/// reader has already claimed the file and the pipeline has committed to
/// converting it, and at that point a failure is this crate's.
///
/// # The whitelist is the passing side, and that is the whole point
///
/// This used to whitelist the *failing* side: `VerifyFailed` was a defect and
/// every other error was a printed note. That has the default backwards. A
/// `SessionWriteError` — the pipeline failing to write the file it had just
/// converted — is not a fact about the corpus by any reading, and it was being
/// filed as a note nobody asserts on. So was a `SessionReadError` on the
/// intermediate this suite wrote itself, and so was any error type added later.
/// Naming what may pass and failing the rest means a new failure mode arrives
/// loud.
///
/// Matched on the typed variant rather than on the message, so renaming the
/// error text cannot silently turn a failure back into a note.
fn is_write_defect(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<crate::error::CasrError>() {
        // The one refusal proven to describe the input rather than the crate:
        // `validate_session` found the session itself unusable and stopped
        // before writing anything.
        Some(crate::error::CasrError::ValidationError { .. }) => false,
        // Every other `CasrError`, and every error that is not one at all.
        Some(_) | None => true,
    }
}

/// Which of the two arms a measurement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Nothing appended to the intermediate.
    Untouched,
    /// [`APPENDED_TURNS`] synthesised exchanges appended to it.
    Appended,
}

/// `B → A` twice — with the store and without — measured and tallied.
#[allow(clippy::too_many_arguments)]
fn hop_two(
    report: &mut HopReport,
    control: &ConversionPipeline,
    stored: &ConversionPipeline,
    path: &Path,
    source_slug: &str,
    target_slug: &str,
    source_capsules: usize,
    intermediate: &Path,
    sandbox: &Path,
    arm: Arm,
    // The text the appended turns carry, when any were appended. `None` in the
    // untouched arm, where there is no appended work and so nothing to find.
    marker: Option<&str>,
) {
    // Unlimited, and not the CLI's defaults, for the reason `cross` gives: this
    // measures the conversion chain and not the budget policy. A budget that
    // legitimately trims the oldest turns would take capsules with it, and the
    // check could no longer tell that from a hop asking the wrong source.
    let opts = ConvertOptions {
        source_hint: Some(intermediate.display().to_string()),
        ..ConvertOptions::default()
    };
    let named = intermediate
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();

    // The store's arm runs first so that the control arm cannot be handed a store
    // the first arm warmed.
    let with = stored.convert(source_slug, &named, opts.clone());
    let without = control.convert(source_slug, &named, opts);

    let measure = |result: &Result<crate::pipeline::ConversionResult, anyhow::Error>| {
        let result = match result {
            Ok(result) => result,
            Err(_) => return None,
        };
        // The delivered session is the file that was written — or, when the
        // chosen source was already in the target's format and needed no
        // conversion, the file the resume command points back at.
        let delivered = result
            .written
            .as_ref()
            .and_then(|written| written.paths.first())
            .cloned()
            .or_else(|| {
                result
                    .source
                    .as_ref()
                    .and_then(|selection| selection.chosen())
                    .map(|chosen| chosen.path.clone())
            })?;
        if delivered.starts_with(sandbox) {
            assert_inside(sandbox, &delivered, source_slug);
        }
        let provider = control.registry.find_by_slug(source_slug)?;
        let ir = provider.read_session_ir(&delivered).ok().flatten()?;
        // Read as bytes, not as events: see `APPENDED_MARKER`.
        let held_work = marker.is_some_and(|marker| {
            std::fs::read_to_string(&delivered)
                .map(|text| text.contains(marker))
                .unwrap_or(false)
        });
        Some(Delivered {
            capsules: capsules_of(&ir),
            held_work,
        })
    };

    let with_measured = measure(&with);
    let without_measured = measure(&without);

    // Written output goes as soon as it has been read: every source session in
    // the corpus is converted four times here, and a sandbox that grows to
    // several times the corpus is a suite nobody runs.
    for produced in [&with, &without].into_iter().flatten() {
        for written in produced.written.iter().flat_map(|w| w.paths.iter()) {
            let _ = std::fs::remove_file(written);
        }
    }

    let chain = report
        .chains
        .entry((source_slug.to_string(), target_slug.to_string()))
        .or_default();
    let tally = match arm {
        Arm::Untouched => &mut chain.untouched,
        Arm::Appended => &mut chain.appended,
    };
    tally.sessions += 1;
    tally.source_capsules += source_capsules;

    if let (Ok(result), Some(measured)) = (&with, &with_measured) {
        tally.with_store += measured.capsules;
        if measured.held_work {
            tally.store_kept_work += 1;
        }
        if result
            .source
            .as_ref()
            .is_some_and(crate::pipeline::SourceSelection::overrode)
        {
            tally.overrode += 1;
        }
        if result.written.as_ref().is_some_and(|w| w.paths.is_empty()) {
            tally.needed_no_conversion += 1;
        }
    }
    if let Some(measured) = &without_measured {
        tally.without_store += measured.capsules;
        if measured.held_work {
            tally.control_kept_work += 1;
        }
    }

    // A hop the pipeline refuses splits by *cause*, and the split is the whole
    // of what this check is entitled to claim either way.
    //
    // A refusal because the session is unusable is a fact about the corpus — it
    // holds one-sided transcripts `validate_session` correctly refuses — and
    // stays a note. A refusal because the file the pipeline just wrote does not
    // read back as the session it converted is this crate's own bug, and has to
    // fail. `CasrError::VerifyFailed` is exactly that line and it is already
    // typed, so the split needs no string matching.
    //
    // Both used to be notes, and that is how a real writer defect hid here in
    // plain sight: `claude-code -> codex -> claude-code` lost a reasoning event
    // the writer never declared, printed among the notes for as long as the
    // check existed. What made it invisible was not the reporting level alone —
    // it was that the control arm is the code path predating the store, so an
    // objection raised there reads as a store failure. That argument justifies
    // not filing it *under the store's name*. It never justified not failing.
    for (label, measured, outcome) in [
        ("with a store", &with_measured, &with),
        ("without one", &without_measured, &without),
    ] {
        if measured.is_none() {
            let message = format!(
                "{}: {target_slug} -> {source_slug} ({arm:?} arm) {label} delivered nothing \
                 measurable{}",
                path.display(),
                outcome
                    .as_ref()
                    .err()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            );
            match outcome.as_ref().err() {
                Some(error) if is_write_defect(error) => report.finding(message),
                Some(_) | None => report.note(message),
            }
        }
    }

    // The objections, all of them stated against the control arm rather than
    // against a number: the number belongs to whatever corpus is on the machine
    // and the property belongs to the store.
    let chain_name = format!("{source_slug} -> {target_slug} -> {source_slug}");
    match (&with_measured, &without_measured) {
        (Some(with_measured), Some(without_measured)) => {
            // Content is a floor, never a trade.
            if without_measured.held_work && !with_measured.held_work {
                report.findings.push(format!(
                    "{}: {chain_name} ({arm:?} arm) delivered the work appended to the \
                     intermediate without the store and not with it, so consulting the store \
                     silently dropped conversation content that exists in one incarnation only",
                    path.display()
                ));
            }
            // Sealed material is a floor unless it is bought with content.
            if with_measured.capsules < without_measured.capsules
                && !(with_measured.held_work && !without_measured.held_work)
            {
                report.findings.push(format!(
                    "{}: {chain_name} ({arm:?} arm) delivered {} capsule(s) through the store \
                     against {} without it, while delivering no conversation content that \
                     `--no-store` did not; sealed material may only be given up to keep turns \
                     that exist nowhere else",
                    path.display(),
                    with_measured.capsules,
                    without_measured.capsules
                ));
            }
            // And where nothing has advanced the old floor still holds, because
            // neither clause above can bite: the origin is strictly better.
            if arm == Arm::Untouched && with_measured.capsules < without_measured.capsules {
                report.findings.push(format!(
                    "{}: {chain_name} (untouched arm) delivered {} capsule(s) through the store \
                     against {} without it; with nothing appended anywhere the store may not cost \
                     sealed material",
                    path.display(),
                    with_measured.capsules,
                    without_measured.capsules
                ));
            }
        }
        (None, Some(_)) => report.findings.push(format!(
            "{}: {chain_name} ({arm:?} arm) delivered a session without the store and none with \
             it, so consulting the store cost the whole conversion{}",
            path.display(),
            with.as_ref()
                .err()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )),
        (Some(_), None) | (None, None) => {}
    }
}

/// What one arm of one hop actually handed the user.
#[derive(Debug, Clone, Copy)]
struct Delivered {
    /// Capsules the target's vendor can replay.
    capsules: usize,
    /// Whether the work appended to the intermediate is in these bytes.
    held_work: bool,
}

fn assert_inside(sandbox: &Path, produced: &Path, slug: &str) {
    assert!(
        produced.starts_with(sandbox),
        "{slug} wrote {} outside the conformance sandbox {}. The provider's session root is not \
         redirected — point `HOME` at the sandbox and clear any provider-specific home override \
         before running the battery.",
        produced.display(),
        sandbox.display()
    );
}

/// One `A → B → A` chain's totals, over every session in the tier.
#[derive(Debug, Clone, Default)]
struct ChainTally {
    /// With nothing appended to the intermediate: the store's payoff, and the
    /// case where returning the origin is trivially correct.
    untouched: ArmTally,
    /// After work was appended to the intermediate: the case that matters.
    appended: ArmTally,
    /// Bytes of synthesised work appended across the tier.
    appended_bytes: u64,
}

/// One arm of one chain's totals.
#[derive(Debug, Clone, Default)]
struct ArmTally {
    sessions: usize,
    /// Capsules in the source sessions, which is the ceiling for both halves.
    source_capsules: usize,
    /// Capsules the chain delivered with the store consulted.
    with_store: usize,
    /// Capsules it delivered without one.
    without_store: usize,
    /// Times the store read a session other than the one the hop named.
    overrode: usize,
    /// Times the source it chose was already in the target's format, so the
    /// second hop wrote nothing and pointed at bytes that were already there.
    needed_no_conversion: usize,
    /// Times the store-backed chain delivered the work appended to the
    /// intermediate. Zero by construction in the untouched arm.
    store_kept_work: usize,
    /// Times the `--no-store` chain did, which is the floor the store arm has to
    /// meet.
    control_kept_work: usize,
}

/// One arm's totals across every chain, as a caller asserts on them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArmTotals {
    pub sessions: usize,
    /// Capsules in the source sessions: the ceiling for both halves.
    pub source_capsules: usize,
    pub with_store: usize,
    pub without_store: usize,
    /// Chains whose store-backed hop delivered the appended work.
    pub store_kept_work: usize,
    /// Chains whose `--no-store` hop did.
    pub control_kept_work: usize,
}

/// What the second hop measured, and everything it objects to.
#[derive(Debug, Default)]
pub struct HopReport {
    tier: String,
    files: usize,
    chains: BTreeMap<(String, String), ChainTally>,
    /// Hops refused for a reason that is a fact about the input rather than a
    /// defect in this crate, with the first few reasons. Printed, never
    /// asserted on. A refusal that *is* a defect goes to `findings` instead —
    /// see `is_write_defect` and the comment at the call site.
    notes: Vec<String>,
    note_count: usize,
    findings: Vec<String>,
}

impl HopReport {
    fn note(&mut self, note: String) {
        self.note_count += 1;
        if self.notes.len() < EXAMPLES {
            self.notes.push(note);
        }
    }

    /// Something that has to fail the build, as opposed to a [`HopReport::note`]
    /// which is only printed. Unlike notes these are never truncated: a caller
    /// asserts on them, so dropping the sixth one would hide a defect.
    fn finding(&mut self, finding: String) {
        self.findings.push(finding);
    }

    /// Chains measured across every pair and both arms. Zero means nothing was
    /// checked.
    pub fn sessions(&self) -> usize {
        self.chains
            .values()
            .map(|chain| chain.untouched.sessions + chain.appended.sessions)
            .sum()
    }

    /// Everything that did not add up. What a caller asserts on.
    pub fn findings(&self) -> &[String] {
        &self.findings
    }

    /// The arm where nothing was appended to the intermediate: the store's
    /// payoff, and the case where returning the origin is trivially correct.
    pub fn untouched(&self) -> ArmTotals {
        self.fold(|chain| &chain.untouched)
    }

    /// The arm where work was appended to the intermediate: the case the store
    /// was getting wrong, and the only one where the choice costs anything.
    pub fn appended(&self) -> ArmTotals {
        self.fold(|chain| &chain.appended)
    }

    fn fold(&self, arm: impl Fn(&ChainTally) -> &ArmTally) -> ArmTotals {
        self.chains
            .values()
            .map(arm)
            .fold(ArmTotals::default(), |mut totals, tally| {
                totals.sessions += tally.sessions;
                totals.source_capsules += tally.source_capsules;
                totals.with_store += tally.with_store;
                totals.without_store += tally.without_store;
                totals.store_kept_work += tally.store_kept_work;
                totals.control_kept_work += tally.control_kept_work;
                totals
            })
    }

    /// Print every count, objections or not.
    pub fn print(&self) {
        eprintln!(
            "\n════ second hop, tier {:?}: {} file(s), {} chain(s)",
            self.tier,
            self.files,
            self.sessions()
        );
        for ((source, target), chain) in &self.chains {
            eprintln!(
                "\n  {source} → {target} → {source}   {} bytes of work appended to the \
                 intermediates",
                chain.appended_bytes
            );
            for (arm, tally) in [
                ("nothing appended", &chain.untouched),
                ("work appended", &chain.appended),
            ] {
                eprintln!("    ── {arm}, {} session(s)", tally.sessions);
                eprintln!(
                    "       capsules in the source          {:>9}",
                    tally.source_capsules
                );
                eprintln!(
                    "       delivered with the store        {:>9}",
                    tally.with_store
                );
                eprintln!(
                    "       delivered without it            {:>9}",
                    tally.without_store
                );
                eprintln!(
                    "       appended work kept, with store  {:>9}",
                    tally.store_kept_work
                );
                eprintln!(
                    "       appended work kept, without     {:>9}",
                    tally.control_kept_work
                );
                eprintln!(
                    "       store read a session not named  {:>9}",
                    tally.overrode
                );
                eprintln!(
                    "       chosen source needed no convert {:>9}",
                    tally.needed_no_conversion
                );
            }
        }
        if self.note_count > 0 {
            eprintln!(
                "\n  hops the pipeline refused or failed: {}",
                self.note_count
            );
            for note in &self.notes {
                eprintln!("    · {note}");
            }
            if self.note_count > self.notes.len() {
                eprintln!("    · … and {} more", self.note_count - self.notes.len());
            }
        }
        if self.findings.is_empty() {
            eprintln!("\n  findings: none\n");
        } else {
            eprintln!("\n  findings ({}):", self.findings.len());
            for finding in self.findings.iter().take(EXAMPLES) {
                eprintln!("    ✗ {finding}");
            }
            if self.findings.len() > EXAMPLES {
                eprintln!("    ✗ … and {} more", self.findings.len() - EXAMPLES);
            }
            eprintln!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Branch, Role, SourceRef};

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

    /// Every slot the closure identity has a term for, on one session.
    #[test]
    fn the_closure_identity_holds_over_every_slot() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(message("old"));
        ir.events.push(message("summary"));
        ir.events.push(event(
            "cmp",
            Visibility::Model,
            Body::Compaction {
                context: vec!["summary".into()],
                supersedes: vec!["old".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));
        ir.events.push(event(
            "chrome",
            Visibility::Ui,
            Body::Control {
                control_kind: "mode".into(),
                data: serde_json::Value::Null,
            },
        ));
        ir.events.push(event(
            "huh",
            Visibility::Unclassified,
            Body::Unknown {
                native_type: None,
                raw: serde_json::Value::Null,
            },
        ));
        let mut turned = message("rolled");
        turned.turn = Some("t2".into());
        ir.events.push(turned);
        ir.events
            .push(event("rb", Visibility::Ui, Body::Rollback { turns: 1 }));

        let plan = resolve(&ir);
        let mut report = Report::new("unit", 1, &[] as &[&dyn Provider]);
        let counts = invariants(&mut report, Path::new("<unit>"), &ir, &plan);

        assert_eq!(report.findings(), &[] as &[String], "{:?}", report.findings());
        assert_eq!(counts.captured, 7);
        assert_eq!(counts.replayed, 1, "only the compaction's context survives");
        assert_eq!(counts.superseded, 1);
        assert_eq!(counts.rolled_back, 1);
        assert_eq!(counts.unclassified, 1);
        assert_eq!(counts.chrome, 1);
        assert_eq!(counts.markers, 2, "the compaction and the rollback");
        assert_eq!(counts.closed, 1);
    }

    /// A compaction whose context names a record the file does not contain is
    /// the failure mode this check exists for: the replay looks full and is not.
    #[test]
    fn a_replay_naming_an_absent_event_is_a_finding() {
        let mut ir = SessionIr::new("codex", "s1");
        ir.events.push(message("a"));
        ir.events.push(event(
            "cmp",
            Visibility::Model,
            Body::Compaction {
                context: vec!["ghost".into()],
                supersedes: vec!["a".into()],
                note: None,
                window_from: None,
                window_to: None,
            },
        ));

        let plan = resolve(&ir);
        let mut report = Report::new("unit", 1, &[] as &[&dyn Provider]);
        invariants(&mut report, Path::new("<unit>"), &ir, &plan);

        assert!(
            report
                .findings()
                .iter()
                .any(|finding| finding.contains("not events in the capture")),
            "{:?}",
            report.findings()
        );
    }

    #[test]
    fn a_live_head_naming_nothing_is_a_finding() {
        let mut ir = SessionIr::new("claude-code", "s1");
        ir.events.push(message("a"));
        ir.live_head = Some("nobody".into());

        let plan = resolve(&ir);
        let mut report = Report::new("unit", 1, &[] as &[&dyn Provider]);
        invariants(&mut report, Path::new("<unit>"), &ir, &plan);

        assert!(
            report
                .findings()
                .iter()
                .any(|finding| finding.contains("live_head")),
            "{:?}",
            report.findings()
        );
    }

    /// F6. The default has to be "this is our bug". A write that failed after a
    /// source was successfully claimed is not a fact about the input, and filing
    /// it as a printed note is how a real defect stays invisible.
    #[test]
    fn only_an_unusable_input_may_be_filed_as_a_note() {
        let unusable = anyhow::Error::from(crate::error::CasrError::ValidationError {
            errors: vec!["one-sided transcript".into()],
            warnings: Vec::new(),
            info: Vec::new(),
        });
        assert!(
            !is_write_defect(&unusable),
            "a session `validate_session` correctly refuses is a fact about the corpus"
        );

        for defect in [
            crate::error::CasrError::SessionWriteError {
                path: PathBuf::from("/sandbox/out.jsonl"),
                provider: "codex".into(),
                detail: "no space left on device".into(),
            },
            crate::error::CasrError::SessionReadError {
                path: PathBuf::from("/sandbox/out.jsonl"),
                provider: "codex".into(),
                detail: "unexpected end of input".into(),
            },
        ] {
            let error = anyhow::Error::from(defect);
            assert!(
                is_write_defect(&error),
                "a hop that got as far as writing and then failed is this crate's bug: {error}"
            );
        }
    }

    /// The registry is the list. If this ever finds fewer than two, the
    /// filtering broke and every table below would silently shrink with it.
    #[test]
    fn the_structured_providers_come_from_the_registry() {
        let registry = ProviderRegistry::default_registry();
        let slugs: Vec<&str> = registry
            .all_providers()
            .into_iter()
            .filter(|provider| provider.supports_structured_write())
            .map(|provider| provider.slug())
            .collect();
        assert!(
            slugs.contains(&"codex") && slugs.contains(&"claude-code"),
            "{slugs:?}"
        );
        for slug in &slugs {
            assert!(
                vendor_of(slug).is_some(),
                "{slug} is on the structured track but `compare::vendor_of` cannot name its \
                 vendor, so no crossing into it can be verified"
            );
        }
    }
}
