//! Regression tests against a real local session corpus.
//!
//! Fixtures prove a parser handles the shapes someone thought to write down.
//! They cannot prove it handles the shapes an agent actually emits, which is
//! how a reader ends up sourcing reasoning from an event type that no longer
//! exists and reporting success while producing nothing.
//!
//! These tests read a directory of genuine sessions and assert on aggregate
//! properties. They are `#[ignore]`d because the corpus is machine-local and
//! private; run them explicitly:
//!
//! ```bash
//! AGSX_CODEX_CORPUS="$HOME/.codex/sessions" \
//!   cargo test --release --test corpus_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read. Nothing here writes to it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use casr::ir::{Body, CapsuleKind, SessionIr};
use casr::providers::{claude_code_ir, codex_ir};

/// Collect up to `limit` session files under the corpus named by `env_var`.
///
/// Returns an empty vec when the variable is unset so the harness can skip
/// rather than fail on a machine without the corpus.
fn corpus_files(env_var: &str, extension: &str, limit: usize) -> Vec<PathBuf> {
    let Ok(root) = std::env::var(env_var) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(extension))
        .collect();
    files.sort();
    files.truncate(limit);
    files
}

struct Totals {
    files: usize,
    parsed: usize,
    failed: Vec<(PathBuf, String)>,
    events: u64,
    by_kind: BTreeMap<String, u64>,
    unknown: u64,
    capsules: u64,
    /// Capsules broken down by the body kind that carries them. Codex seals
    /// three different things and they must be accounted for separately, or a
    /// leak in one hides behind a surplus in another.
    capsules_by_kind: BTreeMap<String, u64>,
    unknown_types: BTreeMap<String, u64>,
    with_compaction: usize,
}

fn scan_codex(limit: usize) -> Option<Totals> {
    scan("AGSX_CODEX_CORPUS", limit, codex_ir::read, |_| true)
}

fn scan_claude(limit: usize) -> Option<Totals> {
    scan(
        "AGSX_CLAUDE_CORPUS",
        limit,
        claude_code_ir::read,
        is_claude_transcript,
    )
}

/// Whether a `.jsonl` under the Claude projects tree is actually a transcript.
///
/// The tree also holds files that merely share the extension — workflow
/// journals under `subagents/workflows/` are the case that surfaced here.
/// Claude names a transcript after its session uuid, and a subagent transcript
/// `agent-<id>.jsonl`, so the filename is the discriminator.
///
/// Filtering here rather than loosening the reader is deliberate: a reader that
/// shrugs at a file with no `sessionId` cannot tell a foreign file from a
/// corrupt session, and would happily invent an id for either.
fn is_claude_transcript(path: &std::path::Path) -> bool {
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

fn scan(
    env_var: &str,
    limit: usize,
    read: fn(&std::path::Path) -> anyhow::Result<SessionIr>,
    keep: fn(&std::path::Path) -> bool,
) -> Option<Totals> {
    // Over-collect, then filter, so `limit` counts real transcripts rather
    // than whatever happened to share the extension.
    let files: Vec<PathBuf> = corpus_files(env_var, "jsonl", limit.saturating_mul(4))
        .into_iter()
        .filter(|path| keep(path))
        .take(limit)
        .collect();
    if files.is_empty() {
        eprintln!("{env_var} unset or empty; skipping");
        return None;
    }

    let mut totals = Totals {
        files: files.len(),
        parsed: 0,
        failed: Vec::new(),
        events: 0,
        by_kind: BTreeMap::new(),
        unknown: 0,
        capsules: 0,
        capsules_by_kind: BTreeMap::new(),
        unknown_types: BTreeMap::new(),
        with_compaction: 0,
    };

    for path in files {
        match read(&path) {
            Ok(ir) => {
                totals.parsed += 1;
                totals.events += ir.events.len() as u64;
                totals.unknown += ir.capture.unknown;
                totals.capsules += ir.capture.capsules;
                for (kind, count) in &ir.capture.by_kind {
                    *totals.by_kind.entry(kind.clone()).or_insert(0) += count;
                }
                if ir
                    .events
                    .iter()
                    .any(|event| matches!(event.body, Body::Compaction { .. }))
                {
                    totals.with_compaction += 1;
                }
                for event in &ir.events {
                    if !event.capsules.is_empty() {
                        *totals
                            .capsules_by_kind
                            .entry(event.body.kind().to_string())
                            .or_insert(0) += event.capsules.len() as u64;
                    }
                    if let Body::Unknown { native_type, .. } = &event.body {
                        let key = native_type.clone().unwrap_or_else(|| "<malformed>".into());
                        *totals.unknown_types.entry(key).or_insert(0) += 1;
                    }
                }
            }
            Err(error) => totals.failed.push((path, error.to_string())),
        }
    }
    Some(totals)
}

fn report(totals: &Totals) {
    println!("files            {} ({} parsed)", totals.files, totals.parsed);
    println!("events           {}", totals.events);
    println!("capsules         {}", totals.capsules);
    println!(
        "with compaction  {} / {} ({}%)",
        totals.with_compaction,
        totals.parsed,
        if totals.parsed == 0 {
            0
        } else {
            totals.with_compaction * 100 / totals.parsed
        }
    );
    println!("by kind:");
    for (kind, count) in &totals.by_kind {
        println!("  {kind:<14} {count}");
    }
    if !totals.unknown_types.is_empty() {
        println!("unknown native types (format drift):");
        for (kind, count) in &totals.unknown_types {
            println!("  {kind:<40} {count}");
        }
    }
    for (path, error) in &totals.failed {
        println!("FAILED {}: {error}", path.display());
    }
}

#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_corpus_parses_without_unknown_events() {
    let Some(totals) = scan_codex(400) else {
        return;
    };
    report(&totals);

    assert!(
        totals.failed.is_empty(),
        "{} of {} rollouts failed to parse",
        totals.failed.len(),
        totals.files
    );

    // The whole point of `Body::Unknown` is that drift is visible rather than
    // silent. A non-zero count here is not a crash, it is a to-do: some line
    // shape is not yet mapped, and the report above names it.
    assert_eq!(
        totals.unknown, 0,
        "unmapped native line shapes found; see the 'unknown native types' list above"
    );
}

