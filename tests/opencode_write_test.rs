//! Vendor-owned target tests for the OpenCode provider.
//!
//! OpenCode owns the schema and migration invariants of `opencode.db`, so casr
//! never writes SQLite. Ordinary tests prove a missing official CLI fails
//! before touching the store. Set `OPENCODE_TEST_BIN` to an official binary to
//! exercise the real import/export/delete lifecycle in an isolated database.

mod test_env;

use std::path::{Path, PathBuf};
use std::process::Command;

use ags::model::{CanonicalMessage, CanonicalSession, MessageRole};
use ags::providers::opencode::OpenCode;
use ags::providers::{Provider, WriteOptions};

static OPENCODE_ENV: test_env::EnvLock = test_env::EnvLock;

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
        // SAFETY: every test holds OPENCODE_ENV until all guards are dropped.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn unset(key: &'static str) -> Self {
        let guard = Self {
            key,
            original: std::env::var_os(key),
        };
        // SAFETY: every test holds OPENCODE_ENV until all guards are dropped.
        unsafe { std::env::remove_var(key) };
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: OPENCODE_ENV still guards the process environment.
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
    _binary: EnvGuard,
    _db_path: EnvGuard,
    _oc_home: EnvGuard,
    _oc_db: EnvGuard,
    _xdg_data: EnvGuard,
    _xdg_config: EnvGuard,
    _home: EnvGuard,
    _cwd: CwdGuard,
    tmp: tempfile::TempDir,
    workspace: PathBuf,
    _lock: test_env::EnvLockGuard<'static>,
}

impl Sandbox {
    fn new() -> Self {
        Self::with_binary(None)
    }

    fn with_official_binary(binary: &Path) -> Self {
        Self::with_binary(Some(binary))
    }

    fn with_binary(binary: Option<&Path>) -> Self {
        let lock = OPENCODE_ENV.lock().expect("environment lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let binary = binary
            .map(Path::to_path_buf)
            .unwrap_or_else(|| tmp.path().join("missing-opencode"));

        Self {
            _binary: EnvGuard::set("OPENCODE_BIN", &binary),
            _db_path: EnvGuard::unset("OPENCODE_DB_PATH"),
            _oc_home: EnvGuard::unset("OPENCODE_HOME"),
            _oc_db: EnvGuard::unset("OPENCODE_DB"),
            _xdg_data: EnvGuard::set("XDG_DATA_HOME", &home.join("share")),
            _xdg_config: EnvGuard::set("XDG_CONFIG_HOME", &home.join("config")),
            _home: EnvGuard::set("HOME", &home),
            _cwd: CwdGuard::change_to(&workspace),
            tmp,
            workspace,
            _lock: lock,
        }
    }

    fn session(&self) -> CanonicalSession {
        CanonicalSession {
            session_id: "source-session".to_string(),
            provider_slug: "claude-code".to_string(),
            workspace: Some(self.workspace.clone()),
            title: Some("OpenCode refusal".to_string()),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_001_000),
            messages: vec![
                CanonicalMessage {
                    idx: 0,
                    role: MessageRole::User,
                    content: "OPEN_CODE_IMPORT_SENTINEL_USER".to_string(),
                    timestamp: Some(1_700_000_000_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::json!({}),
                },
                CanonicalMessage {
                    idx: 1,
                    role: MessageRole::Assistant,
                    content: "OPEN_CODE_IMPORT_SENTINEL_ASSISTANT".to_string(),
                    timestamp: Some(1_700_000_001_000),
                    author: None,
                    tool_calls: vec![],
                    tool_results: vec![],
                    extra: serde_json::json!({}),
                },
            ],
            metadata: serde_json::json!({}),
            source_path: self.workspace.join("source.jsonl"),
            model_name: None,
        }
    }

    fn possible_database_paths(&self) -> [PathBuf; 3] {
        [
            self.workspace.join(".opencode/opencode.db"),
            self.tmp.path().join("home/share/opencode/opencode.db"),
            self.tmp.path().join("home/.opencode/opencode.db"),
        ]
    }
}

fn assert_missing_binary(session: &CanonicalSession, force: bool) {
    let error = OpenCode
        .write_session(session, &WriteOptions { force })
        .expect_err("OpenCode target writes require the official CLI");
    assert!(
        error.to_string().contains("OPENCODE_BIN"),
        "unexpected refusal: {error:#}"
    );
}

fn current_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode-current/opencode.db")
}

