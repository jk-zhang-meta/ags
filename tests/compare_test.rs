//! The structural comparator against real sessions.
//!
//! [`casr::compare`] is the structured track's read-back oracle, and an oracle
//! is only worth what the sessions it has been pointed at are worth. Fixtures
//! prove it runs; 592 Codex rollouts and 175 Claude transcripts prove it calls
//! a correct same-agent write clean and a correct cross-agent write correct —
//! which is the only interesting property, because a comparator that flags
//! every legitimate vendor-boundary drop as damage gets switched off, and a
//! switched-off verifier is the gap it was written to close.
//!
//! Same contract as `replay_test.rs` and `roundtrip_ir_test.rs`: the corpus
//! tests are `#[ignore]`d because the corpus is machine-local and private, and
//! they skip rather than fail when it is absent. Run them explicitly:
//!
//! ```bash
//! AGSX_CODEX_CORPUS="$HOME/.codex/sessions" \
//! AGSX_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --release --test compare_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read. Every write in this file goes to a temp file;
//! nothing here touches `~/.codex` or `~/.claude`.
//!
//! # Two-sided, like the flat allowances
//!
//! Every cross-agent case below asserts that the predicted losses actually
//! happened as well as that nothing else did. An allowance that stops being
//! exercised does not stay neutral: it silently widens until it covers a real
//! regression. `tests/real_world_roundtrip_test.rs` makes the same argument for
//! the flat round trips; [`casr::compare::Comparison::carried_foreign`] makes it
//! inside the comparator itself, for the individual capsule.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use casr::compare::{Comparison, compare, vendor_of};
use casr::ir::{Fidelity, LossKind, SessionIr};
use casr::providers::{claude_code_ir, claude_code_ir_write, codex_ir, codex_ir_write};

// ---------------------------------------------------------------------------
// Corpus discovery (same discriminators as `roundtrip_ir_test.rs`)
// ---------------------------------------------------------------------------

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

/// The Claude projects tree also holds workflow journals that share the
/// extension; only uuid-named files and `agent-*` are transcripts.
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

// ---------------------------------------------------------------------------
// Write to a temp file, read it back, compare
// ---------------------------------------------------------------------------

/// The two agents on the structured track, as the comparator sees them.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Agent {
    slug: &'static str,
    vendor: &'static str,
}

const CODEX: Agent = Agent {
    slug: "codex",
    vendor: "openai",
};
const CLAUDE: Agent = Agent {
    slug: "claude-code",
    vendor: "anthropic",
};

