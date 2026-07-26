//! The provider conformance suite: a thin driver over [`casr::conformance`].
//!
//! Everything about *what* conformance means lives in `src/conformance.rs`, so
//! that a provider author can call it directly. This file holds only what a test
//! can hold:
//!
//! - the two tiers, and which one actually ran;
//! - corpus discovery;
//! - the process-wide environment sandbox every structured write goes into;
//! - the assertions.
//!
//! There is no per-provider test body here, and adding a third structured
//! provider requires no edit to this file: the battery derives its subject list
//! from [`casr::discovery::ProviderRegistry::default_registry`] filtered by
//! `Provider::supports_structured_write`, and it derives which provider owns a
//! given session file from which structured reader claims it.
//!
//! # Two tiers, and it says which one ran
//!
//! The **fixtures** tier runs everywhere, on `tests/fixtures/`. The **corpus**
//! tier is `#[ignore]`d because the corpus is machine-local and private, and it
//! prints a skip reason rather than passing quietly:
//!
//! ```bash
//! AGSX_CODEX_CORPUS="$HOME/.codex/sessions" \
//! AGSX_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --release --test conformance_test -- --ignored --nocapture
//! ```
//!
//! Any `AGSX_<anything>_CORPUS` variable is picked up, by suffix rather than by
//! name, so a new provider's corpus root needs no edit here either.
//!
//! # The corpus is read-only
//!
//! Every structured write goes through `Provider::write_session_ir`, which puts
//! the file under the provider's own session root — `~/.codex/sessions`,
//! `~/.claude/projects`. [`sandboxed`] points `HOME` at a scratch directory and
//! removes every provider-specific home override in the ambient environment for
//! the duration, and the battery asserts that every path it wrote lands inside
//! that directory. Nothing here writes to the corpus.

mod test_env;

use std::path::{Path, PathBuf};

use casr::conformance::{self, HopReport, Report};

static ENV: test_env::EnvLock = test_env::EnvLock;

/// Restores one environment variable when it goes out of scope.
///
/// Owned key rather than `&'static str`, because the overrides being cleared are
/// discovered at run time rather than named in this file.
struct EnvGuard {
    key: String,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: guarded by `test_env::EnvLock` for the lifetime of the test.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            original,
        }
    }

    fn remove(key: &str) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        Self {
            key: key.to_string(),
            original,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: as above.
            Some(value) => unsafe { std::env::set_var(&self.key, value) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

/// Run `body` with every provider's session root redirected into a scratch dir.
///
/// `HOME` is the redirect: both structured providers derive their session root
/// from `dirs::home_dir()` unless a provider-specific override says otherwise,
/// so the overrides are cleared too — selected by suffix rather than by name, so
/// that a third provider's `FOO_HOME` is covered without an edit here. The
/// battery independently asserts that nothing was written outside the directory,
/// which is what turns a miss in this function into a loud failure rather than a
/// file in somebody's real session store.
fn sandboxed<T>(body: impl FnOnce(&Path) -> T) -> T {
    let _lock = ENV.lock().expect("env lock");
    let sandbox = tempfile::TempDir::new().expect("scratch directory");

    // Collected before anything is removed: mutating the environment while
    // iterating it is not something to rely on.
    let overrides: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key != "HOME" && key.ends_with("_HOME"))
        .collect();
    let _cleared: Vec<EnvGuard> = overrides.iter().map(|key| EnvGuard::remove(key)).collect();
    let _home = EnvGuard::set("HOME", sandbox.path());

    body(sandbox.path())
}

// ---------------------------------------------------------------------------
// Tier 1: fixtures. Runs everywhere.
// ---------------------------------------------------------------------------

fn fixture_files() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    files
}

/// The whole battery, on the checked-in fixtures.
///
/// This is the tier that runs on a machine with no corpus, so it has to be the
/// one that fails when a provider joins the structured track without bringing a
/// session its own reader can parse — the battery reports exactly that, per
/// provider, rather than quietly checking the other one twice.
#[test]
fn structured_providers_conform_on_the_fixtures() {
    let files = fixture_files();
    let report = sandboxed(|sandbox| conformance::run("fixtures", &files, sandbox));
    finish("fixtures", &report);
}

// ---------------------------------------------------------------------------
// Tier 2: the real corpus. Skips loudly.
// ---------------------------------------------------------------------------

