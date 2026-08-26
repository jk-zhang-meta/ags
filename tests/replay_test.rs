//! Replay resolution against a real local session corpus.
//!
//! [`ags::replay::resolve`] answers "what should the target model be shown",
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
//! AGS_CODEX_CORPUS="$HOME/.codex/sessions" \
//! AGS_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --release --test replay_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read. Nothing here writes to it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ags::ir::{Body, SessionIr, Visibility};
use ags::providers::{claude_code_ir, codex_ir};
use ags::replay::{ExclusionReason, ReplayPlan, resolve};

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
    let files = corpus_files("AGS_CODEX_CORPUS", 600);
    if files.is_empty() {
        eprintln!("AGS_CODEX_CORPUS unset or empty; skipping");
    }
    files
}

fn claude_corpus() -> Vec<PathBuf> {
    let files: Vec<PathBuf> = corpus_files("AGS_CLAUDE_CORPUS", 800)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGS_CLAUDE_CORPUS unset or empty; skipping");
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
        println!(
            "{label}: {} sessions, {} compacted",
            self.files, self.compacted
        );
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
#[ignore = "requires a local Codex corpus; set AGS_CODEX_CORPUS"]
fn codex_compacted_rollouts_replay_at_least_their_last_context() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    let mut checked = 0usize;
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else {
            continue;
        };
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
#[ignore = "requires a local Codex corpus; set AGS_CODEX_CORPUS"]
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
        let Ok(ir) = codex_ir::read(path) else {
            continue;
        };
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
#[ignore = "requires a local Claude corpus; set AGS_CLAUDE_CORPUS"]
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
#[ignore = "requires a local Claude corpus; set AGS_CLAUDE_CORPUS"]
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
        totals.resolved + totals.superseded + totals.rolled_back + totals.abandoned_fork,
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
#[ignore = "requires a local Codex corpus; set AGS_CODEX_CORPUS"]
fn codex_never_loses_events_to_the_fork_prune() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Aggregate::default();
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else {
            continue;
        };
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
#[ignore = "requires a local corpus; set AGS_CODEX_CORPUS and AGS_CLAUDE_CORPUS"]
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
#[ignore = "requires a local corpus; set AGS_CODEX_CORPUS and AGS_CLAUDE_CORPUS"]
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
#[ignore = "requires a local corpus; set AGS_CODEX_CORPUS and AGS_CLAUDE_CORPUS"]
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
#[ignore = "requires a local Claude corpus; set AGS_CLAUDE_CORPUS"]
fn measured_transcripts_keep_their_replay_size() {
    let Ok(root) = std::env::var("AGS_CLAUDE_CORPUS") else {
        eprintln!("AGS_CLAUDE_CORPUS unset; skipping");
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

// ---------------------------------------------------------------------------
// Claude Code's own replay, as an independent oracle
// ---------------------------------------------------------------------------

/// The part of Claude Code 2.1.220's transcript loader that determines which
/// user/assistant records resume, transcribed from the shipped Bun executable.
///
/// This deliberately depends on raw JSON and standard data structures, never
/// `claude_code_ir` or `replay::resolve`. Calling the implementation under test
/// from here would turn the corpus comparison below back into the self-oracle
/// that let the compaction-summary loss pass.
mod claude_vendor_oracle {
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::Path;

    use chrono::DateTime;
    use serde_json::Value;

    const TIMESTAMP_FALLBACK_MS: i64 = 5_000;

    #[derive(Clone, Debug)]
    struct Record {
        uuid: String,
        kind: String,
        subtype: Option<String>,
        parent: Option<String>,
        is_sidechain: bool,
        timestamp: Option<String>,
        message_id: Option<String>,
        has_tool_result: bool,
        compact_metadata: Option<Value>,
    }

    impl Record {
        fn from_value(value: &Value, parent: Option<String>) -> Option<Self> {
            let kind = value.get("type")?.as_str()?.to_string();
            if !matches!(
                kind.as_str(),
                "user" | "assistant" | "attachment" | "system"
            ) {
                return None;
            }
            let uuid = value.get("uuid")?.as_str()?.to_string();
            let content = value
                .get("message")
                .and_then(|message| message.get("content"));
            let has_tool_result = content.and_then(Value::as_array).is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
            });

            Some(Self {
                uuid,
                kind,
                subtype: value
                    .get("subtype")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parent,
                is_sidechain: value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                timestamp: value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message_id: value
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                has_tool_result,
                compact_metadata: value.get("compactMetadata").cloned(),
            })
        }

        fn is_conversation(&self) -> bool {
            matches!(self.kind.as_str(), "user" | "assistant")
        }

        fn is_compaction(&self) -> bool {
            self.kind == "system" && self.subtype.as_deref() == Some("compact_boundary")
        }
    }

    /// JavaScript `Map`: replacing a value does not move its insertion slot.
    #[derive(Default)]
    struct Records {
        order: Vec<String>,
        by_id: HashMap<String, Record>,
    }

    impl Records {
        fn insert(&mut self, record: Record) {
            if !self.by_id.contains_key(&record.uuid) {
                self.order.push(record.uuid.clone());
            }
            self.by_id.insert(record.uuid.clone(), record);
        }

        fn get(&self, id: &str) -> Option<&Record> {
            self.by_id.get(id)
        }

        fn get_mut(&mut self, id: &str) -> Option<&mut Record> {
            self.by_id.get_mut(id)
        }
    }

    struct Loaded {
        records: Records,
        last_non_sidechain: Option<String>,
        last_prompt_leaf: Option<String>,
        last_prompt_is_explicit: bool,
        cleared_to_empty: bool,
    }

    #[derive(Clone)]
    struct Preserved {
        anchor: String,
        uuids: Vec<String>,
    }

    /// Vendor `PBe`: build the threaded-message map and live-leaf hints.
    fn load(values: &[Value]) -> Loaded {
        let mut records = Records::default();
        let mut progress_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut last_non_sidechain = None;
        let mut last_prompt_leaf = None;
        let mut last_prompt_is_explicit = false;
        let mut last_prompt_was_rewound = false;
        let mut cleared_to_empty = false;

        for value in values {
            let kind = value.get("type").and_then(Value::as_str);
            if kind == Some("progress") {
                let Some(uuid) = value.get("uuid").and_then(Value::as_str) else {
                    continue;
                };
                let parent = value
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let flattened = parent
                    .as_deref()
                    .and_then(|parent| progress_parent.get(parent))
                    .cloned()
                    .unwrap_or(parent);
                progress_parent.insert(uuid.to_string(), flattened);
                continue;
            }

            if matches!(kind, Some("user" | "assistant" | "attachment" | "system")) {
                let parent = value
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let parent = parent
                    .as_deref()
                    .and_then(|parent| progress_parent.get(parent))
                    .cloned()
                    .unwrap_or(parent);
                let Some(record) = Record::from_value(value, parent) else {
                    continue;
                };

                if !record.is_sidechain {
                    last_non_sidechain = Some(record.uuid.clone());
                    last_prompt_is_explicit = false;
                    cleared_to_empty = false;
                    last_prompt_was_rewound = false;
                }
                let is_compaction = record.is_compaction();
                records.insert(record);
                if is_compaction {
                    last_prompt_leaf = None;
                    last_prompt_is_explicit = false;
                }
                continue;
            }

            if kind == Some("last-prompt") {
                match value.get("leafUuid") {
                    Some(Value::String(leaf)) if !leaf.is_empty() => {
                        let same_leaf = last_prompt_leaf.as_deref() == Some(leaf.as_str());
                        last_prompt_is_explicit = value.get("explicit").and_then(Value::as_bool)
                            == Some(true)
                            || (last_prompt_is_explicit && same_leaf);
                        last_prompt_was_rewound = value.get("rewound").and_then(Value::as_bool)
                            == Some(true)
                            || (last_prompt_was_rewound && same_leaf);
                        last_prompt_leaf = Some(leaf.clone());
                        cleared_to_empty = false;
                    }
                    Some(Value::Null)
                        if value.get("explicit").and_then(Value::as_bool) == Some(true) =>
                    {
                        cleared_to_empty = true;
                        last_prompt_leaf = None;
                        last_prompt_is_explicit = false;
                        last_prompt_was_rewound = false;
                    }
                    _ => {}
                }
            }
        }

        Loaded {
            records,
            last_non_sidechain,
            last_prompt_leaf,
            last_prompt_is_explicit,
            cleared_to_empty,
        }
    }

    /// Vendor `TB_`: normalize either preservation encoding.
    fn preserved_from(metadata: &Value, records: &Records) -> Option<Preserved> {
        if let Some(preserved) = metadata.get("preservedMessages")
            && !preserved.is_null()
        {
            let anchor = preserved.get("anchorUuid")?.as_str()?.to_string();
            let uuids = preserved
                .get("uuids")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            return Some(Preserved { anchor, uuids });
        }

        let segment = metadata.get("preservedSegment")?;
        if segment.is_null() {
            return None;
        }
        let anchor = segment.get("anchorUuid")?.as_str()?.to_string();
        let head = segment.get("headUuid")?.as_str()?;
        let mut cursor = segment.get("tailUuid")?.as_str()?.to_string();
        let mut seen = HashSet::new();
        let mut uuids = Vec::new();
        loop {
            if !seen.insert(cursor.clone()) {
                return None;
            }
            let record = records.get(&cursor)?;
            uuids.push(cursor.clone());
            if cursor == head {
                uuids.reverse();
                return Some(Preserved { anchor, uuids });
            }
            cursor = record.parent.clone()?;
        }
    }

    /// Vendor `rsp`: relink preserved records and discard pre-boundary ones.
    fn relink(records: &mut Records) -> Option<String> {
        let mut newest_boundary = None;
        let mut metadata_boundary = None;
        let mut metadata = None;

        for (index, id) in records.order.iter().enumerate() {
            let Some(record) = records.get(id) else {
                continue;
            };
            if !record.is_compaction() {
                continue;
            }
            newest_boundary = Some(index);
            let Some(candidate) = record.compact_metadata.as_ref() else {
                continue;
            };
            if candidate
                .get("preservedMessages")
                .is_some_and(|value| !value.is_null())
                || candidate
                    .get("preservedSegment")
                    .is_some_and(|value| !value.is_null())
            {
                metadata_boundary = Some(index);
                metadata = Some(candidate.clone());
            }
        }

        let metadata = metadata?;
        let newest_boundary = newest_boundary?;
        let metadata_is_newest = metadata_boundary == Some(newest_boundary);
        let resolved = metadata_is_newest.then(|| preserved_from(&metadata, records));
        let resolved = match resolved {
            Some(Some(preserved)) => Some(preserved),
            Some(None) => return None,
            None => None,
        };
        let preserved = resolved.filter(|preserved| !preserved.uuids.is_empty());

        if preserved.as_ref().is_some_and(|preserved| {
            preserved
                .uuids
                .iter()
                .any(|uuid| !records.by_id.contains_key(uuid))
        }) {
            return None;
        }

        let kept: Vec<String> = preserved
            .as_ref()
            .map(|preserved| preserved.uuids.clone())
            .unwrap_or_default();
        let kept_set: HashSet<&str> = kept.iter().map(String::as_str).collect();

        if let Some(preserved) = &preserved {
            let tail = preserved.uuids.last()?.clone();
            let mut parent = preserved.anchor.clone();
            for uuid in &preserved.uuids {
                records.get_mut(uuid)?.parent = Some(parent);
                parent = uuid.clone();
            }

            let reparent: Vec<String> = records
                .order
                .iter()
                .filter_map(|uuid| {
                    let record = records.get(uuid)?;
                    (record.parent.as_deref() == Some(preserved.anchor.as_str())
                        && uuid != &preserved.uuids[0])
                        .then(|| uuid.clone())
                })
                .collect();
            for uuid in reparent {
                records.get_mut(&uuid)?.parent = Some(tail.clone());
            }
        }

        let deleted: Vec<String> = records
            .order
            .iter()
            .enumerate()
            .filter(|(index, uuid)| *index < newest_boundary && !kept_set.contains(uuid.as_str()))
            .map(|(_, uuid)| uuid.clone())
            .collect();
        for uuid in &deleted {
            records.by_id.remove(uuid);
        }

        if let Some(preserved) = &preserved
            && !deleted.is_empty()
        {
            let tail = preserved.uuids.last()?.clone();
            let deleted: HashSet<&str> = deleted.iter().map(String::as_str).collect();
            let reparent: Vec<String> = records
                .order
                .iter()
                .filter_map(|uuid| {
                    let record = records.get(uuid)?;
                    (record.is_conversation()
                        && record
                            .parent
                            .as_deref()
                            .is_some_and(|parent| deleted.contains(parent)))
                    .then(|| uuid.clone())
                })
                .collect();
            for uuid in reparent {
                records.get_mut(&uuid)?.parent = Some(tail.clone());
            }
        }

        kept.last().cloned()
    }

    fn nearest_conversation(records: &Records, start: &str) -> Option<String> {
        let mut cursor = start;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(cursor.to_string()) {
                return None;
            }
            let record = records.get(cursor)?;
            if record.is_conversation() {
                return Some(record.uuid.clone());
            }
            cursor = record.parent.as_deref()?;
        }
    }

    /// Vendor's nested `V`: resolve the candidate conversational leaf set.
    fn leaf_candidates(loaded: &Loaded, relink_tail: Option<&str>) -> Vec<String> {
        if loaded.cleared_to_empty {
            return Vec::new();
        }

        let records = &loaded.records;
        let explicit_leaf = loaded.last_prompt_is_explicit
            && loaded
                .last_prompt_leaf
                .as_deref()
                .is_some_and(|leaf| records.get(leaf).is_some_and(|record| !record.is_sidechain));

        if relink_tail.is_none() || explicit_leaf {
            let mut tail = loaded
                .last_prompt_leaf
                .as_ref()
                .filter(|leaf| records.get(leaf).is_some())
                .cloned();

            if let (Some(recorded), Some(last)) =
                (tail.as_deref(), loaded.last_non_sidechain.as_deref())
                && !loaded.last_prompt_is_explicit
                && recorded != last
                && records.get(last).is_some()
            {
                let mut cursor = Some(last);
                let mut seen = HashSet::new();
                while let Some(uuid) = cursor {
                    if !seen.insert(uuid) {
                        break;
                    }
                    if uuid == recorded {
                        tail = Some(last.to_string());
                        break;
                    }
                    cursor = records
                        .get(uuid)
                        .and_then(|record| record.parent.as_deref());
                }
            }

            if relink_tail.is_none() && tail.is_none() {
                tail.clone_from(&loaded.last_non_sidechain);
            }
            if let Some(tail) = tail
                && let Some(candidate) = nearest_conversation(records, &tail)
            {
                return vec![candidate];
            }
        }

        let mut parents = HashSet::new();
        let mut conversation_parents = HashSet::new();
        for uuid in &records.order {
            let Some(record) = records.get(uuid) else {
                continue;
            };
            if let Some(parent) = &record.parent {
                parents.insert(parent.as_str());
                if record.is_conversation() {
                    conversation_parents.insert(parent.as_str());
                }
            }
        }

        let mut candidates = Vec::new();
        let mut candidate_set = HashSet::new();
        for uuid in &records.order {
            let Some(record) = records.get(uuid) else {
                continue;
            };
            if parents.contains(record.uuid.as_str()) {
                continue;
            }
            let Some(candidate) = nearest_conversation(records, &record.uuid) else {
                continue;
            };
            if !conversation_parents.contains(candidate.as_str())
                && candidate_set.insert(candidate.clone())
            {
                candidates.push(candidate);
            }
        }

        if candidates.len() > 1 {
            let preferred = loaded
                .last_prompt_leaf
                .as_ref()
                .filter(|leaf| candidate_set.contains(*leaf))
                .or(loaded.last_non_sidechain.as_ref());
            if let Some(preferred) = preferred
                && let Some(candidate) = nearest_conversation(records, preferred)
            {
                return vec![candidate];
            }
        }

        candidates
    }

    fn timestamp_ms(timestamp: Option<&str>) -> Option<i64> {
        DateTime::parse_from_rfc3339(timestamp?)
            .ok()
            .map(|ts| ts.timestamp_millis())
    }

    /// Vendor `m2t`/`DBe`: newest timestamp among the candidate leaves.
    fn newest_leaf(records: &Records, candidates: &[String]) -> Option<String> {
        let mut best = None;
        let mut best_time = i64::MIN;
        for uuid in candidates {
            let record = records.get(uuid)?;
            if !record.is_conversation() {
                continue;
            }
            let Some(timestamp) = timestamp_ms(record.timestamp.as_deref()) else {
                continue;
            };
            if timestamp > best_time {
                best = Some(uuid.clone());
                best_time = timestamp;
            }
        }
        best
    }

    /// Vendor `kB_`: recover a missing parent from the nearest prior timestamp.
    fn timestamp_parent(
        records: &Records,
        child: &Record,
        seen: &HashSet<String>,
    ) -> Option<String> {
        let child_time = timestamp_ms(child.timestamp.as_deref())?;
        let mut best = None;
        let mut best_delta = i64::MAX;

        for uuid in &records.order {
            let Some(candidate) = records.get(uuid) else {
                continue;
            };
            if seen.contains(uuid) || candidate.is_sidechain != child.is_sidechain {
                continue;
            }
            let Some(candidate_time) = timestamp_ms(candidate.timestamp.as_deref()) else {
                continue;
            };
            let delta = child_time - candidate_time;
            if (0..=TIMESTAMP_FALLBACK_MS).contains(&delta) && delta < best_delta {
                best = Some(uuid.clone());
                best_delta = delta;
            }
        }
        best
    }

    /// Vendor `HB_`: recover parallel assistant/tool-result records that share
    /// an Anthropic message id with an assistant already on the chain.
    fn recover_parallel(
        records: &Records,
        chain: &[String],
        seen: &mut HashSet<String>,
    ) -> Vec<String> {
        let assistants: Vec<String> = chain
            .iter()
            .filter(|uuid| {
                records
                    .get(uuid)
                    .is_some_and(|record| record.kind == "assistant")
            })
            .cloned()
            .collect();
        if assistants.is_empty() {
            return chain.to_vec();
        }

        let mut chain_anchor: HashMap<String, String> = HashMap::new();
        for uuid in &assistants {
            if let Some(message_id) = records
                .get(uuid)
                .and_then(|record| record.message_id.as_ref())
            {
                chain_anchor.insert(message_id.clone(), uuid.clone());
            }
        }

        let mut assistants_by_message: HashMap<String, Vec<String>> = HashMap::new();
        let mut tool_results_by_parent: HashMap<String, Vec<String>> = HashMap::new();
        for uuid in &records.order {
            let Some(record) = records.get(uuid) else {
                continue;
            };
            if record.kind == "assistant" {
                if let Some(message_id) = &record.message_id {
                    assistants_by_message
                        .entry(message_id.clone())
                        .or_default()
                        .push(uuid.clone());
                }
            } else if record.kind == "user"
                && record.has_tool_result
                && let Some(parent) = &record.parent
            {
                tool_results_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(uuid.clone());
            }
        }

        let mut handled_messages = HashSet::new();
        let mut extras_after: HashMap<String, Vec<String>> = HashMap::new();
        for assistant in &assistants {
            let Some(message_id) = records
                .get(assistant)
                .and_then(|record| record.message_id.as_ref())
            else {
                continue;
            };
            if !handled_messages.insert(message_id.clone()) {
                continue;
            }

            let same_message = assistants_by_message
                .get(message_id)
                .cloned()
                .unwrap_or_else(|| vec![assistant.clone()]);
            let mut assistant_extras: Vec<String> = same_message
                .iter()
                .filter(|uuid| !seen.contains(*uuid))
                .cloned()
                .collect();
            let mut tool_extras = Vec::new();
            for uuid in &same_message {
                if let Some(results) = tool_results_by_parent.get(uuid) {
                    tool_extras.extend(results.iter().filter(|id| !seen.contains(*id)).cloned());
                }
            }
            assistant_extras.sort_by_key(|uuid| {
                records
                    .get(uuid)
                    .and_then(|record| record.timestamp.clone())
                    .unwrap_or_default()
            });
            tool_extras.sort_by_key(|uuid| {
                records
                    .get(uuid)
                    .and_then(|record| record.timestamp.clone())
                    .unwrap_or_default()
            });
            assistant_extras.extend(tool_extras);
            if assistant_extras.is_empty() {
                continue;
            }

            for uuid in &assistant_extras {
                seen.insert(uuid.clone());
            }
            if let Some(anchor) = chain_anchor.get(message_id) {
                extras_after.insert(anchor.clone(), assistant_extras);
            }
        }

        let mut recovered = Vec::new();
        for uuid in chain {
            recovered.push(uuid.clone());
            if let Some(extras) = extras_after.get(uuid) {
                recovered.extend(extras.iter().cloned());
            }
        }
        recovered
    }

    /// Vendor `Bze`, returning only user/assistant records. Its final `CB_`
    /// traversal adds only non-user/assistant descendants, so it cannot change
    /// this filtered sequence and is intentionally absent.
    fn chain(records: &Records, leaf: &str) -> Vec<String> {
        let mut reversed = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = Some(leaf.to_string());

        while let Some(uuid) = cursor {
            if !seen.insert(uuid.clone()) {
                break;
            }
            let Some(record) = records.get(&uuid) else {
                break;
            };
            reversed.push(uuid);
            cursor = match record.parent.as_deref() {
                None => None,
                Some(parent) if records.get(parent).is_some() && !seen.contains(parent) => {
                    Some(parent.to_string())
                }
                Some(_) => timestamp_parent(records, record, &seen),
            };
        }

        reversed.reverse();
        let recovered = recover_parallel(records, &reversed, &mut seen);
        recovered
            .into_iter()
            .filter(|uuid| records.get(uuid).is_some_and(Record::is_conversation))
            .collect()
    }

    /// Expected user/assistant record ids, in Claude Code resume order.
    pub fn replayed_conversation(path: &Path) -> io::Result<Option<Vec<String>>> {
        let text = std::fs::read_to_string(path)?;
        let values: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let mut loaded = load(&values);
        let relink_tail = relink(&mut loaded.records);
        let candidates = leaf_candidates(&loaded, relink_tail.as_deref());
        let Some(leaf) = newest_leaf(&loaded.records, &candidates) else {
            return Ok(None);
        };
        Ok(Some(chain(&loaded.records, &leaf)))
    }
}

