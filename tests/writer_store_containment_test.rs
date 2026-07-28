//! A writer must put the session inside the store the tool reads.
//!
//! Every other writer test in this repository asks whether casr can read back
//! what casr wrote. That oracle cannot see this failure: a session written one
//! directory above the store round-trips perfectly through
//! [`Provider::read_session`] and is still invisible to the agent, because the
//! agent enumerates a directory the file is not in. The question here is not
//! "is the content right" but "is the file somewhere the tool will look", which
//! is answered against [`Provider::session_roots`] — the same enumeration
//! `casr list` walks and the one each provider's module documents against its
//! vendor's own listing rule.
//!
//! # What this pins
//!
//! With a well-formed id every writer lands inside its own declared root. An id
//! containing `../` must be encoded before it becomes a target-store component,
//! so the resulting file stays under the provider's declared root:
//!
//! | writer   | target it builds                              |
//! |----------|-----------------------------------------------|
//! | ClawdBot | `<home>/{id}.jsonl`                           |
//! | Factory  | `<home>/{id}.jsonl`                           |
//! | Vibe     | `<home>/logs/session/session_<utc>_<id8>/messages.jsonl` |
//! | Pi-Agent | `<home>/sessions/{timestamp}_{id}.jsonl`      |
//!
//! These target stores are flat at the id boundary. [`casr::pipeline::atomic_write`]
//! calls `create_dir_all` on the supplied parent, so letting an incoming id
//! supply that parent would materialise a traversal.
//!
//! # Why a hostile id may not need to be typed to reach it
//!
//! `CanonicalSession::session_id` is whatever the *source* provider's reader
//! produced, and a reader that takes it from inside the file rather than from
//! the file's name can carry a separator, because a filename cannot.
//!
//! Amp used to be exactly that: its reader preferred the thread's own `id`
//! field and fell back to the stem only when the field was absent, so any
//! `.json` in an Amp threads directory carrying `"id": "../x"` became a
//! canonical session with that id, and converting it to any of the five writers
//! above wrote outside the target's store. That route was measured end to end
//! and is now closed — Amp's reader takes the stem, for reasons that have
//! nothing to do with this test and are recorded on
//! [`an_amp_thread_can_no_longer_supply_a_traversing_id_from_its_content`].
//!
//! **No other reader has been surveyed for the same shape.** Closing the one
//! route that was measured is not evidence that none remains, and the writers
//! below are unchanged either way: a traversing id still escapes, whatever
//! supplies it. `ProviderRegistry` does not stop it — its boundary check
//! rejects only *absolute* session ids, deliberately, because a Codex session
//! id genuinely is the relative `2026/07/27/rollout-…`.
//!
//! # Read this as a characterisation, not an endorsement
//!
//! [`every_writer_contains_a_traversing_session_id`] pins the repaired
//! behaviour. A newly found escaping writer belongs in `ESCAPES` until its
//! writer is fixed; fixing it means moving that name to `CONTAINS`, never
//! weakening the containment assertion.

mod test_env;

use std::path::{Path, PathBuf};

use casr::discovery::ProviderRegistry;
use casr::model::{CanonicalMessage, CanonicalSession, MessageRole};
use casr::providers::{Provider, WriteOptions};

/// Every test here rewrites the provider home environment, which is
/// process-global and `unsafe` to touch concurrently in Rust 2024.
static ENV: test_env::EnvLock = test_env::EnvLock;

/// A well-formed id, of the shape every provider in the registry mints.
const PLAIN_ID: &str = "019c3eae-94c3-7d73-9b2a-9edb18f1563b";

/// Six levels, because the writers bury their store at different depths and one
/// `..` only escapes the shallowest of them. Pi-Agent needs more than one on its
/// own: it prefixes a timestamp, so `{ts}_..` is a literal directory name that
/// absorbs the first component.
const TRAVERSING_ID: &str = "../../../../../../escaped";

/// The writers whose target path stays inside their own declared roots when
/// handed [`TRAVERSING_ID`].
const CONTAINS: &[&str] = &[
    "claude-code",
    "codex",
    "gemini",
    "cursor",
    "cline",
    "aider",
    "amp",
    "kiro",
    "clawdbot",
    "vibe",
    "factory",
    "pi-agent",
];