#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_corpus_preserves_reasoning_capsules() {
    let Some(totals) = scan_codex(400) else {
        return;
    };
    let reasoning = totals.by_kind.get("reasoning").copied().unwrap_or(0);
    let sealed_context = totals.by_kind.get("sealed_context").copied().unwrap_or(0);
    let carried = |kind: &str| totals.capsules_by_kind.get(kind).copied().unwrap_or(0);
    println!("capsules {} by carrier:", totals.capsules);
    for (kind, count) in &totals.capsules_by_kind {
        println!("  {kind:<15}{count}");
    }

    // Codex seals three different things, in three different places, and each
    // one was being dropped by a different bug. Assert them separately: a
    // single total lets a new leak hide behind an unrelated surplus.
    assert_eq!(
        carried("reasoning"),
        reasoning,
        "every reasoning item in the corpus carries `encrypted_content`; a \
         shortfall means sealed reasoning is being dropped"
    );
    assert_eq!(
        carried("sealed_context"),
        sealed_context,
        "a sealed compaction with no blob is an empty promise"
    );
    assert!(
        sealed_context > 0,
        "three quarters of real rollouts are compacted and Codex returns each \
         compacted history sealed; finding none means the compaction \
         replacement is being read as ordinary messages again"
    );
    assert!(
        carried("message") > 0,
        "`agent_message` content carries `encrypted_content` blocks; finding \
         none means block-level content is being filtered away again"
    );
    assert_eq!(
        totals.capsules,
        totals.capsules_by_kind.values().sum::<u64>(),
        "capsule total and per-carrier breakdown disagree"
    );
}

/// The regression that motivated [`crate::ir::Body::SealedContext`].
///
/// Codex compaction entries of `type: "compaction"` have no `role` and no
/// `content`. Read as messages they become empty assistant turns, so a
/// compacted session replays as a preamble promising a summary that is not
/// there. This asserts the replay carries the blob instead.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_compacted_history_is_not_replayed_as_an_empty_message() {
    let files = corpus_files("AGSX_CODEX_CORPUS", "jsonl", 400);
    if files.is_empty() {
        eprintln!("AGSX_CODEX_CORPUS unset or empty; skipping");
        return;
    }

    let (mut checked, mut blank, mut sealed) = (0usize, 0usize, 0usize);
    for path in &files {
        let Ok(ir) = codex_ir::read(path) else { continue };
        if !ir
            .events
            .iter()
            .any(|event| matches!(event.body, Body::Compaction { .. }))
        {
            continue;
        }
        checked += 1;
        for event in ir.model_visible() {
            match &event.body {
                Body::SealedContext { .. } => {
                    sealed += 1;
                    assert_eq!(
                        event.capsules.len(),
                        1,
                        "{}: sealed context {} carries no blob, which is the \
                         entire reason it exists",
                        path.display(),
                        event.id
                    );
                }
                Body::Message { blocks, .. } if blocks.is_empty() => blank += 1,
                _ => {}
            }
        }
    }

    assert!(checked > 0, "no compacted rollouts in the sample");
    assert!(
        sealed > 0,
        "{checked} compacted rollouts replayed nothing sealed"
    );
    assert_eq!(
        blank, 0,
        "{blank} empty messages in the replay of {checked} compacted rollouts — \
         sealed compaction entries are being fabricated into messages again"
    );
    println!("{checked} compacted rollouts: {sealed} sealed contexts, {blank} empty messages");
}