fn raw_claude_record_at_lines(path: &Path) -> HashMap<u64, (String, String)> {
    std::fs::read_to_string(path)
        .expect("read raw Claude transcript")
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let kind = value.get("type")?.as_str()?;
            if !matches!(kind, "user" | "assistant") {
                return None;
            }
            let uuid = value.get("uuid")?.as_str()?;
            Some((index as u64 + 1, (kind.to_string(), uuid.to_string())))
        })
        .collect()
}

/// Collapse split IR events back to the native records the vendor oracle uses.
fn ags_replayed_claude_records(path: &Path) -> anyhow::Result<Vec<String>> {
    let raw = raw_claude_record_at_lines(path);
    let ir = claude_code_ir::read(path)?;
    let plan = resolve(&ir);
    let by_id: HashMap<&str, _> = ir
        .events
        .iter()
        .map(|event| (event.id.as_str(), event))
        .collect();
    let mut records = Vec::new();
    let mut previous_line = None;

    for id in &plan.events {
        let Some(event) = by_id.get(id.as_str()) else {
            continue;
        };
        if previous_line == Some(event.source.line) {
            continue;
        }
        previous_line = Some(event.source.line);
        if let Some((_, uuid)) = raw.get(&event.source.line) {
            records.push(uuid.clone());
        }
    }
    Ok(records)
}