/// A newly discovered escaping writer belongs here until it is fixed by moving
/// it to [`CONTAINS`].
const ESCAPES: &[&str] = &[];

/// Providers that refuse to write at all, and say so rather than emitting a
/// stub their tool would reject.
const REFUSES: &[&str] = &["antigravity", "chatgpt", "grok", "opencode", "openclaw"];

// ---------------------------------------------------------------------------
// Environment sandbox
// ---------------------------------------------------------------------------

/// casr's own "write here" overrides, one per provider. Amp's store is XDG's.
const CASR_HOMES: &[&str] = &[
    "CLAUDE_HOME",
    "CODEX_HOME",
    "GEMINI_HOME",
    "CLINE_HOME",
    "CHATGPT_HOME",
    "CLAWDBOT_HOME",
    "VIBE_HOME",
    "FACTORY_HOME",
    "OPENCLAW_HOME",
    "PI_AGENT_HOME",
    "CURSOR_HOME",
    "KIRO_HOME",
    "GROK_HOME",
    "AIDER_HOME",
    "OPENCODE_HOME",
    "XDG_DATA_HOME",
    "HOME",
];

/// The vendors' own relocation variables. Cleared, so that a value inherited
/// from the developer's shell cannot aim a writer at a real store.
const VENDOR_OVERRIDES: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "GEMINI_CLI_HOME",
    "CLINE_DATA_DIR",
    "CLINE_DIR",
    "FACTORY_HOME_OVERRIDE",
    "CLAWDBOT_STATE_DIR",
    "OPENCLAW_STATE_DIR",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "CURSOR_CONFIG_DIR",
    "CURSOR_DATA_DIR",
    "OPENCODE_DB",
    "OPENCODE_DB_PATH",
    "XDG_CONFIG_HOME",
    "AIDER_CHAT_HISTORY_FILE",
    "AGSX_STORE",
];

struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: the caller holds `ENV` for the whole lifetime of this guard.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }

    fn unset(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: as above.
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Aim every provider at a directory buried deep enough under `root` that
/// [`TRAVERSING_ID`] escapes the store without escaping the temporary
/// directory. The nesting is the safety property: the writers under test are
/// the ones that will follow `../` wherever it points.
fn sandbox(root: &Path) -> Vec<EnvGuard> {
    let deep = root.join("a/b/c/d/e/f");
    let mut guards: Vec<EnvGuard> = CASR_HOMES
        .iter()
        .map(|key| EnvGuard::set(key, &deep.join(key.to_ascii_lowercase())))
        .collect();
    guards.extend(VENDOR_OVERRIDES.iter().copied().map(EnvGuard::unset));
    guards
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn session(id: &str, workspace: &Path) -> CanonicalSession {
    let message = |idx, role, content: &str, timestamp| CanonicalMessage {
        idx,
        role,
        content: content.to_string(),
        timestamp: Some(timestamp),
        author: None,
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        extra: serde_json::Value::Null,
    };
    CanonicalSession {
        session_id: id.to_string(),
        provider_slug: "claude-code".to_string(),
        workspace: Some(workspace.to_path_buf()),
        title: Some("containment probe".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        messages: vec![
            message(0, MessageRole::User, "hello", 1_700_000_000_000),
            message(1, MessageRole::Assistant, "hi", 1_700_000_001_000),
        ],
        metadata: serde_json::Value::Null,
        source_path: PathBuf::from("/nonexistent/source.jsonl"),
        model_name: None,
    }
}

/// Whether `path` is under any of `roots`, compared on resolved paths so that a
/// `..` in the middle is followed rather than matched textually. A path that
/// does not exist is compared as written, which is the honest reading for the
/// virtual paths Aider, Cursor and OpenCode return.
fn under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    roots.iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        resolved.starts_with(root)
    })
}

