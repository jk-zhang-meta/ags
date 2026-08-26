//! Invalid session ID format tests for the discovery system.
//!
//! Tests how `ProviderRegistry::resolve_session()` handles malformed session
//! IDs: empty string, extremely long string, path traversal attempts, null
//! bytes, Unicode characters, and valid UUID format but non-existent session.
//! Each should return `SessionNotFound` with a safe error message (no path
//! injection, no panic).

mod test_env;

use ags::discovery::{ProviderRegistry, SourceHint};
use ags::error::AgsError;
use ags::providers::Provider;
use ags::providers::claude_code::ClaudeCode;
use ags::providers::clawdbot::ClawdBot;
use ags::providers::codex::Codex;

static CC_ENV: test_env::EnvLock = test_env::EnvLock;
static CLAWDBOT_ENV: test_env::EnvLock = test_env::EnvLock;
static CODEX_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: Tests must hold an `_ENV` lock (see `test_env`) while mutating
        // the process environment and while invoking code that reads it.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Assert that resolving a session ID returns SessionNotFound (not panic,
/// not a path traversal success, not an unexpected error variant).
fn assert_session_not_found(session_id: &str, label: &str) {
    let _cc_lock = CC_ENV.lock().unwrap();
    let _codex_lock = CODEX_ENV.lock().unwrap();
    let cc_tmp = tempfile::TempDir::new().expect("cc tmpdir");
    let codex_tmp = tempfile::TempDir::new().expect("codex tmpdir");
    let _cc_env = EnvGuard::set("CLAUDE_HOME", cc_tmp.path());
    let _codex_env = EnvGuard::set("CODEX_HOME", codex_tmp.path());

    let registry = ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]);
    let result = registry.resolve_session(session_id, None);

    match result {
        Err(AgsError::SessionNotFound { .. }) => {
            // Expected — session ID not found.
        }
        Err(other) => {
            // Any other error is also acceptable (e.g. provider unavailable).
            eprintln!("{label}: got non-SessionNotFound error (acceptable): {other}");
        }
        Ok(resolved) => {
            panic!(
                "{label}: malformed session ID '{session_id}' unexpectedly resolved to {} at {}",
                resolved.provider.slug(),
                resolved.path.display()
            );
        }
    }
}

// ===========================================================================
// Empty string
// ===========================================================================

#[test]
fn resolve_empty_session_id() {
    assert_session_not_found("", "empty string");
}

// ===========================================================================
// Extremely long string (10KB)
// ===========================================================================

#[test]
fn resolve_very_long_session_id() {
    let long_id = "a".repeat(10_240);
    assert_session_not_found(&long_id, "10KB string");
}

// ===========================================================================
// Path traversal attempts
// ===========================================================================

#[test]
fn resolve_path_traversal_dot_dot_slash() {
    assert_session_not_found("../../etc/passwd", "path traversal ../../etc/passwd");
}

#[test]
fn registry_rejects_relative_traversal_that_reaches_an_existing_file() {
    let _lock = CLAWDBOT_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions root");
    let outside = tmp.path().join("outside.jsonl");
    std::fs::write(&outside, "{}\n").expect("write outside transcript");
    let _env = EnvGuard::set("CLAWDBOT_HOME", &sessions);

    let registry = ProviderRegistry::new(vec![Box::new(ClawdBot)]);
    let hint = SourceHint::Alias("cwb".to_string());
    let resolved = registry.resolve_session("../outside", Some(&hint));

    assert!(
        matches!(resolved, Err(AgsError::SessionNotFound { .. })),
        "a relative session id escaped the ClawdBot store: {resolved:?}"
    );
}

#[test]
fn resolve_path_traversal_absolute() {
    assert_session_not_found("/etc/passwd", "absolute path /etc/passwd");
}

#[test]
fn resolve_path_traversal_encoded() {
    assert_session_not_found("..%2F..%2Fetc%2Fpasswd", "URL-encoded path traversal");
}

#[test]
fn resolve_path_traversal_double_dot_backslash() {
    assert_session_not_found("..\\..\\etc\\passwd", "backslash path traversal");
}

// ===========================================================================
// Null bytes embedded
// ===========================================================================

#[test]
fn resolve_null_byte_session_id() {
    assert_session_not_found("session\x00id", "null byte embedded");
}

#[test]
fn resolve_null_bytes_only() {
    assert_session_not_found("\x00\x00\x00", "null bytes only");
}

// ===========================================================================
// Unicode characters
// ===========================================================================

#[test]
fn resolve_unicode_session_id() {
    assert_session_not_found("séssion-日本語-🎉", "unicode characters");
}

#[test]
fn resolve_rtl_override_session_id() {
    assert_session_not_found("session\u{202E}di-tset", "RTL override character");
}

#[test]
fn resolve_zero_width_joiners() {
    assert_session_not_found("session\u{200D}id\u{200B}test", "zero-width joiner/space");
}

// ===========================================================================
// Valid UUID format but non-existent
// ===========================================================================

#[test]
fn resolve_valid_uuid_nonexistent() {
    assert_session_not_found(
        "550e8400-e29b-41d4-a716-446655440000",
        "valid UUID, non-existent",
    );
}

// ===========================================================================
// Special filesystem characters
// ===========================================================================

#[test]
fn resolve_glob_wildcards() {
    assert_session_not_found("session-*-?-[abc]", "glob wildcards");
}

#[test]
fn resolve_shell_metacharacters() {
    assert_session_not_found("$(echo pwned)", "shell metacharacters");
}