fn write_claude_vendor_relink_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("vendor-relink.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user",
            "sessionId": "vendor-relink",
            "uuid": "old",
            "parentUuid": null,
            "timestamp": "2026-07-28T00:00:01.000Z",
            "message": { "role": "user", "content": "old history" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "vendor-relink",
            "uuid": "preserved",
            "parentUuid": "old",
            "timestamp": "2026-07-28T00:00:02.000Z",
            "message": {
                "id": "msg-preserved",
                "role": "assistant",
                "content": [{ "type": "text", "text": "kept" }],
            },
        }),
        serde_json::json!({
            "type": "system",
            "subtype": "compact_boundary",
            "sessionId": "vendor-relink",
            "uuid": "boundary",
            "parentUuid": null,
            "timestamp": "2026-07-28T00:00:03.000Z",
            "compactMetadata": {
                "preservedMessages": {
                    "anchorUuid": "summary",
                    "uuids": ["preserved"],
                    "allUuids": ["preserved"],
                },
            },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "vendor-relink",
            "uuid": "inflight",
            "parentUuid": "old",
            "timestamp": "2026-07-28T00:00:04.000Z",
            "message": { "role": "user", "content": "one more thing" },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "vendor-relink",
            "uuid": "summary",
            "parentUuid": "boundary",
            "timestamp": "2026-07-28T00:00:05.000Z",
            "isCompactSummary": true,
            "message": { "role": "user", "content": "summary" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "vendor-relink",
            "uuid": "reply",
            "parentUuid": "inflight",
            "timestamp": "2026-07-28T00:00:06.000Z",
            "message": {
                "id": "msg-reply",
                "role": "assistant",
                "content": [{ "type": "text", "text": "reply" }],
            },
        }),
        serde_json::json!({
            "type": "last-prompt",
            "sessionId": "vendor-relink",
            "leafUuid": "inflight",
            "lastPrompt": "one more thing",
        }),
    ];
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    (dir, path)
}

