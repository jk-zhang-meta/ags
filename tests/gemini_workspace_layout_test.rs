//! `--workspace` must find Gemini sessions under the layout Gemini writes.
//!
//! `@google/gemini-cli-core@0.52.0` stopped naming a project's directory
//! `SHA256(workspace)` and started naming it with a **registry slug**
//! (`dist/src/config/storage.js:172-190`, `dist/src/config/projectRegistry.js`).
//! casr modelled only the hash form, so on any current install every Gemini
//! session failed the `--workspace` test as "workspace could not be
//! determined" and was hidden. A hidden session is worse than an over-listed
//! one, which is why these drive the binary and assert on what the user sees.
//!
//! The slug carries no path information — it is `slugify(basename(path))` plus
//! a collision counter — so the layout is read back from the ownership marker
//! Gemini drops beside it, `<id>/.project_root`, which holds the absolute
//! project path verbatim (`projectRegistry.js:17`, `:310-345`).
//!
//! Both layouts have to work at once: 0.52.0's migration *copies* the hash
//! directory to the slug directory and never removes the original
//! (`dist/src/config/storageMigration.js:35`, `fs.promises.cp`), so a migrated
//! machine has both and an un-migrated one has only the hash.

mod test_env;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use casr::model::{CanonicalMessage, CanonicalSession, MessageRole};
use casr::providers::{Provider, WriteOptions, gemini::Gemini};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

