//! Integration tests for the Kiro CLI provider.
//!
//! These exercise the full filesystem round-trip (read → write → re-read) and
//! the `ags` CLI against a temporary `$KIRO_HOME`. They live here rather than
//! in the in-crate `#[cfg(test)]` module because `src/lib.rs` declares
//! `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe` in edition
//! 2024 — the shared `EnvGuard`/`EnvLock` harness (see `tests/test_env.rs`)
//! serializes process-global env mutation here, in a separate crate.

mod test_env;

use std::path::{Path, PathBuf};

use ags::discovery::ProviderRegistry;
use ags::model::MessageRole;
use ags::providers::{Provider, WriteOptions, kiro::Kiro};

static KIRO_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold the `KIRO_ENV` lock for the duration, so no
        // other thread reads or mutates the environment concurrently.
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

const FIXTURE_ID: &str = "0a5376f2-7e2f-4981-bcbc-67195586604a";
const PLANTED_WRITE_SECRET: &str = "kiro-write-residual-c0ffee11deadbeefc0ffee11deadbeef";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kiro")
}

fn fixture_json() -> PathBuf {
    fixtures_dir().join(format!("{FIXTURE_ID}.json"))
}

/// Copy the captured fixture triplet into `$KIRO_HOME/sessions/cli/`.
fn seed_kiro_home(home: &Path) {
    let dst = home.join("sessions").join("cli");
    std::fs::create_dir_all(&dst).unwrap();
    for ext in ["json", "jsonl", "history"] {
        let name = format!("{FIXTURE_ID}.{ext}");
        std::fs::copy(fixtures_dir().join(&name), dst.join(&name)).unwrap();
    }
}

#[test]
fn full_filesystem_round_trip_preserves_messages_and_drops_history() {
    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIRO_HOME", tmp.path());

    let original = Kiro.read_session(&fixture_json()).expect("read original");

    let written = Kiro
        .write_session(&original, &WriteOptions { force: true })
        .expect("write session");

    // .json + .jsonl. No `.history`: the reader no longer carries the fixture's
    // `.history` sidecar, so the writer has nothing to re-emit. That is the
    // accepted cost of not publishing raw prompt input — see the read site.
    assert_eq!(written.paths.len(), 2, "json + jsonl");
    assert!(written.resume_command.starts_with("kiro-cli --resume-id "));
    for p in &written.paths {
        assert!(p.exists(), "written file missing: {}", p.display());
    }

    let new_json = Kiro
        .owns_session(&written.session_id)
        .expect("owns written session");
    let reread = Kiro.read_session(&new_json).expect("re-read written");

    assert_eq!(reread.messages.len(), original.messages.len());
    for (a, b) in original.messages.iter().zip(reread.messages.iter()) {
        assert_eq!(a.role, b.role, "role mismatch at idx {}", a.idx);
        assert_eq!(a.content, b.content, "content mismatch at idx {}", a.idx);
        assert_eq!(
            a.tool_calls.len(),
            b.tool_calls.len(),
            "tool_calls at {}",
            a.idx
        );
        assert_eq!(
            a.tool_results.len(),
            b.tool_results.len(),
            "tool_results at {}",
            a.idx
        );
    }

    // Only the audited state format survives; vendor-owned nested state does
    // not belong in canonical metadata.
    assert_eq!(
        original.metadata.get("session_state"),
        reread.metadata.get("session_state"),
        "the minimal session_state must remain stable"
    );
    assert_eq!(
        reread.metadata["session_state"],
        serde_json::json!({"version": "v1"})
    );
    // `.history` does not round-trip, on purpose. kiro-cli's `addToHistory`
    // records every submitted line, not just slash commands, so the file is
    // raw user input — and the metadata bag that would carry it is what
    // `ags convert info --json` prints. Neither side may hold it.
    assert!(
        original.metadata.get("history").is_none() && reread.metadata.get("history").is_none(),
        "`.history` must not be carried in either direction"
    );
    assert_eq!(original.workspace, reread.workspace);
    assert_eq!(
        original.metadata.get("parent_session_id"),
        reread.metadata.get("parent_session_id"),
    );
}