#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_corpus_compaction_shrinks_model_history() {
    let files = corpus_files("AGSX_CODEX_CORPUS", "jsonl", 400);
    if files.is_empty() {
        eprintln!("AGSX_CODEX_CORPUS unset or empty; skipping");
        return;
    }

    let mut checked = 0usize;
    for path in files {
        let Ok(ir) = codex_ir::read(&path) else {
            continue;
        };
        if !ir
            .events
            .iter()
            .any(|event| matches!(event.body, Body::Compaction { .. }))
        {
            continue;
        }
        checked += 1;

        // Not a count comparison: `model_visible()` expands each compaction
        // into its replacement history, so the result can legitimately be
        // larger than the number of raw model events. The invariant that
        // actually matters is that nothing a compaction superseded survives.
        let superseded: std::collections::HashSet<&str> = ir
            .events
            .iter()
            .filter_map(|event| match &event.body {
                Body::Compaction { supersedes, .. } => Some(supersedes),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        assert!(
            !superseded.is_empty(),
            "{}: a compaction that supersedes nothing means the scope was not computed",
            path.display()
        );

        for event in ir.model_visible() {
            assert!(
                !superseded.contains(event.id.as_str()),
                "{}: event {} was superseded by a compaction but is still being \
                 replayed to the target",
                path.display(),
                event.id
            );
        }
    }

    assert!(
        checked > 0,
        "no compacted rollouts in the sample; compaction handling went unverified"
    );
    println!("verified compaction scoping on {checked} rollouts");
}

#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_largest_rollout_stays_within_memory_budget() {
    let Ok(root) = std::env::var("AGSX_CODEX_CORPUS") else {
        eprintln!("AGSX_CODEX_CORPUS unset; skipping");
        return;
    };
    let largest = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|entry| entry.metadata().ok().map(|meta| (meta.len(), entry.into_path())))
        .max_by_key(|(size, _)| *size);

    let Some((size, path)) = largest else {
        eprintln!("no rollouts found; skipping");
        return;
    };
    println!("largest rollout: {} ({} MiB)", path.display(), size / 1_048_576);

    // Not a memory assertion — that needs an allocator hook — but a real
    // guard against the parser falling over on the tail of the size
    // distribution, which is where converters that buffer everything break.
    let ir = codex_ir::read(&path).expect("largest rollout must parse");
    println!(
        "parsed {} events, {} capsules, {} unknown",
        ir.events.len(),
        ir.capture.capsules,
        ir.capture.unknown
    );
    assert_eq!(ir.capture.unknown, 0);
    assert!(!ir.origin.native_session_id.is_empty());
    drop::<SessionIr>(ir);
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_corpus_parses_without_unknown_events() {
    let Some(totals) = scan_claude(200) else {
        return;
    };
    report(&totals);

    assert!(
        totals.failed.is_empty(),
        "{} of {} transcripts failed to parse",
        totals.failed.len(),
        totals.files
    );
    assert_eq!(
        totals.unknown, 0,
        "unmapped native record types found; see the list above"
    );
}

#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_corpus_preserves_thinking_signatures() {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", "jsonl", 400)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
        return;
    }

    let mut reasoning = 0u64;
    let mut sealed = 0u64;
    let mut empty_thinking = 0u64;
    for path in files {
        let Ok(ir) = claude_code_ir::read(&path) else {
            continue;
        };
        for event in &ir.events {
            if let Body::Reasoning { text, .. } = &event.body {
                reasoning += 1;
                if text.is_none() {
                    empty_thinking += 1;
                }
                sealed += event
                    .capsules
                    .iter()
                    .filter(|capsule| capsule.kind == CapsuleKind::AnthropicThinkingSignature)
                    .count() as u64;
            }
        }
    }
    println!("reasoning {reasoning}, sealed {sealed}, empty thinking text {empty_thinking}");

    assert!(reasoning > 0, "a real Claude corpus contains thinking blocks");
    // The signature is the reasoning. Losing it is the single largest fidelity
    // hole in every converter surveyed before this one.
    assert_eq!(
        sealed, reasoning,
        "{reasoning} thinking blocks but only {sealed} signatures carried"
    );
    // Corroborates the premise the capsule design rests on: the plaintext is
    // already gone at the source, so dropping the signature drops everything.
    assert_eq!(
        empty_thinking, reasoning,
        "some thinking blocks carried plaintext; the capsule rationale needs revisiting"
    );
}