fn write_claude_agent_fork_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("agent-vendor-leaf.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user",
            "sessionId": "agent-vendor-leaf",
            "uuid": "root",
            "parentUuid": null,
            "isSidechain": true,
            "timestamp": "2026-07-28T00:00:01.000Z",
            "message": { "role": "user", "content": "start" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "agent-vendor-leaf",
            "uuid": "abandoned",
            "parentUuid": "root",
            "isSidechain": true,
            "timestamp": "2026-07-28T00:00:02.000Z",
            "message": {
                "id": "msg-abandoned",
                "role": "assistant",
                "content": [{ "type": "text", "text": "old branch" }],
            },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "agent-vendor-leaf",
            "uuid": "live-user",
            "parentUuid": "root",
            "isSidechain": true,
            "timestamp": "2026-07-28T00:00:03.000Z",
            "message": { "role": "user", "content": "new branch" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "agent-vendor-leaf",
            "uuid": "live-assistant",
            "parentUuid": "live-user",
            "isSidechain": true,
            "timestamp": "2026-07-28T00:00:04.000Z",
            "message": {
                "id": "msg-live",
                "role": "assistant",
                "content": [{ "type": "text", "text": "current branch" }],
            },
        }),
    ];
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    (dir, path)
}

fn write_claude_parallel_tool_results_fixture(leaf_uuid: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("parallel-tool-results.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user",
            "sessionId": "parallel-tool-results",
            "uuid": "root",
            "parentUuid": null,
            "timestamp": "2026-07-28T00:00:01.000Z",
            "message": { "role": "user", "content": "run both" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "parallel-tool-results",
            "uuid": "thinking",
            "parentUuid": "root",
            "timestamp": "2026-07-28T00:00:02.000Z",
            "message": {
                "id": "msg-parallel",
                "role": "assistant",
                "content": [{ "type": "thinking", "thinking": "plan" }],
            },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "parallel-tool-results",
            "uuid": "call-one",
            "parentUuid": "thinking",
            "timestamp": "2026-07-28T00:00:03.000Z",
            "message": {
                "id": "msg-parallel",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tool-one",
                    "name": "Read",
                    "input": {},
                }],
            },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "parallel-tool-results",
            "uuid": "call-two",
            "parentUuid": "call-one",
            "timestamp": "2026-07-28T00:00:04.000Z",
            "message": {
                "id": "msg-parallel",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tool-two",
                    "name": "Read",
                    "input": {},
                }],
            },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "parallel-tool-results",
            "uuid": "result-one",
            "parentUuid": "call-one",
            "timestamp": "2026-07-28T00:00:05.000Z",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool-one",
                    "content": "one",
                }],
            },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "parallel-tool-results",
            "uuid": "result-two",
            "parentUuid": "call-two",
            "timestamp": "2026-07-28T00:00:06.000Z",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool-two",
                    "content": "two",
                }],
            },
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "parallel-tool-results",
            "uuid": "abandoned-followup",
            "parentUuid": "call-one",
            "timestamp": "2026-07-28T00:00:07.000Z",
            "message": {
                "role": "user",
                "content": "this sibling was abandoned",
            },
        }),
        serde_json::json!({
            "type": "last-prompt",
            "sessionId": "parallel-tool-results",
            "leafUuid": leaf_uuid,
            "lastPrompt": "run both",
        }),
    ];
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    (dir, path)
}

