//! The second store each of Kiro and Cursor keeps, and the listing that used
//! to miss it.
//!
//! Both tools ship as two products sharing one home directory. `detect()` fires
//! on the home directory, so a reader that only understands one product renders
//! as "installed, 0 sessions" for every user of the other — a result the user
//! cannot tell apart from having no sessions at all. These tests seed the store
//! that was not being read, in the byte-exact layout the shipping tool writes,
//! and assert it is now listed.
//!
//! The layouts are not invented. Both bucket names below were produced by
//! running the vendors' own path functions, copied verbatim out of the shipped
//! packages, under node:
//!
//! * `b550143462be8201` — Kiro IDE 1.0.212,
//!   `@kiro/agent/dist/workspace-hash-Dq3QXXpU.js`,
//!   `sha256(paths.sorted().join("\0")).hex[..16]` over `["/home/u/demo-project"]`.
//! * `6ec6c2923210e29ebc9bf9e34db81429` — cursor-agent 2026.07.23-e383d2b,
//!   `./src/state/index.ts`, `md5(resolve(cwd)).hex` over the same path.
//! * `home-u-demo-project` — cursor-agent `../utils/dist/workspace-paths.js`,
//!   `s.replace(/[^a-zA-Z0-9]/g, "-")…` over the same path.
//!
//! These tests run the `casr` binary rather than the provider directly, because
//! the gap they cover is not "the reader is wrong" — it is "`list` never asked
//! the reader about these files".

mod test_env;

use std::path::{Path, PathBuf};

use casr::providers::Provider;
use casr::providers::cursor::Cursor;
use casr::providers::kiro::Kiro;

static ENV: test_env::EnvLock = test_env::EnvLock;