fn database_files(path: &Path) -> [PathBuf; 3] {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
}

fn seed_vendor_database(path: &Path) {
    std::fs::copy(current_fixture(), path).expect("copy current fixture");
    let [_, wal, shm] = database_files(path);
    std::fs::write(wal, b"existing vendor WAL sentinel").expect("write WAL sentinel");
    std::fs::write(shm, b"existing vendor SHM sentinel").expect("write SHM sentinel");
}

fn snapshot_database(path: &Path) -> [(PathBuf, Vec<u8>); 3] {
    database_files(path).map(|file| {
        let bytes = std::fs::read(&file)
            .unwrap_or_else(|error| panic!("snapshot {}: {error}", file.display()));
        (file, bytes)
    })
}

fn assert_database_unchanged(before: &[(PathBuf, Vec<u8>); 3]) {
    for (file, expected) in before {
        let actual = std::fs::read(file)
            .unwrap_or_else(|error| panic!("read unchanged {}: {error}", file.display()));
        assert_eq!(
            &actual,
            expected,
            "refusal changed persistent OpenCode state at {}",
            file.display()
        );
    }
}

fn copy_official_binary_into_workspace(sandbox: &Sandbox, binary: &Path) -> PathBuf {
    let bin_dir = sandbox.workspace.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("relative binary dir");
    let filename = binary.file_name().expect("official binary filename");
    let target = bin_dir.join(filename);
    std::fs::copy(binary, &target).expect("copy official binary");
    target
        .strip_prefix(&sandbox.workspace)
        .expect("binary is below sandbox workspace")
        .to_path_buf()
}

#[test]
fn missing_cli_refuses_force_and_default_without_creating_a_database() {
    let sandbox = Sandbox::new();
    let session = sandbox.session();

    assert_missing_binary(&session, false);
    assert_missing_binary(&session, true);

    for path in sandbox.possible_database_paths() {
        for file in database_files(&path) {
            assert!(!file.exists(), "refusal created {}", file.display());
        }
    }
}

#[test]
fn missing_cli_preserves_an_explicit_vendor_database() {
    let sandbox = Sandbox::new();
    let target = sandbox.tmp.path().join("explicit/opencode.db");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("target dir");
    seed_vendor_database(&target);
    let _target = EnvGuard::set("OPENCODE_DB_PATH", &target);
    let before = snapshot_database(&target);

    assert_missing_binary(&sandbox.session(), false);
    assert_database_unchanged(&before);
    assert_missing_binary(&sandbox.session(), true);
    assert_database_unchanged(&before);
}

#[test]
fn missing_cli_preserves_an_opencode_home_database() {
    let sandbox = Sandbox::new();
    let opencode_home = sandbox.tmp.path().join("isolated-opencode-home");
    std::fs::create_dir_all(&opencode_home).expect("OpenCode home");
    let target = opencode_home.join("opencode.db");
    seed_vendor_database(&target);
    let _home = EnvGuard::set("OPENCODE_HOME", &opencode_home);
    let before = snapshot_database(&target);

    assert_missing_binary(&sandbox.session(), false);
    assert_database_unchanged(&before);
    assert_missing_binary(&sandbox.session(), true);
    assert_database_unchanged(&before);
}