static GEMINI_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that sets an env var and restores the original value on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: Tests must hold `GEMINI_ENV` (see `test_env`) while mutating
        // the process environment and while invoking code that reads it.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: Same lock protects the restore.
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// A `casr` invocation whose every provider home points inside `tmp`.
///
/// `XDG_DATA_HOME` matters twice over: it is Amp's store *and* casr's own
/// session store, so leaving it unset would have these tests create
/// `~/.local/share/agsx` on the machine running them.
fn casr_cmd(tmp: &TempDir) -> Command {
    #[allow(deprecated)]
    let mut cmd = Command::cargo_bin("casr").expect("casr binary should be built");
    cmd.env("CLAUDE_HOME", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("GEMINI_HOME", tmp.path().join("gemini"))
        .env("CURSOR_HOME", tmp.path().join("cursor"))
        .env("CURSOR_CONFIG_DIR", tmp.path().join("cursor-cli-config"))
        .env("CURSOR_DATA_DIR", tmp.path().join("cursor-cli-data"))
        .env("CLINE_HOME", tmp.path().join("cline"))
        .env("AIDER_HOME", tmp.path().join("aider"))
        .env("OPENCODE_HOME", tmp.path().join("opencode"))
        .env("CHATGPT_HOME", tmp.path().join("chatgpt"))
        .env("CLAWDBOT_HOME", tmp.path().join("clawdbot"))
        .env("CLAWDBOT_STATE_DIR", tmp.path().join("clawdbot-state"))
        .env("VIBE_HOME", tmp.path().join("vibe"))
        .env("FACTORY_HOME", tmp.path().join("factory"))
        .env("OPENCLAW_HOME", tmp.path().join("openclaw"))
        .env("OPENCLAW_STATE_DIR", tmp.path().join("openclaw-state"))
        .env("PI_AGENT_HOME", tmp.path().join("pi-agent"))
        .env("KIRO_HOME", tmp.path().join("kiro"))
        .env("GROK_HOME", tmp.path().join("grok"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("NO_COLOR", "1")
        .current_dir(tmp.path());
    cmd
}

fn gemini_tmp(tmp: &TempDir) -> PathBuf {
    tmp.path().join("gemini").join("tmp")
}

/// A workspace directory inside the sandbox, created so the path is real.
fn workspace(tmp: &TempDir, name: &str) -> PathBuf {
    let ws = tmp.path().join("ws").join(name);
    fs::create_dir_all(&ws).expect("workspace should be creatable");
    ws
}

/// Pre-0.52.0 project directory name: `SHA256(absolute workspace path)`.
fn legacy_hash(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// A `~/.gemini/tmp/<id>/chats` directory holding one session for `ws`.
///
/// `marker` writes the `.project_root` ownership file 0.52.0's
/// `ProjectRegistry` creates for every directory it hands out
/// (`projectRegistry.js:310-345`); omitting it reproduces a pre-0.52.0
/// directory, which has none.
///
/// The `projectHash` header field is `SHA256(workspace)` either way: 0.52.0
/// still records it (`chatRecordingService.js:328`, `utils/paths.js:263`), it
/// is only the *directory* that stopped being the hash.
fn seed_session(tmp: &TempDir, dir_name: &str, ws: &Path, marker: bool, session_id: &str) {
    let project_dir = gemini_tmp(tmp).join(dir_name);
    let chats = project_dir.join("chats");
    fs::create_dir_all(&chats).expect("chats dir should be creatable");
    if marker {
        fs::write(
            project_dir.join(".project_root"),
            ws.to_string_lossy().as_bytes(),
        )
        .expect("marker should be writable");
    }
    // Header + two messages, in the JSONL form current Gemini writes.
    let body = format!(
        "{{\"sessionId\":\"{session_id}\",\"projectHash\":\"{}\",\
         \"startTime\":\"2026-03-02T09:00:00.000Z\",\
         \"lastUpdated\":\"2026-03-02T09:00:00.000Z\",\"kind\":\"chat\"}}\n\
         {{\"id\":\"u-1\",\"timestamp\":\"2026-03-02T09:00:05.000Z\",\
         \"type\":\"user\",\"content\":\"hello from {session_id}\"}}\n\
         {{\"id\":\"g-1\",\"timestamp\":\"2026-03-02T09:00:12.000Z\",\
         \"type\":\"gemini\",\"content\":\"hi\",\"model\":\"gemini-3-pro\"}}\n",
        legacy_hash(ws)
    );
    fs::write(
        chats.join(format!("session-2026-03-02T09-00-{session_id}.jsonl")),
        body,
    )
    .expect("session should be writable");
}

struct Listed {
    ids: Vec<String>,
    stderr: String,
}

impl Listed {
    fn hides_sessions(&self) -> bool {
        self.stderr.contains("hidden by --workspace")
    }
}

/// `casr list --provider gemini --workspace <ws> --json`, parsed.
fn list_for_workspace(tmp: &TempDir, ws: &Path) -> Listed {
    let output = casr_cmd(tmp)
        .args(["list", "--provider", "gemini", "--json", "--workspace"])
        .arg(ws)
        .output()
        .expect("casr list should run");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("list --json should emit an envelope: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });
    let ids = envelope["items"]
        .as_array()
        .expect("envelope should carry items")
        .iter()
        .map(|item| {
            item["session_id"]
                .as_str()
                .expect("item should carry session_id")
                .to_string()
        })
        .collect();
    Listed { ids, stderr }
}

/// The reported defect: on a current install `--workspace` hides everything.
///
/// Against unmodified source this fails with an empty listing and
/// "1 session(s) hidden by --workspace: their workspace could not be
/// determined (gemini)", because `workspace_path_hint` only knew how to test a
/// 64-hex directory name and a slug is not one.
#[test]
fn slug_layout_session_is_listed_for_its_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "my-project");
    seed_session(&tmp, "my-project", &ws, true, "slugsess");

    let listed = list_for_workspace(&tmp, &ws);
    assert_eq!(
        listed.ids,
        vec!["slugsess"],
        "a slug-layout session must be listed for the workspace its \
         .project_root marker names.\nstderr:\n{}",
        listed.stderr
    );
    assert!(
        !listed.hides_sessions(),
        "nothing should be reported hidden once the marker answers the \
         question.\nstderr:\n{}",
        listed.stderr
    );
}

/// The marker must exclude as confidently as it includes.
///
/// A slug directory naming *another* workspace is evidence of difference, not
/// absence of evidence — so the session is dropped silently rather than
/// counted as unclassifiable.
#[test]
fn slug_layout_other_workspace_is_excluded_without_a_warning() {
    let tmp = TempDir::new().expect("tempdir");
    let mine = workspace(&tmp, "mine");
    let theirs = workspace(&tmp, "theirs");
    seed_session(&tmp, "mine", &mine, true, "minesess");
    seed_session(&tmp, "theirs", &theirs, true, "theirsess");

    let listed = list_for_workspace(&tmp, &mine);
    assert_eq!(
        listed.ids,
        vec!["minesess"],
        "only the marked workspace's session belongs in the listing.\nstderr:\n{}",
        listed.stderr
    );
    assert!(
        !listed.hides_sessions(),
        "a marker naming another workspace is a determination, not a \
         failure to determine.\nstderr:\n{}",
        listed.stderr
    );
}

/// A machine that has never run 0.52.0 has only the hash layout, and it must
/// keep working — the migration is per-project and runs on first launch, so
/// "current CLI" and "hash directory" coexist indefinitely.
#[test]
fn legacy_hash_layout_is_still_matched() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "old-project");
    seed_session(&tmp, &legacy_hash(&ws), &ws, false, "legacysess");

    let listed = list_for_workspace(&tmp, &ws);
    assert_eq!(
        listed.ids,
        vec!["legacysess"],
        "the pre-0.52.0 SHA256 directory must still resolve.\nstderr:\n{}",
        listed.stderr
    );
    assert!(
        !listed.hides_sessions(),
        "the hash layout was always determinable.\nstderr:\n{}",
        listed.stderr
    );
}

/// After migration a project owns *both* directories, and the live one is the
/// slug.
///
/// `StorageMigration.migrateDirectory` copies rather than moves
/// (`storageMigration.js:35`), so the hash directory survives with a frozen
/// snapshot while every session written afterwards lands only under the slug.
/// Against unmodified source this fails by listing `premigration` alone: the
/// workspace-scoped fast path finds `tmp/<sha256>/chats` on disk, takes it as
/// *the* answer, and never looks at the slug directory.
#[test]
fn migrated_project_lists_sessions_from_both_directories() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "migrated");
    // The frozen copy the migration left behind.
    seed_session(&tmp, &legacy_hash(&ws), &ws, false, "premigrate");
    // The same session, copied into the slug directory by the migration.
    seed_session(&tmp, "migrated", &ws, true, "premigrate");
    // Written after the migration, so it exists only under the slug.
    seed_session(&tmp, "migrated", &ws, true, "postmigrate");

    let listed = list_for_workspace(&tmp, &ws);
    let mut ids = listed.ids.clone();
    ids.sort();
    assert_eq!(
        ids,
        vec!["postmigrate", "premigrate"],
        "both halves of a migrated project belong to the same workspace, and \
         the copied session must still collapse to one entry.\nstderr:\n{}",
        listed.stderr
    );
}