const WORKSPACE: &str = "/home/u/demo-project";
const KIRO_BUCKET: &str = "b550143462be8201";
const KIRO_IDE_ID: &str = "sess_9c1f4c2e-6b0a-4f71-8a55-2f0d7b3ac914";
const CURSOR_CHATS_BUCKET: &str = "6ec6c2923210e29ebc9bf9e34db81429";
const CURSOR_PROJECT_SLUG: &str = "home-u-demo-project";
const CURSOR_AGENT_ID: &str = "7a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
/// A chat with a store but no transcript — created, never prompted.
const CURSOR_ORPHAN_ID: &str = "ffffffff-1111-2222-3333-444444444444";

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold `ENV` for the duration, so no other thread reads
        // or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl EnvGuard {
    /// Unset a variable for the duration, so a test can assert what happens
    /// when the user has not set it.
    fn remove(key: &'static str) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold `ENV` for the duration.
        unsafe { std::env::remove_var(key) };
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// `<root>/sessions/<bucket>/<sess_id>/{session.json,messages.jsonl}`, the
/// layout `SessionPersistence.getSessionPath` + `saveSession` produce. `root`
/// is a `.kiro` directory.
fn seed_kiro_ide(home: &Path) {
    let dir = home.join("sessions").join(KIRO_BUCKET).join(KIRO_IDE_ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        fixtures_dir().join("kiro/ide_session.json"),
        dir.join("session.json"),
    )
    .unwrap();
    std::fs::copy(
        fixtures_dir().join("kiro/ide_messages.jsonl"),
        dir.join("messages.jsonl"),
    )
    .unwrap();
}

/// The Kiro CLI triplet, at `<root>/sessions/cli/<uuid>.{json,jsonl,history}`.
/// Returns the session id.
fn seed_kiro_cli(root: &Path) -> String {
    let cli = root.join("sessions").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let id = "0a5376f2-7e2f-4981-bcbc-67195586604a";
    for ext in ["json", "jsonl", "history"] {
        std::fs::copy(
            fixtures_dir().join(format!("kiro/{id}.{ext}")),
            cli.join(format!("{id}.{ext}")),
        )
        .unwrap();
    }
    id.to_string()
}

/// `<root>/projects/<slug>/agent-transcripts/<id>/<id>.jsonl` and
/// `<root>/chats/<md5(cwd)>/<id>/store.db`, for both the conversation that has
/// both halves and the one that has only a chat store.
fn seed_cursor_agent(root: &Path) {
    let transcripts = root
        .join("projects")
        .join(CURSOR_PROJECT_SLUG)
        .join("agent-transcripts")
        .join(CURSOR_AGENT_ID);
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::copy(
        fixtures_dir().join("cursor/cursor_agent_transcript.jsonl"),
        transcripts.join(format!("{CURSOR_AGENT_ID}.jsonl")),
    )
    .unwrap();

    for id in [CURSOR_AGENT_ID, CURSOR_ORPHAN_ID] {
        let chat = root.join("chats").join(CURSOR_CHATS_BUCKET).join(id);
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::copy(
            fixtures_dir().join("cursor/cursor_agent_chat_store.db"),
            chat.join("store.db"),
        )
        .unwrap();
    }
}

/// Run `casr list --json` against a seeded home, with the store kept out of the
/// way of any real one.
fn list_json(
    provider: &str,
    extra: &[&str],
    envs: &[(&str, &Path)],
    store: &Path,
) -> serde_json::Value {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_casr"));
    cmd.args(["list", "--provider", provider, "--limit", "50", "--json"]);
    cmd.args(extra);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd
        .env("XDG_DATA_HOME", store)
        .output()
        .expect("run casr list");
    assert!(
        output.status.success(),
        "casr list failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("list --json emits an envelope")
}

fn ids(envelope: &serde_json::Value) -> Vec<String> {
    envelope["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["session_id"].as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Kiro IDE
// ---------------------------------------------------------------------------

/// Fails before the fix: `list_sessions` only ever read `sessions/cli`, so an
/// IDE-only `~/.kiro` produced an empty listing while `detect()` reported the
/// tool installed.
///
/// The store is relocated with `HOME`, not `KIRO_HOME`, because that is what
/// actually relocates it: the IDE resolves `os.homedir()/.kiro/sessions` and
/// has no relocation variable of its own.
#[test]
fn kiro_ide_sessions_are_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_kiro_ide(&tmp.path().join(".kiro"));

    // The fixture's workspace is not this test's cwd, and `list` scopes to the
    // working-directory project by default, so the scope is named explicitly.
    let envelope = list_json(
        "kiro",
        &["--workspace", WORKSPACE],
        &[("HOME", tmp.path())],
        store.path(),
    );
    assert_eq!(
        ids(&envelope),
        vec![KIRO_IDE_ID],
        "the IDE session must be listed; before the fix this was empty while \
         `casr providers` still reported Kiro installed"
    );
    assert_eq!(envelope["items"][0]["messages"], 8);
    assert_eq!(envelope["items"][0]["workspace"], WORKSPACE);
    assert!(
        envelope["skipped"].as_array().unwrap().is_empty(),
        "nothing about this session is unreadable"
    );
}

/// On a default install the two roots coincide, so both stores live under one
/// `~/.kiro/sessions`. Neither may swallow or duplicate the other.
#[test]
fn kiro_lists_both_stores_once_each() {
    let _lock = ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("HOME", tmp.path());
    let _kiro = EnvGuard::remove("KIRO_HOME");
    let root = tmp.path().join(".kiro");

    seed_kiro_ide(&root);
    let cli_id = seed_kiro_cli(&root);

    let mut listed: Vec<String> = Kiro
        .list_sessions()
        .expect("list_sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![cli_id.clone(), KIRO_IDE_ID.to_string()],
        "one row per session, from both stores"
    );

    // `owns_session` reaches into the IDE's buckets, which are named by a
    // one-way hash and so can only be searched, not computed from an id.
    let owned = Kiro
        .owns_session(KIRO_IDE_ID)
        .expect("IDE session is owned");
    assert!(owned.ends_with("session.json"));
    assert!(
        Kiro.owns_session(&cli_id)
            .is_some_and(|p| p.ends_with(format!("{cli_id}.json"))),
        "the CLI session is still owned by its metadata file"
    );
}

/// `KIRO_HOME` moves the CLI store and *only* the CLI store.
///
/// This is not a style preference, it is what the two products do. `kiro-cli`
/// resolves its root with (verbatim from the shipped `kiro-cli-chat` 2.14.2
/// bundle) `let e=process.env.KIRO_HOME; if(e&&e.length>0)return e; …`, and
/// running the real binary with `KIRO_HOME=$X` writes `$X/settings/cli.json`
/// rather than `$HOME/.kiro/settings/cli.json`. The Kiro IDE has no such
/// variable — `KIRO_HOME` occurs zero times in the whole shipped desktop
/// package — so its store stays at `~/.kiro/sessions` no matter what.
///
/// Applying `KIRO_HOME` to both scans would look right on a default install,
/// where the roots coincide, and silently read the wrong directory for every
/// user who sets it. The decoy below is the half that must NOT be read.
#[test]
fn kiro_home_moves_only_the_cli_store() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::set("KIRO_HOME", kiro_home.path());

    // Where each product would really write with these variables set.
    let cli_id = seed_kiro_cli(kiro_home.path());
    seed_kiro_ide(&home.path().join(".kiro"));

    // A decoy IDE session under $KIRO_HOME, where Kiro IDE never writes. If
    // the IDE scan honoured KIRO_HOME it would find this one and miss the real
    // one above.
    let decoy = kiro_home
        .path()
        .join("sessions")
        .join(KIRO_BUCKET)
        .join("sess_dddddddd-dddd-4ddd-8ddd-dddddddddddd");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::copy(
        fixtures_dir().join("kiro/ide_session_global.json"),
        decoy.join("session.json"),
    )
    .unwrap();

    let mut listed: Vec<String> = Kiro
        .list_sessions()
        .expect("list_sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![cli_id, KIRO_IDE_ID.to_string()],
        "the CLI store follows KIRO_HOME and the IDE store does not; the decoy \
         under $KIRO_HOME/sessions must not be read"
    );

    // Both roots are reported, because with KIRO_HOME set they are two
    // different directories and a user is entitled to know which were searched.
    let detected = Kiro.detect();
    assert!(detected.installed);
    let evidence = detected.evidence.join(" | ");
    assert!(
        evidence.contains(&kiro_home.path().display().to_string())
            && evidence.contains(&home.path().join(".kiro").display().to_string()),
        "detection should name both roots, got: {evidence}"
    );
}

// ---------------------------------------------------------------------------
// cursor-agent
// ---------------------------------------------------------------------------

/// Fails before the fix: `list_sessions` only read `state.vscdb`, so a machine
/// with `cursor-agent` and no Cursor IDE listed nothing.
#[test]
fn cursor_agent_sessions_are_listed_and_deduped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let vscdb = tempfile::tempdir().unwrap();
    seed_cursor_agent(tmp.path());

    let envelope = list_json(
        "cursor",
        // Deliberately no `--workspace`: a cursor-agent session records none,
        // and an explicit filter would correctly hide it as unclassifiable.
        &[],
        &[
            ("CURSOR_CONFIG_DIR", tmp.path()),
            ("CURSOR_DATA_DIR", tmp.path()),
            // Point the IDE reader at an empty directory so this test is about
            // the CLI store and nothing else.
            ("CURSOR_HOME", vscdb.path()),
        ],
        store.path(),
    );

    assert_eq!(
        ids(&envelope),
        vec![CURSOR_AGENT_ID],
        "the conversation with a transcript is listed exactly once, even though \
         both `chats/<md5>/<id>/store.db` and `agent-transcripts/<id>/<id>.jsonl` \
         describe it — the conversation id is the de-duplication key"
    );

    let item = &envelope["items"][0];
    assert_eq!(item["messages"], 4);
    // From the transcript's sibling chat store, which is the only place the CLI
    // records a title, a creation time or a model.
    assert_eq!(item["title"], "What does src/main.rs do?");
    assert_eq!(item["started_at"], 1784538000000i64);
    // Neither the `projects/<slug>` slug nor the `chats/<md5>` bucket can be
    // inverted, and no workspace is invented from them.
    assert!(item["workspace"].is_null());
    assert_eq!(item["workspace_name_source"], "none");
}

/// A chat store with no transcript is a real conversation this reader cannot
/// render. It must be reported, not dropped: dropping it is exactly the failure
/// this whole change is about, one directory deeper.
#[test]
fn cursor_agent_chat_without_transcript_is_reported_not_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let vscdb = tempfile::tempdir().unwrap();
    seed_cursor_agent(tmp.path());

    let envelope = list_json(
        "cursor",
        // Deliberately no `--workspace`: a cursor-agent session records none,
        // and an explicit filter would correctly hide it as unclassifiable.
        &[],
        &[
            ("CURSOR_CONFIG_DIR", tmp.path()),
            ("CURSOR_DATA_DIR", tmp.path()),
            ("CURSOR_HOME", vscdb.path()),
        ],
        store.path(),
    );

    let skipped = envelope["skipped"].as_array().unwrap();
    assert_eq!(
        skipped.len(),
        1,
        "exactly the transcript-less chat: {skipped:?}"
    );
    assert_eq!(skipped[0]["provider"], "cursor");
    assert!(
        skipped[0]["path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{CURSOR_ORPHAN_ID}/store.db")),
        "the skipped entry names the chat store: {}",
        skipped[0]["path"]
    );
    let reason = skipped[0]["error"].as_str().unwrap();
    assert!(
        reason.contains(CURSOR_ORPHAN_ID) && reason.contains("protobuf"),
        "the reason has to say what could not be read and why: {reason}"
    );

    // And the plain listing says so on stderr rather than silently coming up
    // one short.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_casr"))
        .args(["list", "--provider", "cursor", "--limit", "50"])
        .env("CURSOR_CONFIG_DIR", tmp.path())
        .env("CURSOR_DATA_DIR", tmp.path())
        .env("CURSOR_HOME", vscdb.path())
        .env("XDG_DATA_HOME", store.path())
        .output()
        .expect("run casr list");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not be read") && stderr.contains("cursor: 1"),
        "stderr must aggregate the skip: {stderr}"
    );
}

