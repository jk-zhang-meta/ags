//! Vendor-owned objects that reach canonical metadata must be allow-listed.
//!
//! `casr info --json` prints `session.metadata` verbatim (`src/main.rs`), and
//! it is a command users pipe to a file and paste into issues. Any reader that
//! copies a whole vendor object into that bag has made a standing promise to
//! publish whatever the vendor puts there next — which is how the
//! `cursor-agent` `blobEncryptionKey` shipped (see `tests/cursor_secret_test.rs`
//! and the allow-list it forced in `Cursor::cli_chat_metadata`).
//!
//! These tests hold the same line for the remaining wholesale copies. Each one
//! plants a field the reader does not ask for, carrying a realistically-shaped
//! secret, and requires that the field be dropped. A deny-list of today's
//! secret names would pass these tests and still fail the next vendor release;
//! only an allow-list at the decode site passes them for the right reason.
//!
//! They live here rather than in-crate because they drive the built binary
//! with the provider's own home-directory variables.

use std::path::{Path, PathBuf};

/// Shaped like an OpenRouter key, which is what Cline's own secret store calls
/// `openRouterApiKey`. Not a real key.
const PLANTED_CLINE_SECRET: &str =
    "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";

/// A GitHub token embedded in an HTTPS git remote — the ordinary shape of a
/// remote for anyone using a PAT or an Actions checkout. Not a real token.
///
/// This is not a hypothetical field: `git_remotes` is field 22 of grok's
/// `summary.json`, it holds the workspace's remote URLs verbatim, and grok's
/// own secret scrubber does not run on the session-persistence path.
const PLANTED_GROK_SECRET: &str =
    "https://x-access-token:ghp_c0ffee11deadbeefc0ffee11deadbeefc0ff@github.com/acme/widgets.git";

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Run `casr` with a disposable store and the given provider home variables.
///
/// `AGSX_STORE` is removed rather than set: it overrides the `XDG_DATA_HOME`
/// redirect, so a value inherited from the developer's shell would aim these
/// tests at the real session store.
fn run(args: &[&str], envs: &[(&str, &Path)], store: &Path) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_casr"));
    cmd.args(args).env("XDG_DATA_HOME", store);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.env_remove("AGSX_STORE");
    let out = cmd.output().expect("failed to run casr");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Parse `casr … --json` stdout, failing with the raw text when it is not JSON.
fn parse(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("expected JSON on stdout ({e})\n--- stdout ---\n{stdout}"))
}

// ---------------------------------------------------------------------------
// Cline — `taskHistoryItem`
// ---------------------------------------------------------------------------

const CLINE_TASK_ID: &str = "1700001234567";

/// A `$CLINE_HOME` holding one task, whose `taskHistory.json` entry carries
/// every field Cline 4.0.11 writes *plus* one it does not.
///
/// The planted field is `apiConfiguration`: not a fabricated name, but the
/// object Cline really does keep — in a sibling `state/secrets.json` written
/// 0600, never inside a history entry. If a future Cline ever inlines it, or a
/// third-party writer does, this is the shape that would arrive.
fn seed_cline(root: &Path) {
    let task_dir = root.join("tasks").join(CLINE_TASK_ID);
    std::fs::create_dir_all(&task_dir).unwrap();
    std::fs::copy(
        fixtures_dir().join("cline/tasks/1700001234567/api_conversation_history.json"),
        task_dir.join("api_conversation_history.json"),
    )
    .unwrap();

    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let history = serde_json::json!([{
        "id": CLINE_TASK_ID,
        "ts": 1_700_001_234_567_i64,
        "task": "Fix Cline fixture flow",
        "tokensIn": 100,
        "tokensOut": 200,
        "totalCost": 0,
        "cwdOnTaskInitialization": "/data/projects/fixture-cline",
        "modelId": "claude-sonnet-4-5-20250929",
        "apiConfiguration": { "openRouterApiKey": PLANTED_CLINE_SECRET },
    }]);
    std::fs::write(
        state_dir.join("taskHistory.json"),
        serde_json::to_vec_pretty(&history).unwrap(),
    )
    .unwrap();
}

