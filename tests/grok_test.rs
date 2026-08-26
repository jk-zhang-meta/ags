//! Integration tests for the Grok Build provider.
//!
//! These exercise discovery, reading, and CLI behavior against a temporary
//! `$GROK_HOME` seeded from the fixture tree. They live here rather than in
//! the in-crate `#[cfg(test)]` module because `src/lib.rs` declares
//! `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe` in edition
//! 2024 — the shared `EnvGuard`/`EnvLock` harness (see `tests/test_env.rs`)
//! serializes process-global env mutation here, in a separate crate.

mod test_env;

use std::path::{Path, PathBuf};

use ags::discovery::ProviderRegistry;
use ags::model::MessageRole;
use ags::providers::{Provider, WriteOptions, grok::Grok};

static GROK_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold the `GROK_ENV` lock for the duration, so no
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

const FIXTURE_ID: &str = "019f75d0-aaaa-7bbb-8ccc-b0a1b2c3d4e5";
const ENCODED_CWD: &str = "%2Fdata%2Fprojects%2Fdemo";
const FIXTURE_WORKSPACE: &str = "/data/projects/demo";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok")
}

fn write_fake_grok(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            "#!/bin/sh\nif [ \"$1\" = sessions ] && [ \"$2\" = delete ]; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, "exit 0").unwrap();
    }
}

/// Copy the fixture session tree into `$GROK_HOME/sessions/…`, plus the
/// non-session artifacts a real `~/.grok/sessions` tree contains
/// (`session_search.sqlite`, group-level `prompt_history.jsonl`).
fn seed_grok_home(home: &Path) {
    let dst = home.join("sessions").join(ENCODED_CWD).join(FIXTURE_ID);
    std::fs::create_dir_all(&dst).unwrap();
    let src = fixtures_dir()
        .join("sessions")
        .join(ENCODED_CWD)
        .join(FIXTURE_ID);
    for name in ["updates.jsonl", "summary.json"] {
        std::fs::copy(src.join(name), dst.join(name)).unwrap();
    }
    std::fs::write(
        home.join("sessions").join("session_search.sqlite"),
        b"SQLite format 3\x00fixture",
    )
    .unwrap();
    std::fs::write(
        home.join("sessions").join(ENCODED_CWD).join("prompt_history.jsonl"),
        format!(
            "{{\"timestamp\":\"2026-07-18T15:20:56.242466991Z\",\"session_id\":\"{FIXTURE_ID}\",\"prompt\":\"Run the shell command: echo hi .\",\"is_bash\":false}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn discovery_lists_and_owns_seeded_session() {
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", tmp.path());
    seed_grok_home(tmp.path());

    let listed = Grok.list_sessions().expect("list_sessions").sessions;
    assert_eq!(listed.len(), 1, "exactly one seeded session: {listed:?}");
    assert_eq!(listed[0].0, FIXTURE_ID);

    let owned = Grok.owns_session(FIXTURE_ID).expect("owns seeded session");
    assert!(owned.ends_with("updates.jsonl"));

    // Case-insensitive ownership lookup.
    let upper = FIXTURE_ID.to_ascii_uppercase();
    assert!(Grok.owns_session(&upper).is_some());

    // The registry resolves the `grk` alias (and slug tokens) to Grok.
    let registry = ProviderRegistry::default_registry();
    let provider = registry.find_by_alias("grk").expect("grk alias resolves");
    assert_eq!(provider.slug(), "grok");
    let provider = registry
        .find_by_alias("grok-build")
        .expect("grok-build token resolves");
    assert_eq!(provider.slug(), "grok");
}

#[test]
fn read_session_from_seeded_home_matches_fixture_expectations() {
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", tmp.path());
    seed_grok_home(tmp.path());

    let path = Grok.owns_session(FIXTURE_ID).expect("owns");
    let session = Grok.read_session(&path).expect("read");

    assert_eq!(session.session_id, FIXTURE_ID);
    assert_eq!(session.provider_slug, "grok");
    assert_eq!(session.workspace, Some(PathBuf::from(FIXTURE_WORKSPACE)));
    assert_eq!(session.model_name.as_deref(), Some("grok-build"));
    assert_eq!(session.messages.len(), 5);
    assert_eq!(session.messages[0].role, MessageRole::User);
    assert!(session.messages.iter().any(|m| !m.tool_calls.is_empty()));
    assert!(session.messages.iter().any(|m| !m.tool_results.is_empty()));
}

#[test]
fn write_session_is_refused_without_official_cli() {
    use ags::model::{CanonicalMessage, CanonicalSession};

    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", tmp.path());
    let missing_bin = tmp.path().join("missing-grok");
    let _bin = EnvGuard::set("GROK_BIN", &missing_bin);

    let session = CanonicalSession {
        session_id: "foreign".into(),
        provider_slug: "claude-code".into(),
        workspace: Some(PathBuf::from("/data/projects/foo")),
        title: Some("Hello".into()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_100_000),
        messages: vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Hi there".into(),
            timestamp: Some(1_700_000_000_000),
            author: Some("user".into()),
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }],
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        source_path: PathBuf::from("/nonexistent"),
        model_name: None,
    };

    let err = Grok
        .write_session(&session, &WriteOptions { force: true })
        .expect_err("grok must refuse writes");
    let msg = err.to_string();
    assert!(
        msg.contains("official `grok` CLI") || msg.contains("GROK_BIN"),
        "unhelpful error: {msg}"
    );
    // Nothing may have been written into GROK_HOME.
    assert!(
        !tmp.path().join("sessions").exists(),
        "refused write must not create session dirs"
    );
}

