//! Fail-closed target tests for the OpenCode provider.
//!
//! OpenCode owns the schema and migration invariants of `opencode.db`. Until
//! casr can use OpenCode's own import machinery and verify a real resume, every
//! target mode must refuse without creating or changing that database.

mod test_env;

use std::path::{Path, PathBuf};

use casr::model::{CanonicalMessage, CanonicalSession, MessageRole};
use casr::providers::opencode::OpenCode;
use casr::providers::{Provider, WriteOptions};

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
        let lock = OPENCODE_ENV.lock().expect("environment lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");

        Self {
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
            messages: vec![CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "Do not mutate the database".to_string(),
                timestamp: Some(1_700_000_000_000),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: serde_json::json!({}),
            }],
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

fn assert_refused(session: &CanonicalSession, force: bool) {
    let error = OpenCode
        .write_session(session, &WriteOptions { force })
        .expect_err("OpenCode target writes must fail closed");
    assert!(
        error.to_string().contains("OpenCode is read/resume-only"),
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

#[test]
fn fresh_store_refuses_force_and_default_without_creating_a_database() {
    let sandbox = Sandbox::new();
    let session = sandbox.session();

    assert_refused(&session, false);
    assert_refused(&session, true);

    for path in sandbox.possible_database_paths() {
        for file in database_files(&path) {
            assert!(!file.exists(), "refusal created {}", file.display());
        }
    }
}

#[test]
fn explicit_database_refuses_and_preserves_vendor_fixture_bytes() {
    let sandbox = Sandbox::new();
    let target = sandbox.tmp.path().join("explicit/opencode.db");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("target dir");
    seed_vendor_database(&target);
    let _target = EnvGuard::set("OPENCODE_DB_PATH", &target);
    let before = snapshot_database(&target);

    assert_refused(&sandbox.session(), false);
    assert_database_unchanged(&before);
    assert_refused(&sandbox.session(), true);
    assert_database_unchanged(&before);
}

#[test]
fn opencode_home_database_refuses_and_preserves_vendor_fixture_bytes() {
    let sandbox = Sandbox::new();
    let opencode_home = sandbox.tmp.path().join("isolated-opencode-home");
    std::fs::create_dir_all(&opencode_home).expect("OpenCode home");
    let target = opencode_home.join("opencode.db");
    seed_vendor_database(&target);
    let _home = EnvGuard::set("OPENCODE_HOME", &opencode_home);
    let before = snapshot_database(&target);

    assert_refused(&sandbox.session(), false);
    assert_database_unchanged(&before);
    assert_refused(&sandbox.session(), true);
    assert_database_unchanged(&before);
}
