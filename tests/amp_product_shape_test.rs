//! Which Amp ags reads, stated as tests rather than as a comment.
//!
//! ags's Amp provider was described as reading "Amp's local JSON thread
//! store" without saying whose. Amp ships two products and they do not agree:
//!
//! * The **CLI** (`@ampcode/cli`) keeps thread bodies server-side. Its data
//!   directory — the same `<XDG_DATA_HOME>/amp` ags looks in — holds
//!   `daemon/`, `ide/`, `oauth/`, `runner/`, `device-id.json`,
//!   `history.jsonl`, `session.json` and `secrets.json`, and no transcript.
//! * The **editor extension** (`sourcegraph.amp`) writes one JSON file per
//!   thread into `<XDG_DATA_HOME>/amp/threads/`, and before that into
//!   `<globalStorage>/sourcegraph.amp/threads3/`.
//!
//! The fixtures below are built from the vendor's own layout, so what they
//! check is ags against Amp rather than ags against ags.

mod test_env;

use std::path::{Path, PathBuf};

use ags::providers::{Provider, amp::Amp};

static AMP_ENV: test_env::EnvLock = test_env::EnvLock;

/// Sets an environment variable for the life of the guard.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: the caller holds `AMP_ENV` for the whole test, which
        // serializes every environment read and write in this binary.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as above — the test's `AMP_ENV` guard outlives this one.
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Both roots Amp can use are pinned into temporary directories, so neither
/// this machine's real Amp state nor its absence can decide the outcome.
struct AmpFixture {
    _data: tempfile::TempDir,
    _config: tempfile::TempDir,
    _data_env: EnvGuard,
    _config_env: EnvGuard,
    data_dir: PathBuf,
    config_dir: PathBuf,
}

impl AmpFixture {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("data tmpdir");
        let config = tempfile::tempdir().expect("config tmpdir");
        let data_env = EnvGuard::set("XDG_DATA_HOME", data.path());
        let config_env = EnvGuard::set("XDG_CONFIG_HOME", config.path());
        let data_dir = data.path().to_path_buf();
        let config_dir = config.path().to_path_buf();
        Self {
            _data: data,
            _config: config,
            _data_env: data_env,
            _config_env: config_env,
            data_dir,
            config_dir,
        }
    }

    /// `<XDG_DATA_HOME>/amp`, shared by both products.
    fn amp_data(&self) -> PathBuf {
        self.data_dir.join("amp")
    }

    /// Everything the CLI puts in that directory, and nothing else.
    ///
    /// Enumerated from the `@ampcode/cli-linux-x64` 0.0.1785142937-gb7c681
    /// binary: the four directories it joins onto its data dir, plus the four
    /// files. There is no `threads` among them.
    fn with_cli_install(&self) -> &Self {
        let root = self.amp_data();
        for dir in ["daemon", "ide", "oauth", "runner", "notepad"] {
            std::fs::create_dir_all(root.join(dir)).expect("cli dir");
        }
        for (file, body) in [
            ("device-id.json", "{}"),
            ("history.jsonl", "\"amp -x hello\"\n"),
            // The CLI's local record of a thread is a *pointer* to one that
            // lives on the server, which is exactly why it is not readable.
            (
                "session.json",
                "{\"lastThreadId\":\"T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30\"}",
            ),
            ("secrets.json", "{}"),
        ] {
            std::fs::write(root.join(file), body).expect("cli file");
        }
        self
    }

    fn threads_root(&self) -> PathBuf {
        self.amp_data().join("threads")
    }

    fn legacy_threads_root(&self) -> PathBuf {
        self.config_dir
            .join("Code")
            .join("User")
            .join("globalStorage")
            .join("sourcegraph.amp")
            .join("threads3")
    }

    fn write_thread(root: &Path, file_name: &str, id: &str) -> PathBuf {
        std::fs::create_dir_all(root).expect("threads root");
        let path = root.join(file_name);
        let body = serde_json::json!({
            "v": 0,
            "id": id,
            "created": 1_700_000_000_000_i64,
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
                "meta": {"sentAt": 1_700_000_000_000_i64},
            }],
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&body).expect("thread json"),
        )
        .expect("write thread");
        path
    }
}

/// Every file under `root`, in sorted order.
fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// Which product is installed
// ---------------------------------------------------------------------------

/// A machine with only the Amp CLI has no Amp session ags can read, and
/// `detect` has to say that rather than claim a store it cannot list from.
#[test]
fn a_cli_only_install_is_not_reported_as_a_readable_store() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    amp.with_cli_install();

    let detection = Amp.detect();
    assert!(
        !detection.installed,
        "the CLI keeps threads server-side; there is nothing local to list: {:?}",
        detection.evidence
    );
    assert!(
        Amp.session_roots().is_empty(),
        "a data directory without threads/ is not a thread store: {:?}",
        Amp.session_roots()
    );

    // "Not installed" on a machine with a working `amp` is alarming enough
    // that the reason has to travel with it.
    let explained = detection.evidence.iter().any(|line| {
        line.contains("server-side") && line.contains(&amp.amp_data().display().to_string())
    });
    assert!(
        explained,
        "the empty result needs its explanation in the evidence: {:?}",
        detection.evidence
    );
}