/// CLI smoke test: `casr list --provider grok` finds the seeded session.
#[test]
fn cli_list_finds_seeded_grok_session() {
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    seed_grok_home(tmp.path());

    // `casr list` defaults to scoping by the current working-directory
    // project; the fixture's workspace is a synthetic path, so pass it
    // explicitly via `--workspace` to take it out of cwd scope.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ags"))
        .args([
            "convert",
            "list",
            "--provider",
            "grok",
            "--workspace",
            FIXTURE_WORKSPACE,
            "--limit",
            "5",
        ])
        .env("GROK_HOME", tmp.path())
        .output()
        .expect("run casr list");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "casr list failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains(FIXTURE_ID) || stdout.contains("grok"),
        "expected the seeded Grok session in output:\n{stdout}"
    );
}

/// CLI smoke test: `casr info <id> --source grk` reports the session details.
#[test]
fn cli_info_reports_seeded_grok_session() {
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    seed_grok_home(tmp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ags"))
        .args(["convert", "info", FIXTURE_ID, "--source", "grk"])
        .env("GROK_HOME", tmp.path())
        .output()
        .expect("run casr info");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "casr info failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains(FIXTURE_ID),
        "expected session id in info output:\n{stdout}"
    );
}

/// CLI smoke test: converting into Grok fails when its official CLI is absent.
#[test]
fn cli_convert_into_grok_is_refused_without_official_cli() {
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    seed_grok_home(tmp.path());
    let missing_bin = tmp.path().join("missing-grok");

    // Seed a Claude Code session as the cross-provider source (same-provider
    // conversions short-circuit without touching the writer).
    let cc_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude_code/cc_simple.jsonl");
    let cc_dir = tmp.path().join("claude/projects/-data-projects-myapp");
    std::fs::create_dir_all(&cc_dir).unwrap();
    std::fs::copy(&cc_fixture, cc_dir.join("cc-simple-001.jsonl")).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ags"))
        .args([
            "convert",
            "resume",
            "grok",
            "cc-simple-001",
            "--source",
            "cc",
        ])
        .env("GROK_HOME", tmp.path())
        .env("GROK_BIN", &missing_bin)
        .env("CLAUDE_HOME", tmp.path().join("claude"))
        // `resume` consults the session store, which defaults to the real
        // `dirs::data_dir()/ags`. Without this the refusal below still happens,
        // but only after the store has filed this fixture as the origin of a new
        // conversation in the developer's own store, referencing a path inside a
        // `tempfile` directory that is about to be deleted.
        .env("AGS_STORE", tmp.path().join("ags-store"))
        .output()
        .expect("run casr resume grok");

    assert!(
        !output.status.success(),
        "converting into grok must fail without its official CLI"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("official `grok` CLI"),
        "expected the missing-CLI refusal message, got:\n{combined}"
    );
}