fn write_claude_explicit_clear_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("explicit-clear.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user",
            "sessionId": "explicit-clear",
            "uuid": "old-user",
            "parentUuid": null,
            "timestamp": "2026-07-28T00:00:01.000Z",
            "message": { "role": "user", "content": "discard me" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "explicit-clear",
            "uuid": "old-assistant",
            "parentUuid": "old-user",
            "timestamp": "2026-07-28T00:00:02.000Z",
            "message": {
                "id": "msg-old",
                "role": "assistant",
                "content": [{ "type": "text", "text": "discard me too" }],
            },
        }),
        serde_json::json!({
            "type": "last-prompt",
            "sessionId": "explicit-clear",
            "leafUuid": null,
            "explicit": true,
        }),
    ];
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    (dir, path)
}

fn write_claude_non_explicit_head_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("non-explicit-head.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user",
            "sessionId": "non-explicit-head",
            "uuid": "root",
            "parentUuid": null,
            "timestamp": "2026-07-28T00:00:01.000Z",
            "message": { "role": "user", "content": "start" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "non-explicit-head",
            "uuid": "abandoned",
            "parentUuid": "root",
            "timestamp": "2026-07-28T00:00:02.000Z",
            "message": {
                "id": "msg-abandoned",
                "role": "assistant",
                "content": [{ "type": "text", "text": "old branch" }],
            },
        }),
        serde_json::json!({
            "type": "last-prompt",
            "sessionId": "non-explicit-head",
            "leafUuid": "root",
        }),
        serde_json::json!({
            "type": "user",
            "sessionId": "non-explicit-head",
            "uuid": "live-user",
            "parentUuid": "root",
            "timestamp": "2026-07-28T00:00:03.000Z",
            "message": { "role": "user", "content": "new branch" },
        }),
        serde_json::json!({
            "type": "assistant",
            "sessionId": "non-explicit-head",
            "uuid": "live-assistant",
            "parentUuid": "live-user",
            "timestamp": "2026-07-28T00:00:04.000Z",
            "message": {
                "id": "msg-live",
                "role": "assistant",
                "content": [{ "type": "text", "text": "current branch" }],
            },
        }),
    ];
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    (dir, path)
}