/// `cursor-agent`'s roots are its own, resolved the way it resolves them —
/// `$CURSOR_CONFIG_DIR` / `$XDG_CONFIG_HOME/cursor` / `~/.cursor` for chats,
/// `$CURSOR_DATA_DIR` / `~/.cursor` for projects. A user who moved either is
/// still read.
#[test]
fn cursor_agent_roots_follow_the_cli_env_vars() {
    let _lock = ENV.lock().unwrap();
    let config = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let _c = EnvGuard::set("CURSOR_CONFIG_DIR", config.path());
    let _d = EnvGuard::set("CURSOR_DATA_DIR", data.path());
    let _h = EnvGuard::set("CURSOR_HOME", config.path().join("no-vscdb-here").as_path());

    seed_cursor_agent(data.path());
    // Only `projects/` was seeded under the data dir, so the chats the seeder
    // also wrote there are invisible: they live under the config dir.
    let listed: Vec<String> = Cursor
        .list_sessions()
        .expect("list_sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        listed,
        vec![CURSOR_AGENT_ID],
        "the transcript is found under $CURSOR_DATA_DIR"
    );

    seed_cursor_agent(config.path());
    let listed: Vec<String> = Cursor
        .list_sessions()
        .expect("list_sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        listed.contains(&CURSOR_ORPHAN_ID.to_string()),
        "chat stores are found under $CURSOR_CONFIG_DIR: {listed:?}"
    );
}

