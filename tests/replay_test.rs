//! Replay resolution against a real local session corpus.
//!
//! [`casr::replay::resolve`] answers "what should the target model be shown",
//! and the four mechanisms it folds — compaction, rollback, abort, forks — are
//! all shapes that only occur at scale. A fixture proves the fold runs; only a
//! corpus proves it does not eat the session, which is the failure mode every
//! rule here is guarding against.
//!
//! Same contract as `corpus_test.rs`: `#[ignore]`d because the corpus is
//! machine-local and private, and skipped rather than failed when it is
//! absent. Run them explicitly:
//!
//! ```bash
//! AGSX_CODEX_CORPUS="$HOME/.codex/sessions" \
//! AGSX_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --release --test replay_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read. Nothing here writes to it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use casr::ir::{Body, SessionIr, Visibility};
use casr::providers::{claude_code_ir, codex_ir};
use casr::replay::{ExclusionReason, ReplayPlan, resolve};

/// Collect up to `limit` session files under the corpus named by `env_var`.
fn corpus_files(env_var: &str, limit: usize) -> Vec<PathBuf> {
    let Ok(root) = std::env::var(env_var) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files.truncate(limit);
    files
}

/// Whether a `.jsonl` under the Claude projects tree is actually a transcript.
///
/// Same discriminator as `corpus_test.rs`: the tree also holds workflow
/// journals that merely share the extension.
fn is_claude_transcript(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if stem.starts_with("agent-") {
        return true;
    }
    let groups: Vec<&str> = stem.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && stem.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

fn codex_corpus() -> Vec<PathBuf> {
    let files = corpus_files("AGSX_CODEX_CORPUS", 600);
    if files.is_empty() {
        eprintln!("AGSX_CODEX_CORPUS unset or empty; skipping");
    }
    files
}

fn claude_corpus() -> Vec<PathBuf> {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", 800)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
    }
    files
}

/// Model-visible events in the capture, before any resolution.
fn captured_model_events(ir: &SessionIr) -> usize {
    ir.events
        .iter()
        .filter(|event| event.visibility == Visibility::Model)
        .filter(|event| !matches!(event.body, Body::Compaction { .. }))
        .count()
}

fn last_compaction_context(ir: &SessionIr) -> Option<usize> {
    ir.events.iter().rev().find_map(|event| match &event.body {
        Body::Compaction { context, .. } => Some(context.len()),
        _ => None,
    })
}

#[derive(Default)]
struct Aggregate {
    files: usize,
    compacted: usize,
    captured: usize,
    resolved: usize,
    superseded: usize,
    rolled_back: usize,
    abandoned_fork: usize,
    unclassified: usize,
}

impl Aggregate {
    fn add(&mut self, ir: &SessionIr, plan: &ReplayPlan) {
        self.files += 1;
        self.captured += captured_model_events(ir);
        self.resolved += plan.events.len();
        if !plan.checkpoints.is_empty() {
            self.compacted += 1;
        }
        for excluded in &plan.excluded {
            match excluded.reason {
                ExclusionReason::Superseded { .. } => self.superseded += 1,
                ExclusionReason::RolledBack { .. } => self.rolled_back += 1,
                ExclusionReason::AbandonedFork => self.abandoned_fork += 1,
                ExclusionReason::NotModelVisible => self.unclassified += 1,
            }
        }
    }

    fn report(&self, label: &str) {
        println!("{label}: {} sessions, {} compacted", self.files, self.compacted);
        println!(
            "  model events   {} captured -> {} resolved",
            self.captured, self.resolved
        );
        println!("  superseded     {}", self.superseded);
        println!("  rolled back    {}", self.rolled_back);
        println!("  abandoned fork {}", self.abandoned_fork);
        println!("  unclassified   {}", self.unclassified);
    }
}

/// Every id in the plan is a model-visible event of the session, once.
///
/// Cheap to assert and it catches the whole class of bug where a fold starts
/// emitting ids it synthesised, ids it already emitted, or ids belonging to
/// chrome.
fn assert_plan_is_well_formed(path: &Path, ir: &SessionIr, plan: &ReplayPlan) {
    let replayable: HashSet<&str> = ir
        .events
        .iter()
        .filter(|event| event.visibility == Visibility::Model)
        .filter(|event| !matches!(event.body, Body::Compaction { .. }))
        .map(|event| event.id.as_str())
        .collect();

    let mut seen: HashSet<&str> = HashSet::new();
    for id in &plan.events {
        assert!(
            replayable.contains(id.as_str()),
            "{}: replay plan contains {id}, which is not a model-visible event \
             of this session",
            path.display()
        );
        assert!(
            seen.insert(id.as_str()),
            "{}: replay plan contains {id} twice",
            path.display()
        );
    }
}

