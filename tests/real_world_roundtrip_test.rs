//! Real-session round-trip matrix tests (sanitized fixtures).
//!
//! These tests use provider-native session artifacts derived from real sessions
//! and heavily redacted to remove sensitive content while preserving structure.

mod test_env;

use std::path::{Path, PathBuf};

use casr::model::CanonicalSession;
use casr::providers::claude_code::ClaudeCode;
use casr::providers::codex::Codex;
use casr::providers::gemini::Gemini;
use casr::providers::{Provider, WriteOptions};

static ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: guarded by test_env::EnvLock for the lifetime of the test.
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

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_world")
}

fn read_cc_real_fixture() -> CanonicalSession {
    ClaudeCode
        .read_session(&fixtures_dir().join("cc_real_world_sanitized.jsonl"))
        .expect("cc real fixture should parse")
}

fn read_codex_real_fixture() -> CanonicalSession {
    Codex
        .read_session(&fixtures_dir().join("codex_real_world_sanitized.jsonl"))
        .expect("codex real fixture should parse")
}

fn read_gemini_real_fixture() -> CanonicalSession {
    Gemini
        .read_session(&fixtures_dir().join("gemini_real_world_sanitized.json"))
        .expect("gemini real fixture should parse")
}

fn write_then_read(provider: &dyn Provider, session: &CanonicalSession) -> CanonicalSession {
    let written = provider
        .write_session(session, &WriteOptions { force: false })
        .unwrap_or_else(|e| panic!("{} write failed: {e}", provider.slug()));
    provider
        .read_session(&written.paths[0])
        .unwrap_or_else(|e| panic!("{} read-back failed: {e}", provider.slug()))
}

fn assert_fixture_is_non_trivial(session: &CanonicalSession, label: &str) {
    assert!(
        session.messages.len() >= 8,
        "[{label}] expected non-trivial fixture (>=8 messages), got {}",
        session.messages.len()
    );
    let tool_calls = session
        .messages
        .iter()
        .map(|m| m.tool_calls.len())
        .sum::<usize>();
    assert!(
        tool_calls > 0,
        "[{label}] expected at least one tool call in fixture"
    );
}

fn collect_lossiness(original: &CanonicalSession, roundtrip: &CanonicalSession) -> Vec<String> {
    let mut losses = Vec::new();

    if original.messages.len() != roundtrip.messages.len() {
        losses.push(format!(
            "message_count: {} -> {}",
            original.messages.len(),
            roundtrip.messages.len()
        ));
    }

    for (idx, (a, b)) in original
        .messages
        .iter()
        .zip(roundtrip.messages.iter())
        .enumerate()
    {
        if a.role != b.role {
            losses.push(format!("msg[{idx}] role: {:?} -> {:?}", a.role, b.role));
        }
        if a.content != b.content {
            losses.push(format!(
                "msg[{idx}] content: '{}' -> '{}'",
                truncate_for_diff(&a.content),
                truncate_for_diff(&b.content)
            ));
        }
        if a.tool_calls.len() != b.tool_calls.len() {
            losses.push(format!(
                "msg[{idx}] tool_calls.len: {} -> {}",
                a.tool_calls.len(),
                b.tool_calls.len()
            ));
        } else {
            for (call_idx, (ca, cb)) in a.tool_calls.iter().zip(b.tool_calls.iter()).enumerate() {
                if ca.name != cb.name {
                    losses.push(format!(
                        "msg[{idx}] tool_call[{call_idx}] name: '{}' -> '{}'",
                        ca.name, cb.name
                    ));
                }
                if ca.arguments != cb.arguments {
                    losses.push(format!("msg[{idx}] tool_call[{call_idx}] args changed"));
                }
            }
        }

        if a.tool_results.len() != b.tool_results.len() {
            losses.push(format!(
                "msg[{idx}] tool_results.len: {} -> {}",
                a.tool_results.len(),
                b.tool_results.len()
            ));
        } else {
            for (res_idx, (ra, rb)) in a.tool_results.iter().zip(b.tool_results.iter()).enumerate()
            {
                if ra.content != rb.content {
                    losses.push(format!(
                        "msg[{idx}] tool_result[{res_idx}] content: '{}' -> '{}'",
                        truncate_for_diff(&ra.content),
                        truncate_for_diff(&rb.content)
                    ));
                }
                if ra.is_error != rb.is_error {
                    losses.push(format!(
                        "msg[{idx}] tool_result[{res_idx}] is_error: {} -> {}",
                        ra.is_error, rb.is_error
                    ));
                }
            }
        }
    }

    losses
}