#[test]
fn cline_info_json_drops_an_unlisted_task_history_field() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_cline(home.path());

    let stdout = run(
        &["info", CLINE_TASK_ID, "--json"],
        &[("CLINE_HOME", home.path())],
        store.path(),
    );

    assert!(
        !stdout.contains(PLANTED_CLINE_SECRET),
        "`casr info --json` republished a taskHistory.json field no reader \
         asks for. `Cline::read_task_history_item` must copy only the fields \
         its allow-list names.\n--- stdout ---\n{stdout}"
    );
}

#[test]
fn cline_task_history_item_is_an_allow_list_not_a_blob() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_cline(home.path());

    let stdout = run(
        &["info", CLINE_TASK_ID, "--json"],
        &[("CLINE_HOME", home.path())],
        store.path(),
    );
    let json = parse(&stdout);
    let item = json["metadata"]["taskHistoryItem"]
        .as_object()
        .expect("taskHistoryItem must still be an object");

    let mut keys: Vec<&str> = item.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cwdOnTaskInitialization",
            "id",
            "modelId",
            "task",
            "tokensIn",
            "tokensOut",
            "totalCost",
            "ts",
        ],
        "taskHistoryItem must carry only fields audited against Cline's own \
         HistoryItem type. Widening this set is a decision to republish a \
         vendor field, and belongs in the same review as the allow-list."
    );

    // The enrichment the kept fields feed must still work end to end, or the
    // filter has quietly cost the feature it was protecting.
    assert_eq!(json["title"], "Fix Cline fixture flow");
    assert_eq!(json["model_name"], "claude-sonnet-4-5-20250929");
    assert_eq!(json["workspace"], "/data/projects/fixture-cline");
}

// ---------------------------------------------------------------------------
// Grok Build — `summary`
// ---------------------------------------------------------------------------

const GROK_SESSION_ID: &str = "019f75d0-aaaa-7bbb-8ccc-b0a1b2c3d4e5";
const GROK_ENCODED_CWD: &str = "%2Fdata%2Fprojects%2Fdemo";

/// A `$GROK_HOME` holding the fixture session, with a `summary.json` carrying
/// the fixture's fields plus `git_remotes` — a real grok 0.2.103 field that
/// this reader does not ask for and that grok writes unredacted.
fn seed_grok(root: &Path) {
    let src = fixtures_dir()
        .join("grok/sessions")
        .join(GROK_ENCODED_CWD)
        .join(GROK_SESSION_ID);
    let dst = root
        .join("sessions")
        .join(GROK_ENCODED_CWD)
        .join(GROK_SESSION_ID);
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::copy(src.join("updates.jsonl"), dst.join("updates.jsonl")).unwrap();

    let mut summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(src.join("summary.json")).unwrap()).unwrap();
    summary["git_remotes"] = serde_json::json!([PLANTED_GROK_SECRET]);
    std::fs::write(
        dst.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
}

#[test]
fn grok_info_json_does_not_republish_summary_json() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_grok(home.path());

    let stdout = run(
        &["info", GROK_SESSION_ID, "--json"],
        &[("GROK_HOME", home.path())],
        store.path(),
    );

    assert!(
        !stdout.contains(PLANTED_GROK_SECRET),
        "`casr info --json` republished a summary.json field no reader asks \
         for. The Grok reader must name the keys it lifts out of summary.json, \
         not copy the file.\n--- stdout ---\n{stdout}"
    );
}

/// The whole-file copy is gone, and the named lifts it mooted still work.
///
/// It was justified in the source as "round-trip fidelity", which nothing
/// supported: no code reads `metadata.summary` back, and `Grok::write_session`
/// refuses to write a Grok tree at all — so there is no round trip to be
/// faithful to. See the comment this replaced, in `src/providers/grok.rs`.
#[test]
fn grok_metadata_is_named_keys_not_the_summary_file() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_grok(home.path());

    let stdout = run(
        &["info", GROK_SESSION_ID, "--json"],
        &[("GROK_HOME", home.path())],
        store.path(),
    );
    let json = parse(&stdout);
    let meta = json["metadata"]
        .as_object()
        .expect("metadata must be an object");

    let mut keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent_name",
            "model",
            "native_name",
            "sandbox_profile",
            "sessionId",
            "source",
        ],
        "Grok metadata must be keys this reader chose. `summary` reappearing \
         here is the whole-file copy coming back."
    );

    // Every enrichment summary.json feeds must survive the deletion.
    assert_eq!(json["session_id"], GROK_SESSION_ID);
    assert_eq!(json["native_name"], "Echo hi probe session");
    assert_eq!(json["model_name"], "grok-build");
    assert_eq!(json["workspace"], "/data/projects/demo");
    assert_eq!(meta["agent_name"], "grok-build-plan");
    assert!(json["started_at"].is_i64() && json["ended_at"].is_i64());

    // `sandbox_profile` is allow-listed but absent from this fixture only
    // because its value is the string "off"; guard the name, not the value.
    assert!(
        !meta.contains_key("summary"),
        "metadata.summary is the wholesale copy; it must stay deleted"
    );
}