/// Write `id` with every provider that writes, and hand each result to `check`.
fn for_each_writer(
    id: &str,
    mut check: impl FnMut(&dyn Provider, &casr::providers::WrittenSession),
) {
    let registry = ProviderRegistry::default_registry();
    for provider in registry.all_providers() {
        // A fresh store per provider: five of these writers resolve a traversing
        // id onto a path outside their own home, and two of them resolve it onto
        // the *same* path, so a shared root turns the second write into a
        // spurious `SessionConflict`.
        let tmp = tempfile::TempDir::new().expect("temporary store");
        let root = tmp.path().canonicalize().expect("resolve temporary store");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let _guards = sandbox(&root);

        match provider.write_session(&session(id, &workspace), &WriteOptions { force: false }) {
            Ok(written) => {
                assert!(
                    !REFUSES.contains(&provider.slug()),
                    "{} is listed as refusing to write but wrote {:?}",
                    provider.slug(),
                    written.paths
                );
                check(provider, &written);
            }
            Err(error) => assert!(
                REFUSES.contains(&provider.slug()),
                "{}: write_session failed unexpectedly: {error}",
                provider.slug()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The ordinary case: a converted session is where the tool enumerates.
///
/// `session_roots` is the provider's transcription of its vendor's own listing
/// rule, so a write that lands outside it is a session the agent will not offer
/// however well-formed its contents are.
#[test]
fn every_writer_lands_inside_a_root_it_declares() {
    let _lock = ENV.lock().unwrap();
    for_each_writer(PLAIN_ID, |provider, written| {
        let roots = provider.session_roots();
        assert!(
            !roots.is_empty(),
            "{}: wrote {:?} but declares no session root, so nothing can enumerate it",
            provider.slug(),
            written.paths
        );
        assert!(
            written
                .paths
                .iter()
                .any(|path| under_any_root(path, &roots)),
            "{}: wrote {:?}, none of which is under any declared root {:?}",
            provider.slug(),
            written.paths,
            roots
        );
    });
}

/// And casr can find again what it just wrote.
///
/// The same lookup `casr resume` performs. It is casr-against-casr and so
/// cannot establish that the *vendor* will find the file — only
/// `session_roots`, checked above against each provider's transcription of the
/// vendor's listing rule, speaks to that. What it does establish is that the id
/// reported back to the user resolves, which is the id every later command
/// takes.
#[test]
fn every_writer_reports_an_id_that_resolves_to_what_it_wrote() {
    let _lock = ENV.lock().unwrap();
    for_each_writer(PLAIN_ID, |provider, written| {
        assert!(
            !written.session_id.is_empty(),
            "{}: wrote {:?} under an empty id",
            provider.slug(),
            written.paths
        );
        let owned = provider
            .owns_session(&written.session_id)
            .unwrap_or_else(|| {
                panic!(
                    "{}: reported id {:?} but owns_session cannot resolve it; the session is \
                 written and immediately unreachable",
                    provider.slug(),
                    written.session_id
                )
            });
        assert!(
            written.paths.contains(&owned),
            "{}: id {:?} resolves to {:?}, which is not among the paths it wrote {:?}",
            provider.slug(),
            written.session_id,
            owned,
            written.paths
        );
    });
}

/// Every writer keeps a traversing source id inside its declared store.
///
/// A target provider may not treat an incoming canonical id as a path. Writers
/// that need a flat native id percent-encode it before it reaches the filename,
/// header, or returned resume command.
#[test]
fn every_writer_contains_a_traversing_session_id() {
    let _lock = ENV.lock().unwrap();
    let mut contained = Vec::new();
    let mut escaped = Vec::new();

    for_each_writer(TRAVERSING_ID, |provider, written| {
        let roots = provider.session_roots();
        if written
            .paths
            .iter()
            .any(|path| under_any_root(path, &roots))
        {
            contained.push(provider.slug().to_string());
        } else {
            escaped.push(provider.slug().to_string());
        }
    });

    contained.sort();
    escaped.sort();
    let mut expected_contained: Vec<String> = CONTAINS.iter().map(|s| s.to_string()).collect();
    let mut expected_escaped: Vec<String> = ESCAPES.iter().map(|s| s.to_string()).collect();
    expected_contained.sort();
    expected_escaped.sort();

    assert_eq!(
        escaped, expected_escaped,
        "the set of writers that follow a traversing session id out of their own store changed"
    );
    assert_eq!(
        contained, expected_contained,
        "the set of writers that keep a traversing session id inside their store changed"
    );
}

/// A hostile id cannot turn a refused OpenCode write into a filesystem effect.
#[test]
fn opencode_refuses_a_traversing_id_without_creating_a_database() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    let opencode = registry
        .find_by_slug("opencode")
        .expect("opencode registered");

    let tmp = tempfile::TempDir::new().expect("temporary store");
    let root = tmp.path().canonicalize().expect("resolve temporary store");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let _guards = sandbox(&root);

    let error = opencode
        .write_session(
            &session(TRAVERSING_ID, &workspace),
            &WriteOptions { force: false },
        )
        .expect_err("OpenCode target must refuse");
    assert!(
        error.to_string().contains("OpenCode is read/resume-only"),
        "unexpected refusal: {error:#}"
    );
    assert!(
        !root.join("opencode.db").exists(),
        "refusal must not create opencode.db"
    );
}

/// A hostile id cannot turn a refused OpenClaw import into a filesystem effect.
#[test]
fn openclaw_refuses_a_traversing_id_without_creating_state() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    let openclaw = registry
        .find_by_slug("openclaw")
        .expect("openclaw registered");

    let tmp = tempfile::TempDir::new().expect("temporary store");
    let root = tmp.path().canonicalize().expect("resolve temporary store");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let _guards = sandbox(&root);

    let error = openclaw
        .write_session(
            &session(TRAVERSING_ID, &workspace),
            &WriteOptions { force: true },
        )
        .expect_err("OpenClaw target must refuse even with --force");
    assert!(
        error.to_string().contains("gateway"),
        "unexpected refusal: {error:#}"
    );
    assert!(
        !root.join("a/b/c/d/e/f/openclaw_home/.openclaw").exists(),
        "refusal must not create OpenClaw state"
    );
}

/// The Amp route by which a traversing id reached a `CanonicalSession` without
/// anyone typing it — closed, and pinned closed.
///
/// This test was written against a tree in which Amp's reader took the thread
/// id from the thread's own `id` field, falling back to the filename stem only
/// when that field was absent. The id was therefore file *content*, and content
/// can carry a separator where a filename cannot. That was the reachability
/// half of the defect above: a source file supplied the value that walked five
/// writers out of their store.
///
/// It was closed in the same round by an unrelated change. Amp's
/// `read_session` was flipped to prefer the filename stem because Amp's own
/// *storage* layer keys on the filename (`get(key)` is
/// `joinPath(root, key + ".json")`) while only its product layer keys on the
/// inner `.id`, and on a file where the two disagree Amp itself silently
/// creates a new empty thread and orphans the old one. The inner id is still
/// published, as `metadata.id`. A stem cannot contain a separator, so the
/// vector is gone rather than narrowed.
///
/// Two things this does NOT establish, both deliberately left open:
///
/// * Writer-side containment is independently pinned by
///   [`every_writer_contains_a_traversing_session_id`]. Closing one delivery
///   route was not the writer fix.
/// * Whether another reader can still supply a traversing id from content was
///   not surveyed. Amp was the one measured, not the only one possible.
#[test]
fn an_amp_thread_can_no_longer_supply_a_traversing_id_from_its_content() {
    let _lock = ENV.lock().unwrap();
    let registry = ProviderRegistry::default_registry();
    let amp = registry.find_by_slug("amp").expect("amp registered");

    let tmp = tempfile::TempDir::new().expect("temporary thread dir");
    let path = tmp.path().join("T-plausible-looking-name.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "v": 0,
            "id": TRAVERSING_ID,
            "created": 1_700_000_000_000i64,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        }))
        .expect("serialize thread"),
    )
    .expect("write thread");

    let canonical = amp.read_session(&path).expect("read amp thread");
    assert_eq!(
        canonical.session_id, "T-plausible-looking-name",
        "the session id must come from the filename stem, which cannot carry a separator; \
         taking it from the file's own field is what let a source file walk five writers \
         out of their store"
    );
    assert_eq!(
        std::path::Path::new(&canonical.session_id)
            .components()
            .count(),
        1,
        "a session id that is not exactly one path component is the whole defect"
    );
    assert_ne!(
        canonical.session_id, TRAVERSING_ID,
        "the traversing value from the file's `id` field must not become the session id"
    );
}