#[test]
fn write_session_round_trips_and_rolls_back_with_vendor_cli_stub() {
    use ags::model::{CanonicalMessage, CanonicalSession, ToolCall, ToolResult};

    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", tmp.path());
    let bin = tmp.path().join("grok");
    write_fake_grok(&bin);
    let _bin = EnvGuard::set("GROK_BIN", &bin);

    let source = CanonicalSession {
        session_id: "foreign".into(),
        provider_slug: "claude-code".into(),
        workspace: Some(tmp.path().to_path_buf()),
        title: Some("Imported Grok session".into()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        messages: vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "List files".into(),
                timestamp: Some(1_700_000_000_000),
                author: Some("user".into()),
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "I will inspect the workspace.".into(),
                timestamp: Some(1_700_000_000_500),
                author: Some("grok-build".into()),
                tool_calls: vec![ToolCall {
                    id: Some("call-1".into()),
                    name: "list_files".into(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                tool_results: vec![ToolResult {
                    call_id: Some("call-1".into()),
                    content: "a.txt".into(),
                    is_error: false,
                }],
                extra: serde_json::Value::Null,
            },
        ],
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        source_path: PathBuf::from("/nonexistent"),
        model_name: Some("foreign-model".into()),
    };

    let written = Grok
        .write_session(&source, &WriteOptions { force: false })
        .expect("Grok writer");
    assert_eq!(written.paths[0].file_name().unwrap(), "updates.jsonl");
    let read_back = Grok
        .read_session(&written.paths[0])
        .expect("Grok read-back");
    assert_eq!(read_back.messages.len(), source.messages.len());
    assert_eq!(read_back.messages[0].content, "List files");
    assert_eq!(
        read_back.messages[1].content,
        "I will inspect the workspace."
    );
    assert_eq!(read_back.messages[1].tool_calls.len(), 1);
    assert_eq!(read_back.messages[1].tool_results.len(), 1);
    assert!(Grok.owns_session(&written.session_id).is_some());

    Grok.rollback_write(&written).expect("rollback");
    assert!(Grok.owns_session(&written.session_id).is_none());
}

#[test]
fn official_grok_cli_lifecycle_when_configured() {
    let Ok(binary) = std::env::var("GROK_TEST_BIN") else {
        return;
    };
    let binary = PathBuf::from(binary);
    if !binary.is_file() {
        return;
    }

    use ags::model::{CanonicalMessage, CanonicalSession, ToolCall, ToolResult};
    let _lock = GROK_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", tmp.path());
    let _bin = EnvGuard::set("GROK_BIN", &binary);
    let workspace = tmp
        .path()
        .join("a".repeat(80))
        .join("b".repeat(80))
        .join("c".repeat(80));
    std::fs::create_dir_all(&workspace).unwrap();
    let source = CanonicalSession {
        session_id: "foreign".into(),
        provider_slug: "claude-code".into(),
        workspace: Some(workspace.clone()),
        title: Some("Official Grok lifecycle".into()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000),
        messages: vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "lifecycle probe".into(),
                timestamp: Some(1_700_000_000_000),
                author: Some("user".into()),
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::Value::Null,
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "probe response".into(),
                timestamp: Some(1_700_000_000_000),
                author: Some("grok-build".into()),
                tool_calls: vec![ToolCall {
                    id: Some("official-call".into()),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "probe.txt"}),
                }],
                tool_results: vec![ToolResult {
                    call_id: Some("official-call".into()),
                    content: "probe output".into(),
                    is_error: false,
                }],
                extra: serde_json::Value::Null,
            },
        ],
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        source_path: PathBuf::from("/nonexistent"),
        model_name: None,
    };

    let written = Grok
        .write_session(&source, &WriteOptions { force: false })
        .expect("official Grok export should accept the session");
    let list = std::process::Command::new(&binary)
        .args(["sessions", "list"])
        .current_dir(&workspace)
        .output()
        .expect("official Grok sessions list");
    assert!(
        list.status.success(),
        "official Grok list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&written.session_id),
        "official Grok list did not discover {}: {}",
        written.session_id,
        String::from_utf8_lossy(&list.stdout)
    );
    let export = std::process::Command::new(&binary)
        .args(["export", &written.session_id])
        .current_dir(&workspace)
        .output()
        .expect("official Grok export");
    assert!(export.status.success(), "official Grok export failed");
    let exported = String::from_utf8_lossy(&export.stdout);
    assert!(exported.contains("lifecycle probe"));
    assert!(exported.contains("probe response"));
    Grok.rollback_write(&written)
        .expect("official Grok rollback");
    let listed_after = Grok.list_sessions().unwrap();
    assert!(
        !listed_after
            .sessions
            .iter()
            .any(|(id, _)| id == &written.session_id)
    );
}