#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_corpus_compaction_drops_only_unpreserved_messages() {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", "jsonl", 400)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
        return;
    }

    let mut checked = 0usize;
    for path in files {
        let Ok(ir) = claude_code_ir::read(&path) else {
            continue;
        };
        let superseded: std::collections::HashSet<&str> = ir
            .events
            .iter()
            .filter_map(|event| match &event.body {
                Body::Compaction { supersedes, .. } => Some(supersedes),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        if superseded.is_empty() {
            continue;
        }
        checked += 1;
        for event in ir.model_visible() {
            assert!(
                !superseded.contains(event.id.as_str()),
                "{}: {} was compacted away but is still replayed",
                path.display(),
                event.id
            );
        }
    }
    if checked == 0 {
        eprintln!("no compacted transcripts in the sample; compaction scoping unverified");
    } else {
        println!("verified compaction scoping on {checked} transcripts");
    }
}

/// `Event::id` is unique within the session, and Claude Code attacks that.
///
/// Across a `/compact` it re-appends the records it has to replay — the
/// unresolved `tool_use` and its `tool_result`s — under their original `uuid`s,
/// immediately before the `compact_boundary`. A reader minting one event per
/// line emits the same id twice, and `replay::resolve`'s `position` map,
/// `SessionIr::model_visible`'s `by_id` and `prune_forks`' record index all key
/// on it, so an arbitrary copy wins and the fidelity report double-counts the
/// other.
///
/// Three assertions, because a one-sided version of this passes on a reader that
/// simply had nothing to find:
///
/// 1. **No session contains a duplicate id.** The property itself.
/// 2. **The re-emission handling fires somewhere.** If it never does on a corpus
///    that contains compactions, it has stopped being exercised — and an
///    allowance that stops being exercised does not stay neutral, it widens
///    until it covers a regression.
/// 3. **Nothing is dropped for a *changed* record.** A re-emission whose content
///    differs is kept under a minted `<id>#dup<n>` and counted separately, so
///    `restated` can never be hiding a real edit.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_corpus_re_emissions_are_restated_not_duplicated() {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", "jsonl", 800)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
        return;
    }

    let mut sessions = 0u64;
    let mut events = 0u64;
    let mut restated = 0u64;
    let mut collisions = 0u64;
    let mut duplicated: Vec<(PathBuf, usize)> = Vec::new();
    for path in files {
        let Ok(ir) = claude_code_ir::read(&path) else {
            continue;
        };
        sessions += 1;
        events += ir.events.len() as u64;
        restated += ir.capture.restated;
        collisions += ir.capture.id_collisions;
        if ir.capture.restated > 0 || ir.capture.id_collisions > 0 {
            println!(
                "  {}: {} restated, {} collision(s)",
                path.display(),
                ir.capture.restated,
                ir.capture.id_collisions
            );
        }
        let distinct: std::collections::HashSet<&str> =
            ir.events.iter().map(|event| event.id.as_str()).collect();
        if distinct.len() != ir.events.len() {
            duplicated.push((path, ir.events.len() - distinct.len()));
        }
    }
    println!(
        "{sessions} transcripts, {events} events, {restated} restated, {collisions} id collision(s)"
    );

    assert!(
        duplicated.is_empty(),
        "{} transcript(s) emit a duplicate `Event::id`, e.g. {:?}",
        duplicated.len(),
        &duplicated[..duplicated.len().min(3)]
    );
    assert!(
        restated > 0,
        "no transcript in {sessions} re-emitted a record, so the restatement rule \
         ran on nothing and the assertion above proves only that this corpus had \
         no duplicates to find"
    );
    assert_eq!(
        collisions, 0,
        "a transcript reused an id for content that differs — nothing was dropped, \
         the record is still in `events` under a minted id, but Claude naming two \
         different things the same thing is worth reading the note above rather \
         than resolving quietly"
    );
}

#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_corpus_parent_links_resolve() {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", "jsonl", 400)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
        return;
    }

    let mut dangling = 0u64;
    let mut total = 0u64;
    for path in files {
        let Ok(ir) = claude_code_ir::read(&path) else {
            continue;
        };
        let ids: std::collections::HashSet<&str> = ir
            .events
            .iter()
            .map(|event| event.id.split('#').next().unwrap_or(&event.id))
            .collect();
        for event in &ir.events {
            let Some(parent) = &event.parent else { continue };
            total += 1;
            if !ids.contains(parent.as_str()) {
                dangling += 1;
            }
        }
    }
    println!("parent links {total}, dangling {dangling}");

    // Claude records real parent uuids, so the DAG should close. A few dangling
    // links are expected where a transcript was forked or truncated; a large
    // fraction would mean the envelope is being read wrong.
    assert!(
        total > 0 && (dangling * 100) / total < 5,
        "{dangling} of {total} parent links do not resolve"
    );
}