/// A compaction summary is the anchor *before* the preserved tail.
///
/// File order cannot answer this: the in-flight user message was appended
/// before the summary. Claude Code's `rsp` rewrites the graph on load, making
/// the summary the preserved record's parent, and `Bze` then returns the order
/// below. This synthetic case keeps that vendor rule runnable without a private
/// corpus.
#[test]
fn claude_compaction_replay_matches_vendor_record_order() {
    let (_dir, path) = write_claude_vendor_relink_fixture();
    let expected = claude_vendor_oracle::replayed_conversation(&path)
        .expect("vendor oracle reads fixture")
        .expect("vendor oracle finds a leaf");
    assert_eq!(expected, ["summary", "preserved", "inflight", "reply"]);

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert_eq!(
        actual, expected,
        "ags replay must agree record-for-record with Claude Code's loader"
    );
}

/// Subagent transcripts carry no `last-prompt`; newest-leaf selection still
/// applies. Treating the missing hint as "there are no forks" keeps every
/// sibling branch, while Claude Code's `V` + `DBe` selects the newest
/// conversational leaf.
#[test]
fn claude_agent_replay_matches_vendor_newest_leaf() {
    let (_dir, path) = write_claude_agent_fork_fixture();
    let expected = claude_vendor_oracle::replayed_conversation(&path)
        .expect("vendor oracle reads fixture")
        .expect("vendor oracle finds a leaf");
    assert_eq!(expected, ["root", "live-user", "live-assistant"]);

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert_eq!(
        actual, expected,
        "absence of `last-prompt` is not permission to keep every agent branch"
    );
}

