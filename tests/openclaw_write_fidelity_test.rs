//! OpenClaw's conditional Gateway target and native-reader fidelity.
//!
//! Target writes require the official CLI and authenticated Gateway lifecycle;
//! direct transcript/index mutation remains forbidden. Native transcript
//! fixtures still pin tree-walk behavior in the reader.

mod test_env;

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole};
use casr::providers::openclaw::OpenClaw;
use casr::providers::{Provider, WriteOptions};
use serde_json::json;

static OPENCLAW_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: every caller holds `OPENCLAW_ENV` for the whole lifetime of
        // the guard, so no other test reads or mutates the environment
        // concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn message(idx: usize, role: MessageRole, content: &str) -> CanonicalMessage {
    CanonicalMessage {
        idx,
        role,
        content: content.to_string(),
        timestamp: Some(1_700_000_000_000 + idx as i64 * 1000),
        author: None,
        tool_calls: vec![],
        tool_results: vec![],
        extra: json!({}),
    }
}

fn session_with(messages: Vec<CanonicalMessage>, model_name: Option<&str>) -> CanonicalSession {
    CanonicalSession {
        session_id: "write-fidelity".to_string(),
        provider_slug: "kiro".to_string(),
        workspace: Some(PathBuf::from("/home/user/project")),
        title: None,
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages,
        metadata: json!({}),
        source_path: PathBuf::from("/tmp/source.json"),
        model_name: model_name.map(String::from),
    }
}

#[test]
fn provider_refuses_without_official_cli_or_creating_state() {
    let _lock = OPENCLAW_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("openclaw-state");
    let _env = EnvGuard::set("OPENCLAW_STATE_DIR", &state_dir);
    let _binary = EnvGuard::set("OPENCLAW_BIN", &tmp.path().join("missing-openclaw"));
    let session = session_with(
        vec![message(0, MessageRole::User, "Keep the live index intact")],
        None,
    );

    for force in [false, true] {
        let error = OpenClaw
            .write_session(&session, &WriteOptions { force })
            .expect_err("OpenClaw target writes require the official CLI");
        assert!(
            error.to_string().contains("official `openclaw` CLI"),
            "unexpected refusal: {error:#}"
        );
    }
    assert!(
        !state_dir.exists(),
        "the capability refusal must happen before touching provider state"
    );
}

#[test]
fn cli_refuses_without_official_openclaw_before_writing_state() {
    let tmp = tempfile::tempdir().unwrap();
    let clawdbot = tmp.path().join("clawdbot");
    std::fs::create_dir_all(&clawdbot).unwrap();
    std::fs::write(
        clawdbot.join("native-source.jsonl"),
        [
            r#"{"type":"session","version":2,"id":"native-source","timestamp":"2026-02-14T09:12:00.000Z","cwd":"/home/user/project"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-14T09:12:01.000Z","message":{"role":"user","content":"Keep the live index intact"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-14T09:12:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Understood."}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let state_dir = tmp.path().join("openclaw");

    for force in [false, true] {
        let mut args = vec![
            "--json",
            "resume",
            "ocl",
            "native-source",
            "--source",
            "cwb",
        ];
        if force {
            args.push("--force");
        }
        let output = StdCommand::new(env!("CARGO_BIN_EXE_casr"))
            .args(args)
            .env("CLAWDBOT_HOME", &clawdbot)
            .env("OPENCLAW_STATE_DIR", &state_dir)
            .env("OPENCLAW_BIN", tmp.path().join("missing-openclaw"))
            .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
            .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
            .env("NO_COLOR", "1")
            .output()
            .expect("casr should run");
        assert!(!output.status.success(), "OpenClaw target must refuse");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("official `openclaw` CLI"),
            "unexpected refusal: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        !state_dir.exists(),
        "CLI refusal must not create an OpenClaw transcript or index"
    );
}

// ---------------------------------------------------------------------------
// Side-appended entries are not a rewind
// ---------------------------------------------------------------------------

/// `appendMode: "side"` marks an entry parked by
/// `mergePromptReleasedSessionEntries` — "entries appended while the active
/// prompt released its file lock … attached as a side branch so rewrites
/// retain external state without moving the prepared reply branch".
///
/// casr is right that such an entry is not model-visible, and wrong to file it
/// under "abandoned", which is the report for a path the *user* rewound away
/// from.
#[test]
fn a_side_appended_entry_is_not_reported_as_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("side.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","version":3,"id":"side","timestamp":"2026-02-01T16:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Hi"}}"#,
            r#"{"type":"message","id":"s1","parentId":"m1","appendMode":"side","timestamp":"2026-02-01T16:00:02Z","message":{"role":"user","content":"delivered while the lock was released"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let session = OpenClaw.read_session(&path).expect("read_session");
    let unrepresented = session.metadata["unrepresented"]
        .as_str()
        .expect("the side-appended entry is reported")
        .to_string();

    assert_eq!(
        session.messages.len(),
        2,
        "the side branch is not model-visible"
    );
    assert!(
        !unrepresented.contains("abandoned"),
        "a side append is not a rewind; got {unrepresented:?}"
    );
    assert!(
        unrepresented.contains("side"),
        "it needs a counter that says what it is; got {unrepresented:?}"
    );
}

/// A genuine rewind still reports as abandoned, so the new counter did not just
/// rename the old one.
#[test]
fn a_rewound_branch_is_still_reported_as_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rewound.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","version":3,"id":"rewound","timestamp":"2026-02-01T16:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-02-01T16:00:01Z","message":{"role":"user","content":"Hi"}}"#,
            r#"{"type":"message","id":"dead","parentId":"m1","timestamp":"2026-02-01T16:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"rewound away"}]}}"#,
            r#"{"type":"leaf","id":"l1","targetId":"m1","timestamp":"2026-02-01T16:00:03Z"}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"kept"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let session = OpenClaw.read_session(&path).expect("read_session");
    let unrepresented = session.metadata["unrepresented"]
        .as_str()
        .expect("the rewound entry is reported")
        .to_string();
    assert!(unrepresented.contains("abandoned"), "got {unrepresented:?}");
}