/// A subagent transcript is a real session and stays listed as one, but its
/// path is the only record of whose subagent it was, so the lineage is kept.
#[test]
fn cursor_agent_subagent_transcript_keeps_its_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let child = "1111aaaa-2222-bbbb-3333-cccc4444dddd";
    let dir = tmp
        .path()
        .join("projects")
        .join(CURSOR_PROJECT_SLUG)
        .join("agent-transcripts")
        .join(CURSOR_AGENT_ID)
        .join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{child}.jsonl"));
    std::fs::copy(
        fixtures_dir().join("cursor/cursor_agent_transcript.jsonl"),
        &path,
    )
    .unwrap();

    let session = Cursor
        .read_session(&path)
        .expect("subagent transcript parses");
    assert_eq!(session.session_id, child);
    assert_eq!(
        session.metadata["cursor_agent_parent_id"], CURSOR_AGENT_ID,
        "the parent id is in the path and must not be dropped"
    );
}

/// Cursor 3.13 stamps every composer with VS Code's `IWorkspaceIdentifier`.
/// Reading it is how an IDE session gets a workspace when no bubble carries
/// one; a bare `{id}` (an empty window) has no path in it and must stay `None`.
#[test]
fn cursor_composer_workspace_identifier_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.vscdb");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();

    // The `uri` value is VS Code's `URI.toJSON()` output, verbatim in shape.
    let with_uri = serde_json::json!({
        "fullConversationHeadersOnly": [{"bubbleId": "b1"}],
        "name": "has a workspace",
        "workspaceIdentifier": {
            "id": "3f2a1c9d4b5e6f708192a3b4c5d6e7f8",
            "uri": {
                "$mid": 1,
                "fsPath": "/home/u/demo-project",
                "external": "file:///home/u/demo-project",
                "path": "/home/u/demo-project",
                "scheme": "file"
            }
        }
    });
    let empty_window = serde_json::json!({
        "fullConversationHeadersOnly": [{"bubbleId": "b2"}],
        "name": "no workspace",
        "workspaceIdentifier": {"id": "0011223344556677889900aabbccddee"}
    });
    for (id, composer) in [("c-uri", &with_uri), ("c-empty", &empty_window)] {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                format!("composerData:{id}"),
                serde_json::to_string(composer).unwrap()
            ],
        )
        .unwrap();
    }
    for bubble in ["b1", "b2"] {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                format!(
                    "bubbleId:c-{}:{bubble}",
                    if bubble == "b1" { "uri" } else { "empty" }
                ),
                r#"{"type":1,"text":"hello"}"#
            ],
        )
        .unwrap();
    }
    drop(conn);

    let session = Cursor.read_session(&db.join("c-uri")).unwrap();
    assert_eq!(
        session.workspace.as_deref(),
        Some(Path::new(WORKSPACE)),
        "composerData.workspaceIdentifier.uri.fsPath is the workspace"
    );

    let session = Cursor.read_session(&db.join("c-empty")).unwrap();
    assert!(
        session.workspace.is_none(),
        "an empty-window identifier is a hash and nothing else; inventing a \
         workspace from it would be worse than admitting there is none"
    );
}