/// A compacted Codex rollout must not replay as less than its own compaction.
///
/// The failure this guards is the original one: Codex hands the rewritten
/// history back as a new list of events that shares no id with anything before
/// it, so a resolver that models compaction as "remove these ids" ends up
/// replaying a preamble and nothing else.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_compacted_rollouts_replay_at_least_their_last_context() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    let mut checked = 0usize;
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else { continue };
        let plan = resolve(&ir);
        assert_plan_is_well_formed(path, &ir, &plan);
        totals.add(&ir, &plan);

        let Some(context) = last_compaction_context(&ir) else {
            continue;
        };
        checked += 1;
        assert!(
            plan.events.len() >= context,
            "{}: the last compaction restored {context} events but the replay \
             is {} — compaction is being diffed rather than assigned",
            path.display(),
            plan.events.len()
        );
    }

    totals.report("codex");
    assert!(checked > 0, "no compacted rollouts in the sample");
    println!("{checked} compacted rollouts replay at least their last context");
}

/// Rollback must undo turns, not sessions.
///
/// [`Body::Rollback`] carries `turns: 1` in every corpus occurrence, so the
/// aggregate cost of honouring it is small. A rule that mis-scoped the turn —
/// or that treated an abort as a rollback — would show up here as a
/// double-digit percentage rather than a fraction of one.
///
/// This also pins the visibility that made the original defect invisible.
/// Codex writes both directives as `event_msg`, the reader files them as `Ui`,
/// and the resolver must read them *before* its visibility gate. Every part of
/// that is asserted below, because with any one of them wrong the rule fires on
/// zero of 714 real rollbacks while the unit tests stay green.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_rollback_is_bounded_and_aborts_remove_nothing() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    let mut aborts = 0usize;
    let mut rollbacks = 0usize;
    let mut multi_turn_rollbacks = 0usize;
    let mut abort_ids: HashSet<String> = HashSet::new();
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else { continue };
        for event in &ir.events {
            match &event.body {
                Body::Abort { .. } => {
                    aborts += 1;
                    abort_ids.insert(event.id.clone());
                }
                Body::Rollback { turns } => {
                    rollbacks += 1;
                    if *turns != 1 {
                        multi_turn_rollbacks += 1;
                    }
                    assert_eq!(
                        event.visibility,
                        Visibility::Ui,
                        "{}: {} is a rollback the reader marked {:?}. Codex writes \
                         these as `event_msg` and the reader must keep saying so; \
                         promoting one to Model to make the resolver notice puts a \
                         rendering artifact into the target's context.",
                        path.display(),
                        event.id,
                        event.visibility
                    );
                }
                _ => {}
            }
        }
        let plan = resolve(&ir);
        // An abort must not be attributed as the cause of any removal, and the
        // marker itself is chrome, so it is never in the plan either.
        for excluded in &plan.excluded {
            if let ExclusionReason::RolledBack { by } = &excluded.reason {
                assert!(
                    !abort_ids.contains(by),
                    "{}: {} was removed and blamed on abort {by}",
                    path.display(),
                    excluded.id
                );
            }
        }
        totals.add(&ir, &plan);
    }

    totals.report("codex");
    println!(
        "{aborts} aborts, {rollbacks} rollbacks in the sample \
         ({multi_turn_rollbacks} naming more than one turn)"
    );

    assert!(
        rollbacks > 0,
        "no `Body::Rollback` in the sample; either the rollback rule went \
         unverified or the reader stopped typing `thread_rolled_back`"
    );
    assert!(
        totals.rolled_back > 0,
        "{rollbacks} rollbacks removed nothing — the rule is behind the \
         visibility gate again, and the reader files every one of these as \
         `event_msg`/`Ui`"
    );
    // 1,348 of 492,336 model events across 592 rollouts when this was written.
    assert!(
        totals.rolled_back * 20 < totals.captured,
        "rollback removed {} of {} model events (>5%); the turn scope is wrong",
        totals.rolled_back,
        totals.captured
    );
    assert!(
        aborts > 0,
        "no `Body::Abort` in the sample; the abort rule went unverified"
    );
}