/// Render `source` for `target`, parse the result, and compare the two.
///
/// The write goes through the same renderer the provider's `write_session_ir`
/// uses, so what is compared is what would land on disk. `None` when the replay
/// is empty and there is nothing to write.
fn crossing(source: &SessionIr, target: Agent) -> Option<(Comparison, Fidelity)> {
    let session = "compare-session";
    let now = chrono::Utc::now();
    let (lines, claimed) = if target == CODEX {
        let rendered = codex_ir_write::render(source, session, now)?;
        (rendered.lines, rendered.fidelity)
    } else {
        let rendered = claude_code_ir_write::render(source, session, now)?;
        (rendered.lines, rendered.fidelity)
    };

    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    for line in &lines {
        writeln!(file, "{line}").expect("write");
    }
    file.flush().expect("flush");
    let read = if target == CODEX {
        codex_ir::read
    } else {
        claude_code_ir::read
    };
    let written = read(file.path()).unwrap_or_else(|error| {
        panic!("the writer produced a session its own reader rejects: {error}")
    });

    Some((compare(source, &written, target.vendor), claimed))
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Totals {
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
    /// Grades the comparator derived, against grades the writers claimed.
    observed: BTreeMap<String, usize>,
    claimed: BTreeMap<String, usize>,
    /// Sessions whose grade the written file does not support, with both grades.
    overclaimed: Vec<(String, Fidelity, Fidelity)>,
    /// Sessions the comparator called damaged.
    damaged: Vec<String>,
}

impl Totals {
    fn add(&mut self, path: &Path, report: &Comparison, claimed: Fidelity) {
        self.sessions += 1;
        self.source_events += report.source_events;
        self.target_events += report.target_events;
        self.added_events += report.added_events;
        self.source_capsules += report.source_capsules;
        self.target_capsules += report.target_capsules;
        for (bucket, losses) in [
            (&mut self.predicted, &report.predicted),
            (&mut self.degraded, &report.degraded),
            (&mut self.unexplained, &report.unexplained),
        ] {
            for loss in losses {
                *bucket.entry(format!("{:?}", loss.kind)).or_insert(0) += loss.events;
            }
        }
        self.carried_foreign += report
            .carried_foreign
            .iter()
            .map(|carry| carry.capsules)
            .sum::<usize>();

        let observed = report.fidelity();
        *self.observed.entry(format!("{observed:?}")).or_insert(0) += 1;
        *self.claimed.entry(format!("{claimed:?}")).or_insert(0) += 1;
        if observed > claimed {
            self.overclaimed
                .push((path.display().to_string(), claimed, observed));
        }
        if !report.is_clean() {
            self.damaged
                .push(format!("{}: {}", path.display(), report.damage_detail()));
        }
    }

    fn report(&self, label: &str) {
        println!("\n=== {label}: {} sessions", self.sessions);
        println!(
            "  model events   {} -> {}   (+{} added by the target)",
            self.source_events, self.target_events, self.added_events
        );
        println!(
            "  capsules       {} -> {}",
            self.source_capsules, self.target_capsules
        );
        for (name, bucket) in [
            ("predicted", &self.predicted),
            ("degraded", &self.degraded),
            ("UNEXPLAINED", &self.unexplained),
        ] {
            if bucket.is_empty() {
                println!("  {name:<12} none");
            }
            for (kind, events) in bucket {
                println!("  {name:<12} {kind:<16} {events:>7} event(s)");
            }
        }
        println!("  foreign capsules carried across: {}", self.carried_foreign);
        println!("  grade claimed by writer:   {:?}", self.claimed);
        println!("  grade the file supports:   {:?}", self.observed);
        if !self.overclaimed.is_empty() {
            let worst = self
                .overclaimed
                .iter()
                .map(|(_, claimed, observed)| (*claimed, *observed))
                .max_by_key(|(_, observed)| *observed)
                .expect("checked non-empty");
            println!(
                "  {} session(s) graded better than the file supports, worst {:?} -> {:?}, e.g. {}",
                self.overclaimed.len(),
                worst.0,
                worst.1,
                self.overclaimed[0].0
            );
        }
    }

    /// Nothing was lost that nothing predicted.
    fn assert_no_damage(&self) {
        assert!(
            self.damaged.is_empty(),
            "{} session(s) lost content nothing predicted:\n{}",
            self.damaged.len(),
            self.damaged.join("\n")
        );
    }

    /// No writer claimed a better grade than its own output supports.
    fn assert_no_overclaim(&self) {
        assert!(
            self.overclaimed.is_empty(),
            "{} session(s) were graded better than the written file supports:\n{}",
            self.overclaimed.len(),
            self.overclaimed
                .iter()
                .map(|(path, claimed, observed)| format!(
                    "{path}: claimed {claimed:?}, supports {observed:?}"
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

}

// ---------------------------------------------------------------------------
// Same-agent: the comparator must find nothing at all
// ---------------------------------------------------------------------------

/// Codex → Codex conserves every model-visible event and every capsule.
///
/// The measured ground truth for this corpus is 592 rollouts, 94,583 model
/// events and 29,941 capsules, all conserved exactly. The assertions below are
/// on conservation rather than on those literals, so the test stays true as the
/// corpus grows; the numbers are printed so a drift is visible.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_into_itself_is_clean() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Totals::default();
    for path in &files {
        let Ok(source) = codex_ir::read(path) else {
            continue;
        };
        let Some((report, claimed)) = crossing(&source, CODEX) else {
            continue;
        };
        totals.add(path, &report, claimed);
    }

    totals.report("codex -> codex");
    assert!(totals.sessions > 0, "no Codex rollouts round-tripped");
    totals.assert_no_damage();
    totals.assert_no_overclaim();
    assert_eq!(totals.source_events, totals.target_events);
    assert_eq!(
        totals.source_capsules, totals.target_capsules,
        "a same-vendor capsule must survive verbatim"
    );
    assert!(
        totals.predicted.is_empty(),
        "nothing crosses a vendor boundary same-agent, so nothing may be predicted lost: {:?}",
        totals.predicted
    );
    assert!(
        totals.degraded.is_empty(),
        "a same-agent write reproduces its own shapes: {:?}",
        totals.degraded
    );
    assert_eq!(totals.added_events, 0);
}

/// Claude Code → Claude Code conserves every model-visible event and capsule.
///
/// Measured ground truth: 175 transcripts, 20,073 events, 3,897 capsules.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_into_itself_is_clean() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Totals::default();
    for path in &files {
        let Ok(source) = claude_code_ir::read(path) else {
            continue;
        };
        let Some((report, claimed)) = crossing(&source, CLAUDE) else {
            continue;
        };
        totals.add(path, &report, claimed);
    }

    totals.report("claude -> claude");
    assert!(totals.sessions > 0, "no Claude transcripts round-tripped");
    totals.assert_no_damage();
    totals.assert_no_overclaim();
    assert_eq!(totals.source_events, totals.target_events);
    assert_eq!(totals.source_capsules, totals.target_capsules);
    assert!(totals.predicted.is_empty(), "{:?}", totals.predicted);
    assert!(totals.degraded.is_empty(), "{:?}", totals.degraded);
    assert_eq!(totals.added_events, 0);
}

// ---------------------------------------------------------------------------
// Cross-agent: the losses must be the predicted ones, and they must happen
// ---------------------------------------------------------------------------

/// Codex → Claude Code loses reasoning and sealed history, and the comparator
/// classifies both as predicted rather than as damage.
///
/// Measured ground truth: 28,254 reasoning events and 352 sealed contexts go,
/// and nothing else. Every one of those is a capsule
/// [`casr::ir::Capsule::fits`] said an Anthropic target cannot read, which is
/// the entire distinction this comparator exists to draw — a verifier that
/// called this conversion damaged would be wrong about the case that matters.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_to_claude_loses_only_what_fits_predicted() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Totals::default();
    for path in &files {
        let Ok(source) = codex_ir::read(path) else {
            continue;
        };
        let Some((report, claimed)) = crossing(&source, CLAUDE) else {
            continue;
        };
        totals.add(path, &report, claimed);
    }

    totals.report("codex -> claude");
    assert!(totals.sessions > 0);
    totals.assert_no_damage();
    totals.assert_no_overclaim();
    assert_eq!(
        totals.target_capsules, 0,
        "no OpenAI blob is replayable in a Claude transcript"
    );
    assert_eq!(
        totals.carried_foreign, 0,
        "and none of them may be written into one anyway"
    );
    // Two-sided: the allowance has to be exercised or it is covering nothing.
    assert!(
        totals.predicted.contains_key("Reasoning"),
        "the corpus has 28,254 sealed reasoning events; none was reported as \
         predicted-not-carried, so the classification is no longer being tested"
    );
    assert!(
        totals.predicted.contains_key("SealedContext"),
        "the corpus has 352 sealed compactions; the one loss that deletes \
         conversation rather than train of thought went unclassified"
    );
}

/// Claude Code → Codex loses the thinking signatures and nothing else.
///
/// The asymmetry is real: Claude has one calling convention, so nothing is
/// downgraded on the way out, and Claude never seals its history, so no
/// conversation goes missing.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_to_codex_loses_only_the_thinking_signatures() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut totals = Totals::default();
    for path in &files {
        let Ok(source) = claude_code_ir::read(path) else {
            continue;
        };
        let Some((report, claimed)) = crossing(&source, CODEX) else {
            continue;
        };
        totals.add(path, &report, claimed);
    }

    // Strict. This was briefly weakened to `assert_overclaim_no_worse_than`
    // because `codex_ir_write::Writer::summarise` pushed a `ConversationOnly`
    // loss for every dropped structured companion without calling `degrade`, so
    // 126 of these sessions reported a grade one rung better than their own loss
    // list said. The writers now derive the grade *from* the loss list rather
    // than accumulating it alongside, which removes the class rather than the
    // instance — so the bound goes back to exact.
    totals.assert_no_overclaim();
    assert_eq!(
        totals.target_capsules, 0,
        "no Anthropic signature is replayable in a Codex rollout"
    );
    assert_eq!(totals.carried_foreign, 0);
    assert!(
        !totals.predicted.contains_key("SealedContext"),
        "Claude seals no history, so nothing here may be classified as losing any"
    );
    assert!(
        totals.predicted.contains_key("Reasoning"),
        "the corpus has 3,897 thinking signatures; none was reported as \
         predicted-not-carried, so the classification is no longer being tested"
    );
}

// ---------------------------------------------------------------------------
// Fixtures — these run in the normal suite
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/real_world")
        .join(name)
}