/// A minimal session that claims `ws` as its workspace.
fn session_for(ws: &Path) -> CanonicalSession {
    CanonicalSession {
        session_id: "src-simple".to_string(),
        provider_slug: "test-source".to_string(),
        workspace: Some(ws.to_path_buf()),
        title: Some("Fix the login bug".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_010_000),
        messages: vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Fix the login bug".to_string(),
            timestamp: Some(1_700_000_000_000),
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        }],
        metadata: serde_json::json!({"source": "test"}),
        source_path: PathBuf::from("/tmp/source.jsonl"),
        model_name: None,
    }
}

/// A converted session must land where the CLI reads, not where it used to.
///
/// Once a project has been opened under 0.52.0 its slug directory is
/// non-empty, and the migration that would have carried a hash directory
/// across refuses to run against a non-empty destination
/// (`storageMigration.js:23-30`). So a session written to
/// `tmp/<SHA256(ws)>/chats` on such a machine is one Gemini will never show.
#[test]
fn write_targets_the_registered_project_directory() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "written");
    // A project Gemini has already registered: marker present, chats present.
    seed_session(&tmp, "written", &ws, true, "existing");
    let _env = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let written = Gemini
        .write_session(&session_for(&ws), &WriteOptions { force: false })
        .expect("write should succeed");

    assert_eq!(
        written.paths[0].parent().and_then(|p| p.parent()),
        Some(gemini_tmp(&tmp).join("written").as_path()),
        "the session belongs in the directory the .project_root marker \
         claims for this workspace, not in tmp/<sha256>/"
    );
}