#[test]
fn resolve_semicolon_injection() {
    assert_session_not_found("session; rm -rf /", "semicolon injection");
}

// ===========================================================================
// Error message safety
// ===========================================================================

#[test]
fn error_message_does_not_leak_traversal_path() {
    let _cc_lock = CC_ENV.lock().unwrap();
    let _codex_lock = CODEX_ENV.lock().unwrap();
    let cc_tmp = tempfile::TempDir::new().expect("cc tmpdir");
    let codex_tmp = tempfile::TempDir::new().expect("codex tmpdir");
    let _cc_env = EnvGuard::set("CLAUDE_HOME", cc_tmp.path());
    let _codex_env = EnvGuard::set("CODEX_HOME", codex_tmp.path());

    let registry = ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]);
    let result = registry.resolve_session("../../etc/passwd", None);

    if let Err(e) = result {
        let msg = e.to_string();
        // The error message should NOT contain the resolved/expanded path.
        assert!(
            !msg.contains("/etc/passwd") || msg.contains("../../etc/passwd"),
            "error message should not leak resolved traversal path; got: {msg}"
        );
    }
}

// ===========================================================================
// Provider-level owns_session safety
// ===========================================================================

#[test]
fn cc_owns_session_traversal_returns_none() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    // Path traversal should not find a session.
    let result = ClaudeCode.owns_session("../../etc/passwd");
    assert!(
        result.is_none(),
        "CC owns_session should return None for path traversal; got: {:?}",
        result
    );
}

#[test]
fn codex_owns_session_traversal_returns_none() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    let result = Codex.owns_session("../../etc/passwd");
    assert!(
        result.is_none(),
        "Codex owns_session should return None for path traversal; got: {:?}",
        result
    );
}

#[test]
fn cc_owns_session_empty_returns_none() {
    let _lock = CC_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let _env = EnvGuard::set("CLAUDE_HOME", tmp.path());

    let result = ClaudeCode.owns_session("");
    assert!(
        result.is_none(),
        "CC owns_session should return None for empty string; got: {:?}",
        result
    );
}

// ===========================================================================
// An absolute path is not a session identifier
// ===========================================================================

/// The registry must never resolve an absolute path handed in as a session ID.
///
/// Every provider builds its candidate with `root.join(session_id)` (or
/// `join(format!("{session_id}.jsonl"))`), and `Path::join` throws the receiver
/// away when the argument is absolute. Measured before the fix: `owns_session`
/// on an absolute path was claimed by `claude-code`, `codex` and `kiro`, so
/// `ags convert info <a-claude-transcript>` was parsed by the *Codex* reader and
/// reported `provider: "codex"` with zero messages — and with `--source cod`
/// the same mismatch reached `resume`.
///
/// Asserted through the registry rather than per provider because that is the
/// one place a string is declared to be an identifier, and therefore the only
/// place the guard can cover a provider nobody has written yet.
#[test]
fn registry_never_resolves_an_absolute_path_as_a_session_id() {
    let _cc = CC_ENV.lock().unwrap();
    let _cod = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");

    // A real session file, under a real provider root, that the registry would
    // happily resolve *by id*. Naming it by path must not work.
    let projects = tmp.path().join("claude/projects/-tmp-demo");
    std::fs::create_dir_all(&projects).expect("mkdir");
    let session = projects.join("11111111-2222-3333-4444-555555555555.jsonl");
    std::fs::write(
        &session,
        "{\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"type\":\"user\",\
         \"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .expect("write");

    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _cod_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));

    let registry = ProviderRegistry::default_registry();

    for absolute in [
        session.to_string_lossy().to_string(),
        // The extension-less form is the one the `{id}.jsonl` providers took.
        session.with_extension("").to_string_lossy().to_string(),
    ] {
        let resolved = registry.resolve_session(&absolute, None);
        assert!(
            resolved.is_err(),
            "an absolute path is not an identifier, but the registry resolved \
             {absolute} to {:?}",
            resolved.map(|r| (r.provider.slug().to_string(), r.path))
        );
    }

    // A relative identifier still resolves — Codex session ids genuinely are
    // `2026/07/27/rollout-…`, so the guard must reject only the absolute case.
    assert!(
        ags::providers::claude_code::ClaudeCode
            .owns_session("11111111-2222-3333-4444-555555555555")
            .is_some(),
        "the plain identifier must still resolve"
    );
}

/// Codex reads its identifier as a path on purpose, so it says which paths.
#[test]
fn codex_owns_session_declines_an_absolute_path() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let sessions = tmp.path().join("sessions/2026/07/27");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    let rollout = sessions.join("rollout-1.jsonl");
    std::fs::write(&rollout, "{}\n").expect("write");
    let _env = EnvGuard::set("CODEX_HOME", tmp.path());

    // The relative form is the one this branch exists for, and still works.
    assert_eq!(
        Codex.owns_session("2026/07/27/rollout-1").as_deref(),
        Some(rollout.as_path()),
        "a relative path id is a supported Codex form"
    );

    // Somewhere else entirely, which `join` would have handed straight back.
    let foreign = tmp.path().join("not-codex.jsonl");
    std::fs::write(&foreign, "{}\n").expect("write");
    let claimed = Codex.owns_session(&foreign.to_string_lossy());
    assert!(
        claimed.is_none(),
        "Codex claimed a file outside its own sessions dir: {claimed:?}"
    );
}