/// Every root named by an `AGSX_<something>_CORPUS` variable.
///
/// Selected by shape rather than by name so that a new provider's corpus root
/// is picked up without an edit; the battery works out which reader owns each
/// file anyway.
fn corpus_roots() -> Vec<(String, PathBuf)> {
    let mut roots: Vec<(String, PathBuf)> = std::env::vars()
        .filter(|(key, value)| {
            key.starts_with("AGSX_") && key.ends_with("_CORPUS") && !value.trim().is_empty()
        })
        .map(|(key, value)| (key, PathBuf::from(value)))
        .collect();
    roots.sort();
    roots
}

/// Files under every corpus root, capped per root.
///
/// The cap exists because the largest single rollout in the local corpus is
/// 281 MiB and every source session is written once per structured target;
/// `AGSX_CONFORMANCE_LIMIT` raises or lowers it.
fn corpus_files() -> Vec<PathBuf> {
    let limit: usize = std::env::var("AGSX_CONFORMANCE_LIMIT")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(700);

    let mut all = Vec::new();
    for (key, root) in corpus_roots() {
        let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .collect();
        files.sort();
        eprintln!(
            "  {key}={} → {} file(s), taking {}",
            root.display(),
            files.len(),
            files.len().min(limit)
        );
        files.truncate(limit);
        all.extend(files);
    }
    all
}

/// The whole battery, on real sessions.
///
/// Fixtures prove the battery runs. Only the corpus establishes that a writer is
/// the inverse of its reader on the shapes an agent actually emits — sealed
/// compaction, freeform tool calls, `encrypted_content` buried inside an
/// `agent_message`, the record split that turns one native line into three
/// events. None of those is in a fixture.
#[test]
#[ignore = "requires a local session corpus; set AGSX_CODEX_CORPUS / AGSX_CLAUDE_CORPUS"]
fn structured_providers_conform_on_the_corpus() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!(
            "\n════ conformance tier \"corpus\": DID NOT RUN — no AGSX_*_CORPUS root is set, so \
             nothing below was checked against a real session. The fixtures tier is the only \
             evidence this run produced.\n"
        );
        return;
    }
    let report = sandboxed(|sandbox| conformance::run("corpus", &files, sandbox));
    finish("corpus", &report);
}

// ---------------------------------------------------------------------------
// The second hop, both tiers
// ---------------------------------------------------------------------------

/// The measurement the store exists for, on the checked-in fixtures.
///
/// A second conversion hop used to ask the session the user named rather than the
/// best source for its target, so `codex → claude → codex` came back with none of
/// the original reasoning capsules while the bytes that replay perfectly never
/// left `~/.codex/sessions`. This runs the chain both ways and prints what each
/// arm delivered.
///
/// The fixtures tier proves the chain runs. Only the corpus tier puts a number on
/// it worth quoting: a fixture carries a handful of capsules and a real rollout
/// carries thousands.
#[test]
fn the_second_hop_does_not_lose_what_the_store_could_have_supplied() {
    let files = fixture_files();
    let report = sandboxed(|sandbox| conformance::second_hop("fixtures", &files, sandbox));
    finish_hops("fixtures", &report);
}

/// The same chain on real sessions.
#[test]
#[ignore = "requires a local session corpus; set AGSX_CODEX_CORPUS / AGSX_CLAUDE_CORPUS"]
fn the_second_hop_recovers_the_corpus_capsules_the_first_hop_could_not_carry() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!(
            "\n════ second hop, tier \"corpus\": DID NOT RUN — no AGSX_*_CORPUS root is set, so \
             the payoff below was not measured against a real session.\n"
        );
        return;
    }
    let report = sandboxed(|sandbox| conformance::second_hop("corpus", &files, sandbox));
    finish_hops("corpus", &report);
}