/// With no registered directory the hash is still the right guess.
///
/// It is not a guess about the slug — which cannot be computed — but about the
/// migration: an unregistered project has no slug directory yet, so the next
/// 0.52.0 launch finds a fresh destination and copies the hash directory into
/// it (`storage.js:195-207`).
#[test]
fn write_falls_back_to_the_hash_directory_when_unregistered() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "unregistered");
    let _env = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let written = Gemini
        .write_session(&session_for(&ws), &WriteOptions { force: false })
        .expect("write should succeed");

    assert_eq!(
        written.paths[0].parent().and_then(|p| p.parent()),
        Some(gemini_tmp(&tmp).join(legacy_hash(&ws)).as_path()),
        "an unregistered workspace still gets the hash directory"
    );
}

/// A directory that says nothing is not a session that says nothing.
///
/// `mystery-dir` is neither marked nor a hash, so nothing about the *directory*
/// places it — and the `--workspace` fast path still refuses to answer from the
/// directories that did classify, which is the only reason `mysterysess`
/// reaches the general listing at all. What places it there is its own file:
/// `projectHash` is `SHA256(projectRoot)` (`chatRecordingService.js:328`,
/// `utils/paths.js:263`), which this helper writes because Gemini writes it,
/// and it names this workspace exactly.
///
/// The field used to be parsed into `metadata.project_hash` and read by
/// nothing, so this session was hidden with "workspace could not be determined"
/// while the file it was hidden on the strength of said which workspace it was.
/// Listing it is not a guess widened — it is a second witness consulted, after
/// the directory has declined and only for the file it belongs to.
#[test]
fn a_session_in_an_unclassifiable_directory_is_placed_by_its_own_header() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "known");
    seed_session(&tmp, "known", &ws, true, "knownsess");
    seed_session(&tmp, "mystery-dir", &ws, false, "mysterysess");

    let listed = list_for_workspace(&tmp, &ws);
    let mut ids = listed.ids.clone();
    ids.sort();
    assert_eq!(
        ids,
        vec!["knownsess", "mysterysess"],
        "the header names this workspace, so the session belongs in the \
         listing however its directory is named.\nstderr:\n{}",
        listed.stderr
    );
    assert!(
        !listed.hides_sessions(),
        "nothing was hidden, so nothing should be reported as hidden.\
         \nstderr:\n{}",
        listed.stderr
    );
}

/// The same directory, and a session whose header names somewhere else.
///
/// The witness has to answer both ways or it is not a witness: a `projectHash`
/// that is some other workspace's excludes the session exactly as a
/// `.project_root` naming some other workspace would. Excluded on evidence is
/// not the same as hidden for lack of it, so nothing is reported as hidden here
/// either.
#[test]
fn a_session_whose_header_names_another_workspace_is_excluded() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = workspace(&tmp, "known");
    let other = workspace(&tmp, "other");
    seed_session(&tmp, "known", &ws, true, "knownsess");
    seed_session(&tmp, "mystery-dir", &other, false, "elsewheresess");

    let listed = list_for_workspace(&tmp, &ws);
    assert_eq!(
        listed.ids,
        vec!["knownsess"],
        "a header naming another workspace places the session there.\
         \nstderr:\n{}",
        listed.stderr
    );
    assert!(
        !listed.hides_sessions(),
        "excluded on evidence, not hidden for want of it.\nstderr:\n{}",
        listed.stderr
    );
}
