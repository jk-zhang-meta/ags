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

use crate::compare::{Comparison, compare, vendor_of};
use crate::discovery::ProviderRegistry;
use crate::ir::{Body, Event, Fidelity, Loss, SessionIr, Visibility};
use crate::providers::{Provider, WriteOptions};
use crate::replay::{ExclusionReason, ReplayPlan, resolve};

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

    let written = match target.write_session_ir(ir, &WriteOptions { force: false }) {
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
    if derived > claimed {
        report.pending.push(format!(
            "{} -> {} {}: graded {claimed:?}, but the written file only supports {derived:?}",
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
                if !same_agent && after == 0 && tally.predicted.is_empty() && tally.degraded.is_empty()
                {
                    self.finding(&format!(
                        "{from} -> {to}: `Body::{kind}` appears {before} time(s) in the replays \
                         read and never in the written sessions, and no loss was reported for it"
                    ));
                }
            }

            if !same_agent && tally.loss_events == 0 && !tally.predicted.is_empty() {
                self.finding(&format!(
                    "{from} -> {to}: the comparator found losses the writer's own loss list does \
                     not mention, so nothing downstream can say what the grade is made of"
                ));
            }
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