/// One Anthropic response can issue parallel tools. Their result records are
/// sibling branches in the raw DAG, but Claude Code's `HB_` restores every
/// result associated with the response's shared `message.id`.
#[test]
fn claude_parallel_tool_results_match_vendor_recovery() {
    let (_dir, path) = write_claude_parallel_tool_results_fixture("result-two");
    let expected = claude_vendor_oracle::replayed_conversation(&path)
        .expect("vendor oracle reads fixture")
        .expect("vendor oracle finds a leaf");
    assert_eq!(
        expected,
        [
            "root",
            "thinking",
            "call-one",
            "call-two",
            "result-one",
            "result-two",
        ]
    );

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert_eq!(
        actual, expected,
        "a parallel tool result is response context, not an abandoned fork"
    );
}

#[test]
fn claude_parallel_tool_results_keep_a_selected_followup() {
    let (_dir, path) = write_claude_parallel_tool_results_fixture("abandoned-followup");
    let expected = claude_vendor_oracle::replayed_conversation(&path)
        .expect("vendor oracle reads fixture")
        .expect("vendor oracle finds a leaf");
    assert_eq!(
        expected,
        [
            "root",
            "thinking",
            "call-one",
            "call-two",
            "result-one",
            "result-two",
            "abandoned-followup",
        ]
    );

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert_eq!(
        actual, expected,
        "the selected followup belongs after its complete parallel tool response"
    );
}