/// Print the counts, then fail on the objections.
///
/// No number is asserted: they belong to whatever corpus is on the machine, and
/// baking one in would turn a suite that measures the store into one that
/// measures this laptop. What is asserted are properties, and they are not the
/// ones this suite used to assert.
///
/// It used to assert that consulting the store never delivers less sealed
/// material than not consulting it. That is now **false**, and it has to be:
/// correctly preferring an advanced derivative over an older-but-richer origin
/// delivers fewer capsules on purpose. The two properties that are true, and that
/// are the ones actually wanted, are that **the store may never deliver an
/// outcome the user would not have got without it** —
///
/// - conversation content is a floor and never a trade, because a turn exists in
///   one incarnation only and nothing can rebuild it; and
/// - sealed material is a floor unless it is *bought* with content, because a
///   capsule a derivative lacks is content its origin still holds
///
/// — with the old floor kept where it is still provably right: the arm where
/// nothing was appended anywhere, in which the origin is strictly better and the
/// store must deliver at least what `--no-store` does.
///
/// The per-chain half of those lives in `conformance::second_hop`, which can name
/// the file. What is left here is the tier-wide half, and one assertion that is
/// really about this suite rather than about the store: the appended arm has to
/// have run. A second-hop suite that only measures untouched intermediates is
/// measuring the one case where returning the origin is trivially correct.
fn finish_hops(tier: &str, report: &HopReport) {
    report.print();
    assert!(
        report.sessions() > 0,
        "the {tier} second-hop tier measured no chain at all, so nothing below was checked"
    );
    assert!(
        report.findings().is_empty(),
        "the {tier} second-hop tier found {} failure(s):\n  {}",
        report.findings().len(),
        report.findings().join("\n  ")
    );

    let untouched = report.untouched();
    let appended = report.appended();

    assert!(
        appended.sessions > 0,
        "the {tier} tier appended work to no intermediate at all, so every chain it measured was \
         the degenerate one where the intermediate is a lossy projection of the origin and \
         returning the origin is trivially correct"
    );
    // The floor has to be a floor of something. `store_kept_work ==
    // control_kept_work` is satisfied by `0 == 0`, which is the state where
    // *neither* arm delivered the work that was appended — the exact failure
    // this suite exists to detect, passing as agreement. So the control arm is
    // required to have found the marker at least once before the two are
    // compared. It found it in 13 of 13 fixture chains and 778 of 778 corpus
    // chains, so this asserts a property that holds rather than one that might.
    assert!(
        appended.control_kept_work > 0,
        "the {tier} tier's `--no-store` arm delivered the appended work in none of its {} \
         chain(s), so the floor the store is held to is the empty one and the comparison below \
         would pass on two zeroes",
        appended.sessions
    );
    assert_eq!(
        appended.store_kept_work, appended.control_kept_work,
        "the {tier} tier delivered the work appended to the intermediate in {} chain(s) without \
         the store and only {} with it; content is a floor, never a trade",
        appended.control_kept_work, appended.store_kept_work
    );
    assert!(
        untouched.with_store >= untouched.without_store,
        "the {tier} tier delivered {} capsule(s) through the store against {} without it with \
         nothing appended anywhere; where nothing has advanced the origin is strictly better and \
         the store may not cost sealed material",
        untouched.with_store,
        untouched.without_store
    );
    if untouched.source_capsules > 0 {
        assert!(
            untouched.with_store > 0,
            "the {tier} tier's sources carried {} capsule(s) and the store-backed chain delivered \
             none of them, which is the defect the store exists to fix",
            untouched.source_capsules
        );
    }
    eprintln!(
        "  tier {tier:?} second hop RAN: {} chain(s).\n    nothing appended: {} capsule(s) in the \
         sources, {} delivered with the store, {} without it.\n    work appended:    {} \
         capsule(s) in the sources, {} delivered with the store, {} without it; the appended work \
         arrived in {}/{} store-backed chains and {}/{} without one.",
        report.sessions(),
        untouched.source_capsules,
        untouched.with_store,
        untouched.without_store,
        appended.source_capsules,
        appended.with_store,
        appended.without_store,
        appended.store_kept_work,
        appended.sessions,
        appended.control_kept_work,
        appended.sessions,
    );
}

// ---------------------------------------------------------------------------
// Shared ending
// ---------------------------------------------------------------------------

/// Print every count, then fail on the objections.
///
/// In that order on purpose: the tallies are the half of the suite that finds
/// the silent defects, and a panic before them would throw away the evidence for
/// the failure it is reporting.
fn finish(tier: &str, report: &Report) {
    report.print();
    assert!(
        report.sessions() > 0,
        "the {tier} tier claimed no session at all, so nothing below was checked; \
         a battery that runs on nothing passes on nothing"
    );
    assert!(
        report.findings().is_empty(),
        "the {tier} tier found {} conformance failure(s):\n  {}",
        report.findings().len(),
        report.findings().join("\n  ")
    );
    eprintln!(
        "  tier {tier:?} RAN: {} session(s) checked, no findings.",
        report.sessions()
    );
}