/// The store ags does read belongs to the editor extension, and the evidence
/// says so — the whole defect was calling it "Amp's".
#[test]
fn the_detected_store_is_named_as_the_extensions() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    amp.with_cli_install();
    let threads = amp.threads_root();
    AmpFixture::write_thread(
        &threads,
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30.json",
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30",
    );

    let detection = Amp.detect();
    assert!(detection.installed, "{:?}", detection.evidence);
    assert!(
        detection
            .evidence
            .iter()
            .any(|line| line.contains(&threads.display().to_string())
                && line.contains("extension")),
        "the evidence must name whose store this is: {:?}",
        detection.evidence
    );
    assert_eq!(Amp.session_roots(), vec![threads]);
}

/// `threads3` under VS Code's `globalStorage` is not fictional: it was the
/// extension's only store through 0.0.1750505632 and was still read as a
/// migration source through 0.0.1760429452. A user who has not run a newer
/// build has every thread there and nowhere else.
#[test]
fn the_pre_migration_global_storage_root_is_still_read() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    let legacy = amp.legacy_threads_root();
    let id = "T-1c9f2b7e-5a44-4c30-8de6-90b1f2c3d4e5";
    let path = AmpFixture::write_thread(&legacy, &format!("{id}.json"), id);

    let detection = Amp.detect();
    assert!(
        detection.installed,
        "an un-migrated install is an install: {:?}",
        detection.evidence
    );
    assert!(
        Amp.session_roots().contains(&legacy),
        "expected {} in {:?}",
        legacy.display(),
        Amp.session_roots()
    );
    assert!(Amp.is_session_path(&path), "{} not listed", path.display());
    assert_eq!(Amp.owns_session(id).as_deref(), Some(path.as_path()));
}

// ---------------------------------------------------------------------------
// Listing and resolution answer about the same set
// ---------------------------------------------------------------------------

/// Whatever ags convert lists, ags must be able to resolve.
///
/// The listing rule is Amp's `keys()` — any `.json` file directly in a threads
/// root — so resolution has to accept the same names. It used to demand
/// `T-<uuid>`, which refused threads it had just printed.
#[test]
fn every_listed_thread_is_a_thread_ags_can_resolve() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    let threads = amp.threads_root();

    // Names Amp's own minter produces, and names it does not but still opens:
    // a file restored from a backup, and one renamed by hand.
    let listed = [
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30.json",
        "restored copy.json",
        "notes.json",
    ];
    for file_name in listed {
        let stem = file_name.trim_end_matches(".json");
        AmpFixture::write_thread(&threads, file_name, stem);
    }

    let paths = all_files(&threads);
    assert_eq!(paths.len(), listed.len(), "fixture: {paths:?}");
    for path in &paths {
        assert!(Amp.is_session_path(path), "{} not listed", path.display());
        let id = path.file_stem().and_then(|s| s.to_str()).expect("stem");
        assert_eq!(
            Amp.owns_session(id).as_deref(),
            Some(path.as_path()),
            "listed {} as {id} and then refused to resolve it",
            path.display()
        );
    }
}

/// The three things in a threads root that are not threads, plus the one
/// leftover Amp's atomic write can strand there.
///
/// Amp does a single `readdir` and drops every directory entry rather than
/// descending, and its filter is `!entry.isDirectory && name.endsWith(".json")`.
/// A recursive walk without that rule put a deleted thread back in front of
/// the user.
#[test]
fn nothing_below_the_threads_root_is_mistaken_for_a_thread() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    let threads = amp.threads_root();
    let real = AmpFixture::write_thread(
        &threads,
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30.json",
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30",
    );

    let decoys = [
        threads.join("attachments").join("diagram.json"),
        threads
            .join(".trash")
            .join("T-2d3e4f50-6172-4839-a0b1-c2d3e4f50617.json"),
        threads
            .join("blobs")
            .join("ab")
            .join("cd")
            .join("blob.json"),
    ];
    for decoy in &decoys {
        std::fs::create_dir_all(decoy.parent().expect("decoy parent")).expect("decoy dir");
        std::fs::write(decoy, "{}").expect("decoy file");
    }
    // `set()` writes `<id>.json.amptmp` and renames it; an interrupted save
    // leaves it behind. Amp's suffix test is what excludes it.
    let leftover = threads.join("T-2d3e4f50-6172-4839-a0b1-c2d3e4f50617.json.amptmp");
    std::fs::write(&leftover, "{}").expect("leftover");

    let listed: Vec<PathBuf> = all_files(&threads)
        .into_iter()
        .filter(|p| Amp.is_session_path(p))
        .collect();
    assert_eq!(listed, vec![real]);

    for decoy in &decoys {
        let id = decoy.file_stem().and_then(|s| s.to_str()).expect("stem");
        assert_eq!(
            Amp.owns_session(id),
            None,
            "{} is not a thread and its name must not resolve to one",
            decoy.display()
        );
    }
    assert!(!Amp.is_session_path(&leftover));
}

/// A session id is a key in one directory, never a path out of it.
#[test]
fn an_id_that_walks_out_of_the_threads_root_resolves_to_nothing() {
    let _lock = AMP_ENV.lock().unwrap();
    let amp = AmpFixture::new();
    let threads = amp.threads_root();
    AmpFixture::write_thread(
        &threads,
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30.json",
        "T-8b1d0f4a-3d2e-4a11-9b77-2a7c6d5e4f30",
    );
    // A readable file one level up, named so that `<id>.json` would reach it.
    std::fs::write(amp.amp_data().join("device-id.json"), "{}").expect("sibling");

    for escape in [
        "../device-id",
        "../../etc/passwd",
        "/etc/passwd",
        "sub/thread",
        "",
    ] {
        assert_eq!(
            Amp.owns_session(escape),
            None,
            "{escape:?} is not a key Amp's readdir could have produced"
        );
    }
}