// ---------------------------------------------------------------------------
// Cursor (desktop IDE) — `modelConfig`
// ---------------------------------------------------------------------------

const CURSOR_COMPOSER_ID: &str = "cur-composer-001";

/// Shaped like the value Cursor really stores at the top level of a
/// `composerData` entry: 32 bytes of `crypto.getRandomValues`, base64. Not a
/// real key.
const PLANTED_CURSOR_SECRET: &str = "T2gd0iJl+bkGnFXcQx9nFqQzXbYFcy4bZ7pQe0lHqUM=";

/// A `$CURSOR_HOME` whose `state.vscdb` is the fixture DB with one row edited:
/// the composer gains the two live encryption keys Cursor 3.13.10 persists at
/// the top level of `composerData:<uuid>`, and its `modelConfig` gains a field
/// Cursor's `Gyr` projection does not produce.
///
/// The top-level keys are not a hypothetical. They are the same credential
/// class as the `cursor-agent` `blobEncryptionKey` that
/// `tests/cursor_secret_test.rs` exists for, in the desktop store this reader
/// opens — which is why nothing from this entry may be copied wholesale.
fn seed_cursor(root: &Path) {
    let global = root.join("User").join("globalStorage");
    std::fs::create_dir_all(&global).unwrap();
    let db = global.join("state.vscdb");
    std::fs::copy(fixtures_dir().join("cursor/state.vscdb"), &db).unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    let key = format!("composerData:{CURSOR_COMPOSER_ID}");
    let raw: String = conn
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .unwrap();
    let mut composer: serde_json::Value = serde_json::from_str(&raw).unwrap();
    composer["blobEncryptionKey"] = serde_json::Value::String(PLANTED_CURSOR_SECRET.into());
    composer["speculativeSummarizationEncryptionKey"] =
        serde_json::Value::String(PLANTED_CURSOR_SECRET.into());
    composer["modelConfig"]["maxMode"] = serde_json::Value::Bool(false);
    composer["modelConfig"]["byokApiKey"] = serde_json::Value::String(PLANTED_CURSOR_SECRET.into());
    conn.execute(
        "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
        rusqlite::params![serde_json::to_string(&composer).unwrap(), key],
    )
    .unwrap();
}

#[test]
fn cursor_info_json_discloses_nothing_from_the_composer_entry() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_cursor(home.path());

    let stdout = run(
        &["info", CURSOR_COMPOSER_ID, "--json"],
        &[("CURSOR_HOME", home.path())],
        store.path(),
    );

    assert!(
        !stdout.contains(PLANTED_CURSOR_SECRET),
        "`casr info --json` republished a value from the composerData entry \
         that no reader asks for. Cursor persists two 32-byte random keys at \
         the top level of that entry, so nothing in it may be copied \
         wholesale.\n--- stdout ---\n{stdout}"
    );
}

#[test]
fn cursor_model_config_is_an_allow_list_not_a_blob() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_cursor(home.path());

    let stdout = run(
        &["info", CURSOR_COMPOSER_ID, "--json"],
        &[("CURSOR_HOME", home.path())],
        store.path(),
    );
    let json = parse(&stdout);
    let cfg = json["metadata"]["modelConfig"]
        .as_object()
        .expect("modelConfig must still be an object");

    let mut keys: Vec<&str> = cfg.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["maxMode", "modelName"],
        "modelConfig must carry only the names Cursor's own `Gyr` projection \
         produces. Widening this set republishes a field nobody has read."
    );
    assert_eq!(cfg["modelName"], "gpt-4");
    assert_eq!(cfg["maxMode"], false);
}

// ---------------------------------------------------------------------------
// Amp — the whole thread file
// ---------------------------------------------------------------------------

const AMP_THREAD_ID: &str = "T-cccccccc-cccc-cccc-cccc-cccccccccccc";