#[test]
fn writer_drops_vendor_state_and_envelope_residuals() {
    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIRO_HOME", tmp.path());

    let mut session = Kiro.read_session(&fixture_json()).expect("read original");
    session.metadata["session_state"] = serde_json::json!({
        "version": "v1",
        "conversation_metadata": {
            "last_request": { "system_prompt": PLANTED_WRITE_SECRET }
        },
        "rts_model_state": {
            "additional_fields": { "blob": PLANTED_WRITE_SECRET }
        },
        "permissions": {
            "filesystem": { "future_secret": PLANTED_WRITE_SECRET }
        }
    });

    for message in &mut session.messages {
        let kind = match &message.role {
            MessageRole::User => "Prompt",
            MessageRole::Assistant => "AssistantMessage",
            MessageRole::Tool => "ToolResults",
            other => panic!("unexpected fixture role: {other:?}"),
        };
        message.extra = serde_json::json!({
            "version": "v1",
            "kind": kind,
            "future_root": format!("{PLANTED_WRITE_SECRET}:{kind}:root"),
            "data": {
                "future_nested": format!("{PLANTED_WRITE_SECRET}:{kind}:data")
            }
        });
    }

    let written = Kiro
        .write_session(&session, &WriteOptions { force: true })
        .expect("write sanitized session");
    let metadata_path = written
        .paths
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .expect("metadata path");
    let journal_path = written
        .paths
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("journal path");

    let metadata_text = std::fs::read_to_string(metadata_path).unwrap();
    let journal_text = std::fs::read_to_string(journal_path).unwrap();
    assert!(
        !metadata_text.contains(PLANTED_WRITE_SECRET),
        "writer republished canonical session_state residuals:\n{metadata_text}"
    );
    assert!(
        !journal_text.contains(PLANTED_WRITE_SECRET),
        "writer republished a root or nested envelope residual:\n{journal_text}"
    );

    let native_metadata: serde_json::Value = serde_json::from_str(&metadata_text).unwrap();
    assert_eq!(
        native_metadata["session_state"],
        serde_json::json!({"version": "v1"})
    );

    let reread = Kiro
        .read_session(metadata_path)
        .expect("re-read sanitized session");
    assert_eq!(reread.messages.len(), session.messages.len());
    for (expected, actual) in session.messages.iter().zip(&reread.messages) {
        assert_eq!(actual.role, expected.role);
        assert_eq!(actual.content, expected.content);
        assert_eq!(actual.timestamp, expected.timestamp);
        assert_eq!(actual.tool_calls, expected.tool_calls);
        assert_eq!(actual.tool_results, expected.tool_results);
    }
}

#[test]
fn discovery_lists_and_owns_seeded_session() {
    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIRO_HOME", tmp.path());
    seed_kiro_home(tmp.path());

    let listed = Kiro.list_sessions().expect("list_sessions").sessions;
    assert_eq!(listed.len(), 1, "exactly one seeded session");
    assert_eq!(listed[0].0, FIXTURE_ID);

    let owned = Kiro.owns_session(FIXTURE_ID).expect("owns seeded session");
    assert!(owned.ends_with(format!("{FIXTURE_ID}.json")));

    // The registry resolves the `kr` alias to the Kiro provider.
    let registry = ProviderRegistry::default_registry();
    let provider = registry.find_by_alias("kr").expect("kr alias resolves");
    assert_eq!(provider.slug(), "kiro");
}