fn codex_fixture() -> SessionIr {
    codex_ir::read(&fixture("codex_real_world_sanitized.jsonl")).expect("codex fixture parses")
}

fn claude_fixture() -> SessionIr {
    claude_code_ir::read(&fixture("cc_real_world_sanitized.jsonl")).expect("cc fixture parses")
}

#[test]
fn the_fixtures_are_worth_comparing() {
    let codex = codex_fixture();
    assert!(
        codex.model_visible().len() >= 8,
        "a fixture with an empty replay proves nothing about a comparator"
    );
    assert!(
        codex
            .model_visible()
            .iter()
            .any(|event| !event.capsules.is_empty()),
        "the Codex fixture must carry sealed material or the whole \
         predicted-not-carried classification goes untested here"
    );
    assert!(claude_fixture().model_visible().len() >= 8);
}

#[test]
fn a_codex_fixture_survives_itself_intact() {
    let source = codex_fixture();
    let (report, claimed) = crossing(&source, CODEX).expect("non-empty replay");

    assert!(report.is_clean(), "{}", report.damage_detail());
    assert!(report.predicted.is_empty(), "{:?}", report.predicted);
    assert!(report.degraded.is_empty(), "{:?}", report.degraded);
    assert_eq!(report.source_events, report.target_events);
    assert_eq!(report.source_capsules, report.target_capsules);
    assert!(report.source_capsules > 0);
    assert_eq!(report.fidelity(), Fidelity::ContextComplete);
    assert_eq!(claimed, Fidelity::ContextComplete);
}