/// An explicit null `last-prompt` is Claude Code's "clear to empty" command.
///
/// It is stronger than the absence of a branch hint: vendor `PBe` returns no
/// leaves at all, so retaining the preceding messages would resurrect history
/// the user deliberately removed.
#[test]
fn claude_explicit_clear_matches_vendor_empty_replay() {
    let (_dir, path) = write_claude_explicit_clear_fixture();
    let expected =
        claude_vendor_oracle::replayed_conversation(&path).expect("vendor oracle reads fixture");
    assert_eq!(expected, None, "vendor clears every replay leaf");

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert!(
        actual.is_empty(),
        "ags must not replay history after an explicit clear: {actual:?}"
    );
}

/// A non-explicit `last-prompt` is only a hint.
///
/// If later main-chain records descend from that hint, vendor `V` advances to
/// the newest record before choosing the live branch. Treating the hint as an
/// explicit head retains an abandoned sibling below it.
#[test]
fn claude_non_explicit_head_advances_with_vendor_main_chain() {
    let (_dir, path) = write_claude_non_explicit_head_fixture();
    let expected = claude_vendor_oracle::replayed_conversation(&path)
        .expect("vendor oracle reads fixture")
        .expect("vendor oracle finds a leaf");
    assert_eq!(expected, ["root", "live-user", "live-assistant"]);

    let actual = ags_replayed_claude_records(&path).expect("ags reads fixture");
    assert_eq!(
        actual, expected,
        "a non-explicit head must not keep an abandoned descendant branch"
    );
}

/// Diff ags against Claude Code's own PBe+rsp+V+Bze reconstruction.
///
/// Unlike the aggregate tests above, this is an oracle: each expected record
/// comes from an independent reimplementation of the vendor's graph rewrite
/// and leaf walk. The corpus remains private and read-only.
#[test]
#[ignore = "requires a local Claude corpus; set AGS_CLAUDE_CORPUS"]
fn claude_replay_matches_vendor_record_for_record() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut checked = 0usize;
    let mut no_leaf = 0usize;
    let mut mismatches = Vec::new();
    for path in &files {
        let expected = claude_vendor_oracle::replayed_conversation(path)
            .unwrap_or_else(|error| panic!("{}: vendor oracle: {error}", path.display()));
        let Some(expected) = expected else {
            no_leaf += 1;
            continue;
        };
        let Ok(actual) = ags_replayed_claude_records(path) else {
            continue;
        };
        checked += 1;
        if actual == expected {
            continue;
        }

        let first = actual
            .iter()
            .zip(&expected)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or_else(|| actual.len().min(expected.len()));
        let expected_set: HashSet<&str> = expected.iter().map(String::as_str).collect();
        let actual_set: HashSet<&str> = actual.iter().map(String::as_str).collect();
        let missing: Vec<&str> = expected
            .iter()
            .map(String::as_str)
            .filter(|uuid| !actual_set.contains(uuid))
            .take(5)
            .collect();
        let extra: Vec<&str> = actual
            .iter()
            .map(String::as_str)
            .filter(|uuid| !expected_set.contains(uuid))
            .take(5)
            .collect();
        mismatches.push(format!(
            "{}: first difference at record {first} (vendor={:?}, ags={:?}); \
             vendor={}, ags={}, missing={missing:?}, extra={extra:?}",
            path.display(),
            expected.get(first),
            actual.get(first),
            expected.len(),
            actual.len()
        ));
    }

    println!("{checked} transcripts checked, {no_leaf} with no vendor leaf");
    assert!(checked > 0, "no Claude transcript produced a vendor replay");
    assert!(
        mismatches.is_empty(),
        "{} of {checked} transcripts differ from Claude Code:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
