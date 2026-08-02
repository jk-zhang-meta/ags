//! Vendor-owned target tests for the Cline provider.
//!
//! Set `CLINE_TEST_BIN` to an official Cline CLI. The test uses an isolated
//! data directory and no credentials, then exercises the public provider API
//! plus Cline's own history discovery and deletion lifecycle.

mod test_env;

use std::path::{Path, PathBuf};
use std::process::Command;

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult};
use casr::providers::cline::Cline;
use casr::providers::{Provider, WriteOptions};

static CLINE_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let guard = Self {
            key,
            original: std::env::var_os(key),
        };
        // SAFETY: the test holds CLINE_ENV until all guards are dropped.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn unset(key: &'static str) -> Self {
        let guard = Self {
            key,
            original: std::env::var_os(key),
        };
        // SAFETY: the test holds CLINE_ENV until all guards are dropped.
        unsafe { std::env::remove_var(key) };
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: CLINE_ENV still guards the process environment.
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn change_to(path: &Path) -> Self {
        let original = std::env::current_dir().expect("read cwd");
        std::env::set_current_dir(path).expect("set isolated cwd");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Field order is intentional: the environment lock drops last.
struct Sandbox {
    _cline_dir: EnvGuard,
    _cline_data_dir: EnvGuard,
    _cline_home: EnvGuard,
    _cline_bin: EnvGuard,
    _home: EnvGuard,
    _cwd: CwdGuard,
    binary: PathBuf,
    data_dir: PathBuf,
    workspace: PathBuf,
    _tmp: tempfile::TempDir,
    _lock: test_env::EnvLockGuard<'static>,
}

impl Sandbox {
    fn new(binary: &Path) -> Self {
        let lock = CLINE_ENV.lock().expect("environment lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let data_dir = tmp.path().join("cline-data");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");

        Self {
            _cline_dir: EnvGuard::unset("CLINE_DIR"),
            _cline_data_dir: EnvGuard::unset("CLINE_DATA_DIR"),
            _cline_home: EnvGuard::set("CLINE_HOME", &data_dir),
            _cline_bin: EnvGuard::set("CLINE_BIN", binary),
            _home: EnvGuard::set("HOME", &home),
            _cwd: CwdGuard::change_to(&workspace),
            binary: binary.to_path_buf(),
            data_dir,
            workspace,
            _tmp: tmp,
            _lock: lock,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .current_dir(&self.workspace)
            .env("CLINE_DATA_DIR", &self.data_dir)
            .env("CLINE_NO_AUTO_UPDATE", "1")
            .env("CLINE_TELEMETRY_DISABLED", "1");
        command
    }

    fn session(&self) -> CanonicalSession {
        CanonicalSession {
            session_id: "source-session".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(self.workspace.clone()),
            title: Some("Cline official import".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_003_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "CLINE_IMPORT_SENTINEL_USER".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "CLINE_IMPORT_SENTINEL_ASSISTANT".to_string(),
                    timestamp: Some(1_700_000_001_000),
                    author: Some("claude-test".to_string()),
                    tool_calls: vec![ToolCall {
                        id: Some("cline-call-1".to_string()),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "README.md"}),
                    }],
                    tool_results: vec![],
                    extra: serde_json::json!({}),
                },
                CanonicalMessage {
                    idx: 2,
                    role: MessageRole::Tool,
                    content: String::new(),
                    timestamp: Some(1_700_000_002_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![ToolResult {
                        call_id: Some("cline-call-1".to_string()),
                        content: "CLINE_IMPORT_SENTINEL_TOOL_RESULT".to_string(),
                        is_error: false,
                    }],
                    extra: serde_json::json!({}),
                },
                CanonicalMessage {
                    idx: 3,
                    role: MessageRole::Assistant,
                    content: "CLINE_IMPORT_SENTINEL_FINAL".to_string(),
                    timestamp: Some(1_700_000_003_000),
                    author: Some("claude-test".to_string()),
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::json!({}),
                },
            ],
            metadata: serde_json::json!({}),
            source_path: self.workspace.join("source.jsonl"),
            model_name: Some("claude-test".to_string()),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.command().args(["hub", "stop"]).output();
    }
}

#[test]
fn official_cli_creates_discovers_reads_and_deletes_the_written_session() {
    let Some(binary) = std::env::var_os("CLINE_TEST_BIN").map(PathBuf::from) else {
        return;
    };
    assert!(
        binary.is_file(),
        "CLINE_TEST_BIN is not a file: {}",
        binary.display()
    );

    let sandbox = Sandbox::new(&binary);
    let written = Cline
        .write_session(&sandbox.session(), &WriteOptions { force: false })
        .expect("official Cline Hub import");
    assert_eq!(written.paths.len(), 1);
    assert_eq!(
        written.resume_command,
        format!("cline --id {}", written.session_id)
    );

    let readback = Cline
        .read_session(&written.paths[0])
        .expect("read official Cline messages artifact");
    assert_eq!(readback.messages.len(), 4);
    assert_eq!(readback.messages[0].content, "CLINE_IMPORT_SENTINEL_USER");
    assert_eq!(readback.messages[1].tool_calls.len(), 1);
    assert_eq!(readback.messages[1].tool_calls[0].name, "read_file");
    assert_eq!(readback.messages[2].role, MessageRole::User);
    assert_eq!(readback.messages[2].tool_results.len(), 1);
    assert_eq!(
        readback.messages[2].tool_results[0].content,
        "CLINE_IMPORT_SENTINEL_TOOL_RESULT"
    );

    let history = sandbox
        .command()
        .args(["history", "--json", "--limit", "100"])
        .output()
        .expect("official Cline history");
    assert!(
        history.status.success(),
        "official history failed: {}",
        String::from_utf8_lossy(&history.stderr)
    );
    let history: serde_json::Value =
        serde_json::from_slice(&history.stdout).expect("official history JSON");
    assert!(
        history.to_string().contains(&written.session_id),
        "official history did not discover {}",
        written.session_id
    );

    Cline
        .rollback_write(&written)
        .expect("official Cline session deletion");
    let history = sandbox
        .command()
        .args(["history", "--json", "--limit", "100"])
        .output()
        .expect("official Cline history after deletion");
    assert!(history.status.success());
    let history: serde_json::Value =
        serde_json::from_slice(&history.stdout).expect("official history JSON after deletion");
    assert!(
        !history.to_string().contains(&written.session_id),
        "official history still contains deleted session {}",
        written.session_id
    );
    assert!(
        !written.paths[0].exists(),
        "official deletion left the messages artifact behind"
    );
}