#[test]
fn official_cli_imports_exports_and_deletes_the_written_session() {
    let Some(binary) = std::env::var_os("OPENCODE_TEST_BIN").map(PathBuf::from) else {
        return;
    };
    assert!(
        binary.is_file(),
        "OPENCODE_TEST_BIN is not a file: {}",
        binary.display()
    );

    let sandbox = Sandbox::with_official_binary(&binary);
    let target = sandbox.tmp.path().join("official/opencode.db");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("target dir");
    let _target = EnvGuard::set("OPENCODE_DB_PATH", &target);
    let session = sandbox.session();

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("official OpenCode import");
    assert_eq!(written.paths.len(), 1);
    assert_eq!(
        written.paths[0].parent(),
        Some(target.as_path()),
        "the read-back locator must name the isolated vendor database"
    );

    let readback = OpenCode
        .read_session(&written.paths[0])
        .expect("casr reads the vendor-imported session");
    assert_eq!(readback.messages.len(), 2);
    assert_eq!(
        readback.messages[0].content,
        "OPEN_CODE_IMPORT_SENTINEL_USER"
    );
    assert_eq!(
        readback.messages[1].content,
        "OPEN_CODE_IMPORT_SENTINEL_ASSISTANT"
    );

    let export = Command::new(&binary)
        .current_dir(&sandbox.workspace)
        .env("OPENCODE_DB", &target)
        .env("OPENCODE_PURE", "1")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .args(["export", &written.session_id])
        .output()
        .expect("official OpenCode export");
    assert!(
        export.status.success(),
        "official export failed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let exported: serde_json::Value =
        serde_json::from_slice(&export.stdout).expect("official export JSON");
    let exported_text = exported.to_string();
    assert!(exported_text.contains("OPEN_CODE_IMPORT_SENTINEL_USER"));
    assert!(exported_text.contains("OPEN_CODE_IMPORT_SENTINEL_ASSISTANT"));

    OpenCode
        .rollback_write(&written)
        .expect("official OpenCode session deletion");
    assert!(
        OpenCode.read_session(&written.paths[0]).is_err(),
        "the deleted session must no longer be readable"
    );
    assert!(
        target.is_file(),
        "rollback deletes only the imported session, never the shared database"
    );
}

#[test]
fn official_cli_resolves_relative_binary_and_database_before_switching_cwd() {
    let Some(binary) = std::env::var_os("OPENCODE_TEST_BIN").map(PathBuf::from) else {
        return;
    };
    assert!(
        binary.is_file(),
        "OPENCODE_TEST_BIN is not a file: {}",
        binary.display()
    );

    let sandbox = Sandbox::with_official_binary(&binary);
    let relative_binary = copy_official_binary_into_workspace(&sandbox, &binary);
    let _binary = EnvGuard::set("OPENCODE_BIN", &relative_binary);
    let relative_target = PathBuf::from("relative-db/opencode.db");
    let _target = EnvGuard::set("OPENCODE_DB_PATH", &relative_target);
    let child_workspace = sandbox.tmp.path().join("different-workspace");
    std::fs::create_dir_all(&child_workspace).expect("different workspace");
    let mut session = sandbox.session();
    session.workspace = Some(child_workspace);

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("official import with relative binary and database");
    let expected_target = sandbox.workspace.join(relative_target);
    assert_eq!(
        written.paths[0].parent(),
        Some(expected_target.as_path()),
        "relative paths must resolve before the child switches cwd"
    );
    OpenCode
        .read_session(&written.paths[0])
        .expect("read relative-path import");
    OpenCode
        .rollback_write(&written)
        .expect("delete relative-path import");
}

#[test]
fn official_cli_resolves_relative_opencode_home_before_switching_cwd() {
    let Some(binary) = std::env::var_os("OPENCODE_TEST_BIN").map(PathBuf::from) else {
        return;
    };
    assert!(
        binary.is_file(),
        "OPENCODE_TEST_BIN is not a file: {}",
        binary.display()
    );

    let sandbox = Sandbox::with_official_binary(&binary);
    let relative_home = PathBuf::from("relative-home");
    let _home = EnvGuard::set("OPENCODE_HOME", &relative_home);
    let child_workspace = sandbox.tmp.path().join("different-workspace");
    std::fs::create_dir_all(&child_workspace).expect("different workspace");
    let mut session = sandbox.session();
    session.workspace = Some(child_workspace);

    let written = OpenCode
        .write_session(&session, &WriteOptions { force: false })
        .expect("official import with relative OpenCode home");
    let expected_target = sandbox.workspace.join(relative_home).join("opencode.db");
    assert_eq!(
        written.paths[0].parent(),
        Some(expected_target.as_path()),
        "relative OpenCode home must resolve before the child switches cwd"
    );
    OpenCode
        .read_session(&written.paths[0])
        .expect("read relative-home import");
    OpenCode
        .rollback_write(&written)
        .expect("delete relative-home import");
}