/// CLI smoke test: `ags convert list --provider kiro` finds the seeded session.
#[test]
fn cli_list_finds_seeded_kiro_session() {
    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    seed_kiro_home(tmp.path());

    // `ags convert list` defaults to scoping by the current working-directory
    // project; the captured fixture's workspace is a macOS path, so we pass
    // it explicitly via `--workspace` to take it out of cwd scope.
    let workspace =
        "/Users/tranquangdang21/Projects/jcode/.worktrees/feat-380-compaction-resistant-notepad";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ags"))
        .args([
            "convert",
            "list",
            "--provider",
            "kiro",
            "--workspace",
            workspace,
            "--limit",
            "5",
        ])
        .env("KIRO_HOME", tmp.path())
        .output()
        .expect("run ags convert list");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ags convert list failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains(FIXTURE_ID) || stdout.contains("kiro"),
        "expected the seeded Kiro session in output:\n{stdout}"
    );
}

/// CLI smoke test: `ags convert info <id> --source kr` reports the session details.
#[test]
fn cli_info_reports_seeded_kiro_session() {
    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    seed_kiro_home(tmp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ags"))
        .args(["convert", "info", FIXTURE_ID, "--source", "kr"])
        .env("KIRO_HOME", tmp.path())
        .output()
        .expect("run ags convert info");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ags convert info failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains(FIXTURE_ID),
        "expected session id in info output:\n{stdout}"
    );
}

/// Cross-provider import: a synthetic non-Kiro session writes a valid triplet
/// (sans `.history`) and re-reads cleanly.
#[test]
fn foreign_session_writes_and_rereads() {
    use ags::model::{CanonicalMessage, CanonicalSession, ToolCall, ToolResult};

    let _lock = KIRO_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("KIRO_HOME", tmp.path());

    let session = CanonicalSession {
        session_id: "foreign".into(),
        provider_slug: "claude-code".into(),
        workspace: Some(PathBuf::from("/data/projects/foo")),
        title: Some("Hello".into()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_100_000),
        messages: vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Hi there".into(),
                timestamp: Some(1_700_000_000_000),
                author: Some("user".into()),
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "Hello back".into(),
                timestamp: None,
                author: None,
                tool_calls: vec![ToolCall {
                    id: Some("t1".into()),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
                tool_results: vec![ToolResult {
                    call_id: Some("t1".into()),
                    content: "file.txt".into(),
                    is_error: false,
                }],
                extra: serde_json::Value::Null,
            },
        ],
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        source_path: PathBuf::from("/nonexistent"),
        model_name: None,
    };

    let written = Kiro
        .write_session(&session, &WriteOptions { force: true })
        .expect("write foreign session");
    // No history present → only .json + .jsonl.
    assert_eq!(written.paths.len(), 2);
    let metadata_path = written
        .paths
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .expect("metadata path");
    let native_metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(metadata_path).unwrap()).unwrap();
    assert!(
        native_metadata.get("session_created_reason").is_none(),
        "an absent optional enum must be omitted, not written as null"
    );
    let journal_path = written
        .paths
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("journal path");
    let journal = std::fs::read_to_string(journal_path).unwrap();
    let envelopes: Vec<serde_json::Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result_envelope = envelopes
        .iter()
        .find(|envelope| envelope["kind"] == "ToolResults")
        .expect("Kiro needs a ToolResults envelope to replay native results");
    assert_eq!(
        result_envelope["data"]["results"]["t1"]["result"]["Success"]["items"][0]["Text"],
        "file.txt",
        "Kiro's V2 loader rebuilds tool results from data.results, not data.content"
    );

    let reread = Kiro
        .read_session(&Kiro.owns_session(&written.session_id).unwrap())
        .expect("re-read");
    assert_eq!(reread.messages.len(), 2);
    assert_eq!(reread.messages[0].content, "Hi there");
    assert_eq!(reread.messages[1].content, "Hello back");
    assert_eq!(reread.messages[1].tool_calls.len(), 1);
    assert_eq!(reread.messages[1].tool_calls[0].name, "shell");
    assert_eq!(reread.messages[1].tool_results.len(), 1);
    assert_eq!(reread.messages[1].tool_results[0].content, "file.txt");
}