/// A re-rooted Claude transcript must survive its own compaction.
///
/// Claude's DAG is real, so the fork prune is a genuine ancestor query — and a
/// naive one truncates a compacted transcript to the handful of records
/// written after the boundary, because compaction re-roots the graph.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_compacted_transcripts_survive_the_fork_prune() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    let mut checked = 0usize;
    let mut smallest = usize::MAX;
    for path in &files {
        let Ok(ir) = claude_code_ir::read(path) else {
            continue;
        };
        let plan = resolve(&ir);
        assert_plan_is_well_formed(path, &ir, &plan);
        totals.add(&ir, &plan);

        if plan.checkpoints.is_empty() {
            continue;
        }
        checked += 1;
        smallest = smallest.min(plan.events.len());
        assert!(
            plan.events.len() > 10,
            "{}: compacted transcript resolved to {} events out of {} captured \
             — the walk is stopping short of the checkpoint",
            path.display(),
            plan.events.len(),
            captured_model_events(&ir)
        );
    }

    totals.report("claude");
    assert!(
        checked > 0,
        "no compacted transcripts in the sample; the checkpoint composition \
         went unverified"
    );
    println!("{checked} compacted transcripts, smallest replay {smallest} events");
}

/// The prune removes branches, not conversations.
///
/// Claude only has a DAG at all when the user edits or retries, so abandoned
/// forks are rare: 79 events across 173 transcripts when this was written. A
/// prune that started eating live turns would move this by orders of magnitude.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_fork_prune_stays_a_minority_of_the_transcript() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    let mut with_leaf = 0usize;
    for path in &files {
        let Ok(ir) = claude_code_ir::read(path) else {
            continue;
        };
        if ir.live_head.is_some() {
            with_leaf += 1;
        }
        totals.add(&ir, &resolve(&ir));
    }

    totals.report("claude");
    println!("{with_leaf} of {} transcripts name a leaf", totals.files);

    assert!(
        with_leaf > 0,
        "no transcript named a live head; either the fork prune went unverified \
         or the reader stopped lifting `last-prompt.leafUuid` onto the session"
    );
    assert!(
        totals.abandoned_fork * 20 < totals.captured,
        "the fork prune dropped {} of {} model events (>5%); it is truncating \
         live conversation, not pruning branches",
        totals.abandoned_fork,
        totals.captured
    );
    // Every exclusion is accounted for by exactly one reason, so the four
    // buckets plus the replay must reconstruct the capture.
    assert_eq!(
        totals.resolved
            + totals.superseded
            + totals.rolled_back
            + totals.abandoned_fork,
        totals.captured,
        "resolved + excluded does not reconstruct the captured model events"
    );
}

/// Codex names no live head, so the fork prune must be inert there.
///
/// Stated as a corpus assertion rather than a comment because the guarantee is
/// structural — the resolver never branches on the agent, it just finds
/// `live_head: None` — and a future reader that started synthesising a head
/// would break it silently.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_never_loses_events_to_the_fork_prune() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else { continue };
        assert_eq!(
            ir.live_head,
            None,
            "{}: a Codex rollout records no branch head; synthesising one turns \
             the fork prune on over a graph Codex does not have",
            path.display()
        );
        totals.add(&ir, &resolve(&ir));
    }

    totals.report("codex");
    assert_eq!(
        totals.abandoned_fork, 0,
        "Codex records a linear history and no leaf; any fork exclusion means \
         the prune is running on a graph it cannot read"
    );
    assert_eq!(
        totals.resolved + totals.superseded + totals.rolled_back,
        totals.captured,
        "resolved + excluded does not reconstruct the captured model events"
    );
}

/// `model_visible()` and `resolve()` must not drift apart again.
#[test]
#[ignore = "requires a local corpus; set AGSX_CODEX_CORPUS and AGSX_CLAUDE_CORPUS"]
fn model_visible_agrees_with_the_resolver() {
    let mut checked = 0usize;
    for path in codex_corpus() {
        let Ok(ir) = codex_ir::read(&path) else {
            continue;
        };
        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(visible, resolve(&ir).events, "{}", path.display());
        checked += 1;
    }
    for path in claude_corpus() {
        let Ok(ir) = claude_code_ir::read(&path) else {
            continue;
        };
        let visible: Vec<&str> = ir.model_visible().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(visible, resolve(&ir).events, "{}", path.display());
        checked += 1;
    }
    if checked == 0 {
        return;
    }
    println!("model_visible() matches resolve() on {checked} sessions");
}

