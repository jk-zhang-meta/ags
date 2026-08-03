//! `cursor-agent`'s chat metadata carries a live secret, and `casr` must not
//! repeat it.
//!
//! Every `cursor-agent` conversation is created from one metadata literal
//! (`../agent-kv/dist/index.js` in the shipped 2026.07.23-e383d2b package):
//!
//! ```js
//! const J = e => ({agentId: e ?? crypto.randomUUID(), …, subagentInfo: void 0,
//!                  blobEncryptionKey: Q()});
//! function Q(){ const e = new Uint8Array(32); crypto.getRandomValues(e); return m(e) }
//! ```
//!
//! so `meta['0']` always ends with 32 random bytes, hex. The CLI sends that
//! value to Cursor's backend as the `x-blob-encryption-key` header, which makes
//! it a credential and not a curiosity. `casr info --json` is a command users
//! pipe to a file and paste into issues, so echoing the chat object wholesale
//! published it. The reader wants exactly three fields — `name`, `createdAt`
//! and `lastUsedModel` — and must copy only those.
//!
//! These tests are here rather than in-crate because they need the CLI's own
//! environment variables, and `src/lib.rs` is `#![forbid(unsafe_code)]`.

use std::path::Path;

/// The value planted in `tests/fixtures/cursor/cursor_agent_chat_store.db`. It
/// is shaped exactly like a real one — 64 lowercase hex characters, the last
/// key in the object, which is where `Q()` puts it — but is not a real key.
const PLANTED_KEY: &str = "5eec6e7a11c0ffee5eec6e7a11c0ffee5eec6e7a11c0ffee5eec6e7a11c0ffee";

const AGENT_ID: &str = "7a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
const PROJECT_SLUG: &str = "home-u-demo-project";
const CHATS_BUCKET: &str = "6ec6c2923210e29ebc9bf9e34db81429";

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Both halves of a `cursor-agent` conversation, at the paths the CLI uses.
fn seed(root: &Path) {
    let transcripts = root
        .join("projects")
        .join(PROJECT_SLUG)
        .join("agent-transcripts")
        .join(AGENT_ID);
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::copy(
        fixtures_dir().join("cursor/cursor_agent_transcript.jsonl"),
        transcripts.join(format!("{AGENT_ID}.jsonl")),
    )
    .unwrap();

    let chat = root.join("chats").join(CHATS_BUCKET).join(AGENT_ID);
    std::fs::create_dir_all(&chat).unwrap();
    std::fs::copy(
        fixtures_dir().join("cursor/cursor_agent_chat_store.db"),
        chat.join("store.db"),
    )
    .unwrap();
}

/// Run a `casr` subcommand against a seeded `cursor-agent` home, with the
/// session store pointed somewhere disposable.
fn run(args: &[&str], root: &Path, store: &Path) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_casr"));
    cmd.args(args)
        .env("CURSOR_CONFIG_DIR", root)
        .env("CURSOR_DATA_DIR", root)
        .env("XDG_DATA_HOME", store)
        .env_remove("AGS_STORE");
    let out = cmd.output().expect("failed to run casr");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The fixture must actually carry the secret, or the tests below pass by
/// asserting nothing. This is the guard against someone "cleaning up" the
/// fixture and silently disarming the regression.
#[test]
fn the_fixture_still_carries_a_blob_encryption_key() {
    let db = fixtures_dir().join("cursor/cursor_agent_chat_store.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("fixture store.db must open");
    let hex: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
        .expect("fixture must have a meta['0'] row");
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        json["blobEncryptionKey"], PLANTED_KEY,
        "the fixture must keep a blobEncryptionKey — without it these tests \
         assert nothing, which is exactly why the original disclosure shipped"
    );
}

/// The regression. `casr info --json` reached the whole decoded `meta['0']`
/// object into `metadata.cursor_agent_chat`, secret included.
#[test]
fn info_json_does_not_disclose_the_blob_encryption_key() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed(root.path());

    let stdout = run(&["info", AGENT_ID, "--json"], root.path(), store.path());

    assert!(
        !stdout.contains(PLANTED_KEY),
        "`casr info --json` disclosed the cursor-agent blobEncryptionKey.\n\
         It is sent to Cursor's backend as x-blob-encryption-key, and this \
         command is routinely piped to a file or pasted into an issue.\n\
         --- stdout ---\n{stdout}"
    );
}

/// `list --json` never carried it, and must stay that way.
#[test]
fn list_json_does_not_disclose_the_blob_encryption_key() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed(root.path());

    let stdout = run(
        &["list", "--provider", "cursor", "--limit", "50", "--json"],
        root.path(),
        store.path(),
    );

    assert!(
        !stdout.contains(PLANTED_KEY),
        "`casr list --json` disclosed the cursor-agent blobEncryptionKey.\n\
         --- stdout ---\n{stdout}"
    );
}

/// The passthrough is an allow-list, not a deny-list of today's secret names.
/// A deny-list would have to be edited every time Cursor adds a field; this
/// asserts the shape that does not, by planting a field no reader asks for and
/// requiring it to be dropped too.
#[test]
fn chat_metadata_is_an_allow_list_not_a_blob() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed(root.path());

    let stdout = run(&["info", AGENT_ID, "--json"], root.path(), store.path());
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json must emit valid JSON");
    let chat = &json["metadata"]["cursor_agent_chat"];

    let obj = chat
        .as_object()
        .expect("cursor_agent_chat must still be an object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["createdAt", "lastUsedModel", "name"],
        "cursor_agent_chat must carry only the fields this reader uses; \
         anything else is a vendor blob whose contents casr does not control"
    );

    // The three that are kept must still be the real values, or the fix has
    // quietly cost the feature it was protecting.
    assert_eq!(chat["name"], "What does src/main.rs do?");
    assert_eq!(chat["createdAt"], 1784538000000_i64);
    assert_eq!(chat["lastUsedModel"], "claude-4.5-sonnet-thinking");

    // And the enrichment those fields feed must still work end to end.
    assert_eq!(json["title"], "What does src/main.rs do?");
    assert_eq!(json["model_name"], "claude-4.5-sonnet-thinking");
}