fn truncate_for_diff(text: &str) -> String {
    const MAX_CHARS: usize = 120;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    compact.chars().take(MAX_CHARS).collect::<String>() + "..."
}

fn assert_roundtrip_lossless(
    original: &CanonicalSession,
    roundtrip: &CanonicalSession,
    label: &str,
) {
    assert_roundtrip_lossless_except(original, roundtrip, label, &[]);
}

/// The round trip lost nothing beyond `expected`, and lost every one of them.
///
/// Two-sided on purpose. A flat round trip through a provider with no tool role
/// genuinely cannot keep `MessageRole::Tool`, and pretending otherwise would
/// mean either a failing test or a weakened one. But an allowance that stops
/// being exercised is worse than no allowance: it silently widens to cover a
/// real regression. So an expected loss that does not occur fails too.
fn assert_roundtrip_lossless_except(
    original: &CanonicalSession,
    roundtrip: &CanonicalSession,
    label: &str,
    expected: &[&str],
) {
    let losses = collect_lossiness(original, roundtrip);
    let (allowed, unexpected): (Vec<&String>, Vec<&String>) = losses
        .iter()
        .partition(|loss| expected.iter().any(|pat| loss.contains(pat)));
    assert!(
        unexpected.is_empty(),
        "[{label}] lossy round-trip detected:\n{}",
        unexpected
            .iter()
            .map(|loss| loss.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    for pat in expected {
        assert!(
            allowed.iter().any(|loss| loss.contains(pat)),
            "[{label}] expected loss {pat:?} did not occur — the allowance is now \
             covering nothing and must be removed"
        );
    }
}

#[test]
fn real_world_fixture_files_are_redacted() {
    let redaction_patterns = [
        "AGENTS.md",
        "/home/ubuntu",
        ".ssh/",
        "contabo",
        "ovh",
        "BEGIN RSA PRIVATE KEY",
        "OPENAI_API_KEY",
    ];

    for name in [
        "cc_real_world_sanitized.jsonl",
        "codex_real_world_sanitized.jsonl",
        "gemini_real_world_sanitized.json",
    ] {
        let path = fixtures_dir().join(name);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
        for pat in redaction_patterns {
            assert!(
                !content.contains(pat),
                "fixture {} still contains sensitive marker '{}'",
                name,
                pat
            );
        }
    }
}

#[test]
fn roundtrip_cc_to_codex_to_gemini_to_cc_is_lossless() {
    let _env_lock = ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));
    let _gemini = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let original = read_cc_real_fixture();
    assert_fixture_is_non_trivial(&original, "cc fixture");

    let cod = write_then_read(&Codex, &original);
    let gmi = write_then_read(&Gemini, &cod);
    let back_to_cc = write_then_read(&ClaudeCode, &gmi);

    assert_roundtrip_lossless(&original, &back_to_cc, "cc->cod->gmi->cc");
}

#[test]
fn roundtrip_codex_to_cc_to_gemini_to_codex_is_lossless() {
    let _env_lock = ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));
    let _gemini = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let original = read_codex_real_fixture();
    assert_fixture_is_non_trivial(&original, "codex fixture");

    let cc = write_then_read(&ClaudeCode, &original);
    let gmi = write_then_read(&Gemini, &cc);
    let back_to_codex = write_then_read(&Codex, &gmi);

    // `Tool -> User` is the one real loss on this path and it is structural,
    // not a bug: Claude Code represents a tool result as a user record holding
    // a `tool_result` block, and Gemini has no tool role at all, so by the time
    // the session returns to Codex the role is gone. That is exactly what
    // `Fidelity::ConversationOnly` describes, and it is why the structured
    // track exists.
    //
    // The old fixture never surfaced this: all 33 of its records were
    // `role: assistant` carrying Claude-shaped `tool_use` blocks inline, a
    // shape Codex has never written, and it contained no tool results at all.
    // The round trip was lossless because the input was degenerate.
    assert_roundtrip_lossless_except(
        &original,
        &back_to_codex,
        "cod->cc->gmi->cod",
        &["role: Tool -> User"],
    );
}

#[test]
fn roundtrip_gemini_to_cc_to_codex_to_gemini_is_lossless() {
    let _env_lock = ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));
    let _gemini = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let original = read_gemini_real_fixture();
    assert_fixture_is_non_trivial(&original, "gemini fixture");

    let cc = write_then_read(&ClaudeCode, &original);
    let cod = write_then_read(&Codex, &cc);
    let back_to_gemini = write_then_read(&Gemini, &cod);

    assert_roundtrip_lossless(&original, &back_to_gemini, "gmi->cc->cod->gmi");
}