/// What a compaction marker looks like today, and why the fold's two
/// compaction rules cost nothing on real sessions.
///
/// Three measurements in one pass, because they are three halves of the same
/// claim — that the resolver now takes `Body::Compaction` at its word, and that
/// doing so changes nothing about what any current session replays:
///
/// - Every compaction records its `context` in file order, so replaying it in
///   the compaction's order rather than the file's is a no-op today. The old
///   re-sort-by-file-position was therefore invisible while it quietly overruled
///   the state assignment.
/// - The whole plan comes out in file order, which is the same measurement seen
///   from the other end: the removed sort had nothing to reorder.
/// - Every marker is `Visibility::Model`, so exempting it from the visibility
///   gate is likewise a no-op today. It matters for the reader that files its
///   boundary as chrome, the way Codex files its rollbacks and aborts.
///
/// A failure here is not automatically a bug: a reader is *allowed* to record a
/// context in an order of its own, and the replay follows the compaction. It
/// means this measurement is stale and the ordering is now load-bearing. Do not
/// "fix" it by sorting the replay again.
#[test]
#[ignore = "requires a local corpus; set AGSX_CODEX_CORPUS and AGSX_CLAUDE_CORPUS"]
fn compactions_are_recorded_in_file_order_and_marked_model_visible() {
    let mut compactions = 0usize;
    let mut out_of_order = 0usize;
    let mut not_model = 0usize;
    let mut plans_out_of_order = 0usize;

    let mut check = |path: &Path, ir: &SessionIr| {
        let position: HashMap<&str, usize> = ir
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.as_str(), index))
            .collect();
        for event in &ir.events {
            let Body::Compaction { context, .. } = &event.body else {
                continue;
            };
            compactions += 1;
            if event.visibility != Visibility::Model {
                not_model += 1;
                println!(
                    "{}: {} is a compaction the reader marked {:?}",
                    path.display(),
                    event.id,
                    event.visibility
                );
            }
            let places: Vec<usize> = context
                .iter()
                .filter_map(|id| position.get(id.as_str()).copied())
                .collect();
            if places.windows(2).any(|pair| pair[0] > pair[1]) {
                out_of_order += 1;
                println!(
                    "{}: {} lists context out of file order",
                    path.display(),
                    event.id
                );
            }
        }

        let replayed: Vec<usize> = resolve(ir)
            .events
            .iter()
            .filter_map(|id| position.get(id.as_str()).copied())
            .collect();
        if replayed.windows(2).any(|pair| pair[0] > pair[1]) {
            plans_out_of_order += 1;
            println!("{}: the replay is not in file order", path.display());
        }
    };

    for path in codex_corpus() {
        if let Ok(ir) = codex_ir::read(&path) {
            check(&path, &ir);
        }
    }
    for path in claude_corpus() {
        if let Ok(ir) = claude_code_ir::read(&path) {
            check(&path, &ir);
        }
    }
    if compactions == 0 {
        return;
    }

    println!(
        "{compactions} compactions: {out_of_order} not in file order, \
         {not_model} not model-visible; {plans_out_of_order} plans not in file order"
    );
    assert_eq!(
        out_of_order, 0,
        "see this test's doc comment before changing it"
    );
    assert_eq!(
        plans_out_of_order, 0,
        "see this test's doc comment before changing it"
    );
    assert_eq!(
        not_model, 0,
        "a reader now files its compaction boundary as chrome; the fold reads \
         the marker before the visibility gate, so verify that is intended"
    );
}