/// The kind of thing that ends up in an Amp transcript the moment the agent
/// runs `printenv` — Amp does no redaction on write. Not a real key.
const PLANTED_AMP_SECRET: &str = "ANTHROPIC_API_KEY=sk-ant-api03-c0ffee11deadbeefc0ffee11deadbeef";

/// Amp's stable per-install UUID, which lives at `env.initial.platform`. Not
/// a credential, but not something to print in output people paste publicly.
const PLANTED_AMP_INSTALL_ID: &str = "b3f0d1e2-4a5b-4c6d-8e9f-0a1b2c3d4e5f";

/// A `$XDG_DATA_HOME/amp/threads/<id>.json` shaped like a real Amp thread:
/// `env.initial` as Amp actually writes it (`trees` + `platform`, no `cwd`),
/// a transcript with a secret in a tool result, and one field this reader has
/// never heard of.
fn seed_amp(root: &Path) {
    let threads = root.join("amp").join("threads");
    std::fs::create_dir_all(&threads).unwrap();
    let thread = serde_json::json!({
        "v": 7,
        "id": AMP_THREAD_ID,
        "created": 1_700_000_000_000_i64,
        "title": "Wire up the deploy script",
        "agentMode": "default",
        "env": {
            "initial": {
                "trees": [{
                    "displayName": "demo",
                    "uri": "file:///data/projects/demo",
                    "repository": {"type": "git", "url": "https://github.com/acme/demo", "ref": "main"},
                }],
                "platform": {
                    "os": "linux",
                    "clientType": "vscode",
                    "installationID": PLANTED_AMP_INSTALL_ID,
                    "deviceFingerprint": "v1:fp_9f8e7d6c5b4a39281706f5e4d3c2b1a0",
                },
            }
        },
        "aFutureAmpField": {"nested": PLANTED_AMP_SECRET},
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "print the environment"}],
             "meta": {"sentAt": 1_700_000_001_000_i64}},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Running it."},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"cmd": "printenv"}},
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": [{"type": "text", "text": PLANTED_AMP_SECRET}]},
             ], "meta": {"sentAt": 1_700_000_002_000_i64}},
        ],
    });
    std::fs::write(
        threads.join(format!("{AMP_THREAD_ID}.json")),
        serde_json::to_vec_pretty(&thread).unwrap(),
    )
    .unwrap();
}

/// Amp's home is `$XDG_DATA_HOME/amp`, which is also where the disposable
/// session store goes — they are different subdirectories, so one temp dir
/// serves as both.
#[test]
fn amp_info_json_does_not_republish_the_thread_file() {
    let home = tempfile::tempdir().unwrap();
    seed_amp(home.path());

    let stdout = run(&["info", AMP_THREAD_ID, "--json"], &[], home.path());

    assert!(
        !stdout.contains(PLANTED_AMP_SECRET),
        "`casr info --json` republished the Amp thread file. The transcript is \
         already the canonical `messages` array, which this command reports as \
         counts plus an opt-in --peek tail; copying the thread wholesale \
         reverses that and prints every tool result verbatim.\n\
         --- stdout ---\n{stdout}"
    );
    assert!(
        !stdout.contains(PLANTED_AMP_INSTALL_ID),
        "`casr info --json` republished Amp's per-install identifier from \
         env.initial.platform.\n--- stdout ---\n{stdout}"
    );
}

#[test]
fn amp_metadata_is_thread_facts_not_the_thread() {
    let home = tempfile::tempdir().unwrap();
    seed_amp(home.path());

    let stdout = run(&["info", AMP_THREAD_ID, "--json"], &[], home.path());
    let json = parse(&stdout);
    let meta = json["metadata"]
        .as_object()
        .expect("metadata must be an object");

    let mut keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["agentMode", "created", "id", "native_name", "title", "v"],
        "Amp metadata must be the thread-level facts the allow-list names. \
         `messages` or `env` appearing here is the whole-file copy coming back."
    );

    // Everything the dropped fields fed must still be derived.
    assert_eq!(json["title"], "Wire up the deploy script");
    assert_eq!(json["native_name"], "Wire up the deploy script");
    assert_eq!(json["workspace"], "/data/projects/demo");
    assert_eq!(json["messages"], 2);
    assert_eq!(json["started_at"], 1_700_000_000_000_i64);

    // The transcript is still reachable on request — it just is not the
    // default, which is the behaviour the wholesale copy had overridden.
    let peeked = run(
        &["info", AMP_THREAD_ID, "--json", "--peek"],
        &[],
        home.path(),
    );
    assert!(
        peeked.contains("print the environment"),
        "--peek must still show the transcript tail\n{peeked}"
    );
}