#[test]
fn a_claude_fixture_survives_itself_intact() {
    let source = claude_fixture();
    let (report, claimed) = crossing(&source, CLAUDE).expect("non-empty replay");

    assert!(report.is_clean(), "{}", report.damage_detail());
    assert!(report.predicted.is_empty(), "{:?}", report.predicted);
    assert!(report.degraded.is_empty(), "{:?}", report.degraded);
    assert_eq!(report.fidelity(), Fidelity::ContextComplete);
    assert_eq!(claimed, Fidelity::ContextComplete);
}

/// The case the comparator exists for: a real cross-agent write is *correct*
/// while losing material, and the report has to say which.
#[test]
fn a_codex_fixture_crossing_to_claude_loses_only_its_reasoning() {
    let source = codex_fixture();
    let sealed = source
        .model_visible()
        .iter()
        .map(|event| event.capsules.len())
        .sum::<usize>();
    let (report, claimed) = crossing(&source, CLAUDE).expect("non-empty replay");

    assert!(
        report.is_clean(),
        "a write that loses exactly what `fits()` forbade is correct: {}",
        report.damage_detail()
    );
    assert_eq!(report.target_capsules, 0);
    assert_eq!(report.carried_foreign, Vec::new());
    let reasoning = report
        .predicted
        .iter()
        .find(|loss| loss.kind == LossKind::Reasoning)
        .expect("the fixture's sealed reasoning must be reported as predicted-not-carried");
    assert_eq!(reasoning.events, sealed);
    assert_eq!(reasoning.grade, Fidelity::ContextNoReasoning);
    assert_eq!(report.fidelity(), Fidelity::ContextNoReasoning);
    assert!(
        claimed >= report.fidelity(),
        "the writer graded {claimed:?}, better than the file supports"
    );
}

#[test]
fn a_claude_fixture_crossing_to_codex_keeps_its_conversation() {
    let source = claude_fixture();
    let (report, claimed) = crossing(&source, CODEX).expect("non-empty replay");

    assert!(report.is_clean(), "{}", report.damage_detail());
    assert!(report.unexplained.is_empty());
    assert!(
        claimed >= report.fidelity(),
        "the writer graded {claimed:?}, better than the file supports"
    );
}

#[test]
fn the_comparator_only_knows_the_two_structured_agents() {
    assert_eq!(vendor_of(CODEX.slug), Some(CODEX.vendor));
    assert_eq!(vendor_of(CLAUDE.slug), Some(CLAUDE.vendor));
    assert_eq!(
        vendor_of("gemini"),
        None,
        "guessing a vendor would classify every capsule as foreign and turn a \
         correct conversion into a verification failure"
    );
}

