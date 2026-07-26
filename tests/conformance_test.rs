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

use casr::conformance::{self, Report};

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