// ---------------------------------------------------------------------------
// Kiro CLI — `session_state` and `.history`
// ---------------------------------------------------------------------------

const KIRO_SESSION_ID: &str = "0a5376f2-7e2f-4981-bcbc-67195586604a";

/// What a pasted credential looks like once it is a line in `<id>.history`.
///
/// kiro-cli's `addToHistory` records every submitted line — the shipped
/// fixture's own `.history` already contains a plain prompt, not just slash
/// commands — and suppresses only blanks and consecutive duplicates.
const PLANTED_KIRO_HISTORY_SECRET: &str =
    "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY";

/// A value in a `session_state` field that `SessionStateV1` does not declare.
const PLANTED_KIRO_STATE_SECRET: &str = "kiro-oidc-refresh-c0ffee11deadbeefc0ffee11deadbeef";

/// A `$KIRO_HOME` holding the captured fixture triplet, with two additions:
/// a plain credential line appended to `.history`, and a `session_state` key
/// outside the five `SessionStateV1` declares.
fn seed_kiro(root: &Path) {
    let src = fixtures_dir().join("kiro");
    let dst = root.join("sessions").join("cli");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::copy(
        src.join(format!("{KIRO_SESSION_ID}.jsonl")),
        dst.join(format!("{KIRO_SESSION_ID}.jsonl")),
    )
    .unwrap();

    let history = std::fs::read_to_string(src.join(format!("{KIRO_SESSION_ID}.history"))).unwrap();
    std::fs::write(
        dst.join(format!("{KIRO_SESSION_ID}.history")),
        format!("{history}{PLANTED_KIRO_HISTORY_SECRET}\n"),
    )
    .unwrap();

    let mut meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(src.join(format!("{KIRO_SESSION_ID}.json"))).unwrap(),
    )
    .unwrap();
    meta["session_state"]["auth_state"] =
        serde_json::json!({ "refresh_token": PLANTED_KIRO_STATE_SECRET });
    std::fs::write(
        dst.join(format!("{KIRO_SESSION_ID}.json")),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
}

#[test]
fn kiro_info_json_publishes_neither_history_nor_an_unlisted_state_field() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_kiro(home.path());

    let stdout = run(
        &["info", KIRO_SESSION_ID, "--json", "--source", "kr"],
        &[("KIRO_HOME", home.path())],
        store.path(),
    );

    assert!(
        !stdout.contains(PLANTED_KIRO_HISTORY_SECRET),
        "`casr info --json` republished a line from <id>.history. That file is \
         every prompt the user submitted, not a slash-command log, so a pasted \
         key is in it verbatim.\n--- stdout ---\n{stdout}"
    );
    assert!(
        !stdout.contains(PLANTED_KIRO_STATE_SECRET),
        "`casr info --json` republished a session_state field that \
         SessionStateV1 does not declare.\n--- stdout ---\n{stdout}"
    );
}

#[test]
fn kiro_session_state_is_an_allow_list_and_history_is_absent() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_kiro(home.path());

    let stdout = run(
        &["info", KIRO_SESSION_ID, "--json", "--source", "kr"],
        &[("KIRO_HOME", home.path())],
        store.path(),
    );
    let json = parse(&stdout);
    let meta = json["metadata"]
        .as_object()
        .expect("metadata must be an object");

    assert!(
        !meta.contains_key("history"),
        "metadata must not carry `.history` at all: it has no fields to \
         allow-list, only the user's typing"
    );

    let state = meta["session_state"]
        .as_object()
        .expect("session_state must still be an object");
    let mut keys: Vec<&str> = state.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent_name",
            "conversation_metadata",
            "permissions",
            "rts_model_state",
            "version",
        ],
        "session_state must carry only the fields SessionStateV1 declares \
         (`goal` is optional and absent from this capture). Widening this set \
         republishes a field nobody has read."
    );

    // The one thing casr reads back out of session_state must still work.
    assert_eq!(json["model_name"], "claude-opus-4.8");
    assert_eq!(
        json["metadata"]["parent_session_id"],
        "98cb06e6-28da-4ba8-8ebe-be6bf16841c1"
    );
}