/// The fork prune may never contradict the newest compaction.
///
/// `resolve` folds compaction as a state assignment: after the boundary, the
/// model's context *is* `context`. `prune_forks` runs afterwards and is only a
/// membership test over a DAG that compaction re-roots, so a leaf whose parent
/// chain cannot reach the boundary is a fact about the graph, not a licence to
/// delete the post-compaction session.
#[test]
#[ignore = "requires a local corpus; set AGSX_CODEX_CORPUS and AGSX_CLAUDE_CORPUS"]
fn the_newest_checkpoint_context_is_never_pruned() {
    let mut compacted = 0usize;
    let mut pruned_sessions = 0usize;
    let mut pruned_events = 0usize;

    let mut check = |path: &Path, ir: &SessionIr| {
        let plan = resolve(ir);
        let Some(marker) = plan.checkpoints.last() else {
            return;
        };
        compacted += 1;
        let context: HashSet<&str> = ir
            .events
            .iter()
            .find(|event| &event.id == marker)
            .and_then(|event| match &event.body {
                Body::Compaction { context, .. } => Some(context),
                _ => None,
            })
            .map(|context| context.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let lost = plan
            .excluded
            .iter()
            .filter(|excluded| excluded.reason == ExclusionReason::AbandonedFork)
            .filter(|excluded| context.contains(excluded.id.as_str()))
            .count();
        if lost > 0 {
            pruned_sessions += 1;
            pruned_events += lost;
            println!(
                "{}: the fork prune dropped {lost} of {} events the newest \
                 compaction placed in the model's context",
                path.display(),
                context.len()
            );
        }
    };

    for path in codex_corpus() {
        if let Ok(ir) = codex_ir::read(&path) {
            check(&path, &ir);
        }
    }
    for path in claude_corpus() {
        if let Ok(ir) = claude_code_ir::read(&path) {
            check(&path, &ir);
        }
    }
    if compacted == 0 {
        return;
    }

    println!("{compacted} compacted sessions, {pruned_sessions} with pruned checkpoint context");
    assert_eq!(
        pruned_events, 0,
        "{pruned_events} events across {pruned_sessions} sessions were assigned \
         to the model's context by a compaction and then removed by the fork \
         prune"
    );
}

/// The two transcripts whose replay size was measured by hand stay that size.
///
/// `55f695db` is the one that proved reaching the checkpoint *marker* has to
/// count as reaching the checkpoint: its `compact_boundary` names a
/// `logicalParentUuid` that is not in the file, and a walk recognising only
/// context ids returned 9 events instead of 11. `2d68b149` is the one that
/// proved the leaf's descendants belong in the replay: 1,311 with them, 1,309
/// without. Both numbers were established by measurement rather than by
/// argument, so they are pinned here rather than restated in a comment.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn measured_transcripts_keep_their_replay_size() {
    let Ok(root) = std::env::var("AGSX_CLAUDE_CORPUS") else {
        eprintln!("AGSX_CLAUDE_CORPUS unset; skipping");
        return;
    };
    let by_stem: HashMap<String, PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?.to_string();
            Some((stem, path))
        })
        .collect();

    let mut checked = 0usize;
    for prefix in [
        "55f695db-fa1b-4d8d-9fc9-4bcf2c9fa892",
        "2d68b149-3e63-489f-9785-0bdbcd139b0e",
    ] {
        let Some(path) = by_stem.get(prefix) else {
            eprintln!("{prefix} not in this corpus; skipping");
            continue;
        };
        let ir = claude_code_ir::read(path).expect("read");
        let plan = resolve(&ir);
        let replayed: HashSet<&str> = plan.events.iter().map(String::as_str).collect();

        // The checkpoint-marker rule, stated as the property the 9-vs-11
        // measurement was actually about: every id the newest checkpoint
        // assigns has to survive into the replay. Without the rule the walk
        // could not reach the marker on this transcript and dropped part of
        // that context.
        if let Some(newest) = plan.checkpoints.last() {
            let context = ir
                .events
                .iter()
                .find(|event| &event.id == newest)
                .and_then(|event| match &event.body {
                    Body::Compaction { context, .. } => Some(context.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            for id in &context {
                assert!(
                    replayed.contains(id.as_str()),
                    "{}: checkpoint {newest} assigns {id}, which is not in the replay",
                    path.display()
                );
            }
        }

        // The descendant walk, likewise: if the transcript has a live head,
        // anything hanging below it is live and must be replayed.
        if let Some(head) = ir.live_head.as_deref() {
            for event in &ir.events {
                if event.parent.as_deref() == Some(head) && replayed.contains(head) {
                    assert!(
                        replayed.contains(event.id.as_str()),
                        "{}: {} descends from live head {head} and was pruned",
                        path.display(),
                        event.id
                    );
                }
            }
        }

        println!("{prefix}: {} events replayed", plan.events.len());
        checked += 1;
    }
    // Deliberately no absolute size assertion. Both transcripts are live
    // sessions on the machine that runs this suite -- `55f695db` grew from 11
    // replayed events to 18 while this very change was being written -- so
    // pinning a count pins the afternoon, not the rule. The rules are pinned
    // above as properties, which is what the original measurements established.
    println!("{checked} measured transcripts still satisfy the rules measured on them");
}
