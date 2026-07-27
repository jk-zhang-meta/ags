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

/// A `kiro-cli`-written session in the bucketed layout, under `<root>` —
/// which for kiro-cli is `$KIRO_HOME`.
///
/// Byte-shaped after the real thing rather than after the IDE fixture: this is
/// what `kiro-cli-chat` 2.14.2 wrote when asked to make a flat session loadable
/// under its KAS engine, `session.json` field order and all. The `cli_<uuid>_…`
/// id is its own, and is the reason an id prefix cannot be read as a product.
/// Returns the session id.
fn seed_kiro_cli_bucketed(root: &Path) -> String {
    let id = "cli_0a5376f2-7e2f-4981-bcbc-67195586604a_uHORXqEL";
    let dir = root.join("sessions").join(KIRO_BUCKET).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schemaVersion": "1.0.0",
            "id": id,
            "title": "Research the opencode repo",
            "agentMode": "default",
            "workspacePaths": [WORKSPACE],
            "createdAt": "2026-06-07T14:14:27.290365+00:00",
            "lastModifiedAt": "2026-07-27T13:54:01.685124967+00:00",
            "dataModelVersion": 1,
            "createdReason": "human"
        }))
        .unwrap(),
    )
    .unwrap();
    // The converter emits no `source` on the user payload; the IDE does. Both
    // are valid — `source` is optional — and neither identifies the writer.
    let messages = [
        serde_json::json!({
            "id": "93cdb28f-d3b8-4b0a-81a9-b7ff12cb5d5a",
            "timestamp": "2026-06-07T14:14:27+00:00",
            "payload": {"type": "user", "content": "Research the opencode repo."}
        }),
        serde_json::json!({
            "id": "1f0c4a52-6d3b-4a97-9d21-0b7c8e5f3a44",
            "timestamp": "2026-06-07T14:14:36+00:00",
            "payload": {"type": "assistant", "content": "Reading its provider abstraction now.",
                        "operationType": "Say"}
        }),
    ];
    std::fs::write(
        dir.join("messages.jsonl"),
        messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
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
    // Nine, not eight: `session_start` carries the prompt that opened the
    // session and is the only copy of it, so it is a message.
    assert_eq!(envelope["items"][0]["messages"], 9);
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

/// `KIRO_HOME` moves everything `kiro-cli` writes, and nothing the IDE writes.
///
/// This is not a style preference, it is what the two products do. `kiro-cli`
/// resolves its root with (verbatim from the shipped `kiro-cli-chat` 2.14.2
/// bundle) `let e=process.env.KIRO_HOME; if(e&&e.length>0)return e; …`, and
/// running the real binary with `KIRO_HOME=$X` writes `$X/settings/cli.json`
/// rather than `$HOME/.kiro/settings/cli.json`. The Kiro IDE has no such
/// variable — `KIRO_HOME` occurs zero times in the whole shipped desktop
/// package — so its store stays at `~/.kiro/sessions` no matter what.
///
/// What `KIRO_HOME` moves is *both* of kiro-cli's layouts, not just the flat
/// one. An earlier revision of this test seeded a bucketed session under
/// `$KIRO_HOME/sessions` as a "decoy" and asserted it must not be listed, on
/// the theory that bucketed means IDE. It does not. Driving the shipped binary
/// in a sandboxed `HOME`, with no network and no login:
///
/// ```text
/// $ KIRO_HOME=$X kiro-cli-chat chat _ ensure-session --source-format v2 \
///       --source-session-id 0a5376f2-… --target-format kas --cwd /tmp/wsX
/// {"kind":"ensureSession","data":{"sessionId":"cli_0a5376f2-…_uHORXqEL"}}
/// $ ls $X/sessions/c25a05601239adfe/cli_0a5376f2-…_uHORXqEL
/// messages.jsonl  session.json
/// $ printf '/tmp/wsX' | sha256sum | cut -c1-16   →  c25a05601239adfe
/// ```
///
/// The decoy was a real kiro-cli session, and refusing to list it was data
/// loss. So it is seeded here as what it is and asserted to be listed. What
/// still distinguishes the two stores is the *layout* — flat triplet versus
/// bucketed directory — never the root and never the id prefix: kiro-cli mints
/// `sess_` ids natively and `cli_` ids for conversions, and the IDE mints
/// `sess_` too.
#[test]
fn kiro_home_moves_both_of_the_cli_layouts() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::set("KIRO_HOME", kiro_home.path());

    // Where each product would really write with these variables set.
    let cli_id = seed_kiro_cli(kiro_home.path());
    seed_kiro_ide(&home.path().join(".kiro"));
    let cli_kas_id = seed_kiro_cli_bucketed(kiro_home.path());

    let mut listed: Vec<String> = Kiro
        .list_sessions()
        .expect("list_sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    listed.sort();
    let mut want = vec![cli_id, cli_kas_id, KIRO_IDE_ID.to_string()];
    want.sort();
    assert_eq!(
        listed, want,
        "both of kiro-cli's layouts follow KIRO_HOME and the IDE's does not; \
         pinning the bucketed scan to ~/.kiro drops the middle one entirely"
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

/// The gap the listing tests above could not see: a path handed to casr
/// directly is matched against `session_roots()`, and `session_roots()` was
/// `~/.kiro/sessions` alone.
///
/// With `KIRO_HOME` set that root contains none of kiro-cli's sessions, so
/// `casr info $KIRO_HOME/sessions/cli/<id>.json` matched no provider root at
/// all and fell through to the best-effort parser — reporting some other
/// agent's format for a file casr can read perfectly well.
#[test]
fn kiro_session_roots_cover_every_root_a_session_can_live_under() {
    let _lock = ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", home.path());
    let _k = EnvGuard::set("KIRO_HOME", kiro_home.path());

    let cli_id = seed_kiro_cli(kiro_home.path());
    seed_kiro_ide(&home.path().join(".kiro"));

    let roots = Kiro.session_roots();
    for store in [
        kiro_home.path().join("sessions"),
        home.path().join(".kiro").join("sessions"),
    ] {
        assert!(
            roots.contains(&store),
            "{} is a root Kiro sessions live under, but session_roots() is {roots:?}",
            store.display()
        );
    }

    // The property every caller actually uses these roots for.
    let cli_session = kiro_home
        .path()
        .join("sessions")
        .join("cli")
        .join(format!("{cli_id}.json"));
    assert!(
        roots.iter().any(|r| cli_session.starts_with(r)),
        "a CLI session passed by path must fall under a returned root: {}",
        cli_session.display()
    );
}

/// The user-visible end of the same path, through the binary.
///
/// A contract check rather than a regression test for the root fix: with the
/// roots reverted this still passes, because discovery's file-signature
/// inference recognises a real `kiro-cli` metadata file on its own. That
/// fallback is a safety net and not the contract — it is a guess that happens
/// to be right here — so what is pinned is that casr resolves the file without
/// needing it. `kiro_session_roots_cover_every_root_a_session_can_live_under`
/// above is the one that fails on the unfixed source.
#[test]
fn kiro_session_passed_by_path_is_read_as_kiro() {
    let home = tempfile::tempdir().unwrap();
    let kiro_home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let cli_id = "cccc1111-2222-4333-8444-555555555555";
    let cli = kiro_home.path().join("sessions").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(
        cli.join(format!("{cli_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "session_id": cli_id,
            "cwd": WORKSPACE,
            "created_at": "2026-06-07T14:14:27.290365Z",
            "updated_at": "2026-06-07T14:14:36.404077Z",
            "title": "Two turns"
        }))
        .unwrap(),
    )
    .unwrap();
    let journal = [
        serde_json::json!({"version": "v1", "kind": "Prompt", "data": {
            "message_id": "93cdb28f-d3b8-4b0a-81a9-b7ff12cb5d5a",
            "content": [{"kind": "text", "data": "Add a /health endpoint."}]}}),
        serde_json::json!({"version": "v1", "kind": "AssistantMessage", "data": {
            "message_id": "7199a1ef-0b07-4efc-a793-42d5ce193a5a",
            "content": [{"kind": "text", "data": "Added it to server.py."}]}}),
    ];
    std::fs::write(
        cli.join(format!("{cli_id}.jsonl")),
        journal
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let path = cli.join(format!("{cli_id}.json"));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_casr"))
        .args(["info", &path.display().to_string(), "--json"])
        .env("HOME", home.path())
        .env("KIRO_HOME", kiro_home.path())
        .env("XDG_DATA_HOME", store.path())
        .output()
        .expect("run casr info");
    assert!(
        output.status.success(),
        "casr info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("info --json emits an envelope");
    assert_eq!(
        envelope["provider"], "kiro",
        "the file is under $KIRO_HOME/sessions/cli, which is a Kiro root; \
         picking a parser by guessing is what happens when no root matches"
    );
    assert_eq!(
        envelope["messages"], 2,
        "both turns, which is what reading it as Kiro gets you"
    );

    // And it got there by matching a root, not by winning a bake-off. When no
    // root matches, discovery probes every registered provider and keeps the
    // most plausible parse — which is how a Kiro session comes back as some
    // other agent's. The probe is silent under `--json`, so it is observed
    // without it: the losing providers complain on stderr as they are asked.
    let plain = std::process::Command::new(env!("CARGO_BIN_EXE_casr"))
        .args(["info", &path.display().to_string()])
        .env("HOME", home.path())
        .env("KIRO_HOME", kiro_home.path())
        .env("XDG_DATA_HOME", store.path())
        .output()
        .expect("run casr info");
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        !stderr.contains("pi-agent") && !stderr.contains("clawdbot"),
        "other providers were asked to parse a file that sits under a Kiro \
         root, which means no root matched it: {stderr}"
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
