//! What Codex 0.145.0 will offer, and what casr's listing may therefore claim.
//!
//! Every expectation here was measured against the shipped `@openai/codex`
//! 0.145.0 binary, by driving its own `codex app-server` over stdio with
//! `thread/list` while real rollouts sat at chosen paths under a throwaway
//! `CODEX_HOME`. `src/providers/codex.rs` records the full matrix; these tests
//! hold casr to it from the outside.
//!
//! The line each test is drawing is the same one: a listing is an answer to
//! "what is there", and it is wrong in two directions. Claiming a file Codex
//! will not offer invents a session; dropping one it does have loses one. The
//! third case — a file that is genuinely there and genuinely unreadable — is
//! neither, and has to be *reported*, which is why the compressed rollout is
//! not simply excluded.

mod test_env;

use std::path::{Path, PathBuf};

use casr::providers::Provider;
use casr::providers::codex::Codex;

static CODEX_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: the test holds `CODEX_ENV` (see `test_env`) for the whole
        // time the environment is mutated and read.
        unsafe { std::env::set_var(key, value) };
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

/// A real zstd frame, so the compressed-rollout case is not tested against a
/// file that merely has the name.
///
/// Produced with `zstd -19` (CLI v1.5.7) from the two JSONL lines a Codex
/// rollout starts with — a `session_meta` for `zst-0001` with `"source":
/// "cli"`, and one `event_msg`/`user_message`. 0.145.0 leaves exactly this
/// where the `.jsonl` was: `rollout/src/compression.rs` renames in place, and
/// `thread/list` goes on listing the thread.
const COMPRESSED_ROLLOUT: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x64, 0x0e, 0x00, 0xf5, 0x04, 0x00, 0x82, 0x4b, 0x22, 0x19, 0x60, 0x77,
    0x0e, 0x4c, 0x1b, 0x8d, 0x36, 0x28, 0xfb, 0xa3, 0xd4, 0x13, 0x48, 0x00, 0x70, 0x18, 0x5a, 0x94,
    0xeb, 0x64, 0x12, 0x41, 0x40, 0x2e, 0x07, 0xc0, 0xb6, 0xbd, 0x56, 0x53, 0x92, 0x26, 0x15, 0x44,
    0xee, 0x66, 0x8a, 0xca, 0xe9, 0xfc, 0xdd, 0x9d, 0x1f, 0xd8, 0xcd, 0xb2, 0xa1, 0x58, 0x2d, 0x9e,
    0x96, 0x4d, 0x9d, 0xd8, 0x62, 0x60, 0xdb, 0x4e, 0x71, 0x57, 0x4d, 0x84, 0x4c, 0x41, 0xf5, 0xee,
    0x91, 0xe4, 0xfc, 0x9d, 0x1d, 0xaa, 0x9a, 0x7d, 0xf7, 0xbe, 0x08, 0x2a, 0x2b, 0xf1, 0xf7, 0x78,
    0x39, 0x77, 0x2f, 0x76, 0x57, 0xa8, 0x9d, 0xfd, 0x3d, 0x46, 0xd7, 0xde, 0x23, 0x9c, 0x12, 0x06,
    0xe9, 0xdd, 0x99, 0xdd, 0x1d, 0x76, 0xdf, 0xee, 0x3b, 0x76, 0x51, 0x0c, 0x21, 0x59, 0x01, 0x31,
    0xd6, 0xf8, 0x3b, 0x95, 0xb0, 0x36, 0xcb, 0xa8, 0xde, 0x1d, 0x56, 0x96, 0xcd, 0x34, 0xa3, 0x6e,
    0xee, 0xbf, 0x33, 0xc5, 0xea, 0xb5, 0x06, 0x00, 0x2d, 0xe2, 0xb3, 0x40, 0x0c, 0x24, 0x37, 0x62,
    0xe3, 0x50, 0x8c, 0xe6, 0x80, 0x2c, 0xa6, 0x14, 0x04, 0x60, 0xef, 0x6c,
];

/// A minimal modern rollout whose `session_meta.payload.source` is `source`.
///
/// The shapes are the artifact's: `"cli"` and `"exec"` are the unit variants of
/// `SessionSource`, and the subagent form is copied from a genuine rollout in
/// the corpus this was measured against.
fn rollout(id: &str, source: serde_json::Value, thread_source: &str) -> String {
    let lines = [
        serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-07-28T03:00:00.000Z",
            "payload": {
                "id": id,
                "session_id": id,
                "cwd": "/tmp/ws",
                "originator": "codex-tui",
                "cli_version": "0.145.0",
                "source": source,
                "thread_source": thread_source,
                "model_provider": "openai",
            }
        }),
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-07-28T03:00:01.000Z",
            "payload": {"type": "user_message", "message": format!("hello from {id}")}
        }),
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-07-28T03:00:02.000Z",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ack"}]
            }
        }),
    ];
    lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn subagent_source() -> serde_json::Value {
    serde_json::json!({
        "subagent": {
            "thread_spawn": {
                "parent_thread_id": "019f0000-0000-7000-8000-000000000001",
                "depth": 1,
                "agent_path": "/tmp/agent",
                "agent_nickname": "Gibbs",
                "agent_role": serde_json::Value::Null
            }
        }
    })
}

fn plant(home: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(path.parent().expect("rollout has a parent")).expect("mkdir");
    std::fs::write(&path, bytes).expect("write rollout");
    path
}

/// Listed paths, relative to the Codex home, sorted.
fn listed(home: &Path) -> Vec<String> {
    let listing = Codex.list_sessions().expect("Codex enumerates itself");
    assert!(
        listing.unreadable.is_empty(),
        "nothing in this fixture is unreadable, but the listing reported: {:?}",
        listing.unreadable
    );
    let mut rels: Vec<String> = listing
        .sessions
        .iter()
        .map(|(_, path)| {
            path.strip_prefix(home)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    rels.sort();
    rels
}

// ---------------------------------------------------------------------------
// #66 — the listing must be the set Codex's own picker would offer
// ---------------------------------------------------------------------------

/// One home holding every case the 0.145.0 probe distinguished.
///
/// The assertion is on the whole set rather than on membership one file at a
/// time, because both failure directions matter and only an exact set catches
/// the second one.
#[test]
fn listing_is_exactly_what_codex_would_offer() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let home = tmp.path();
    let _env = EnvGuard::set("CODEX_HOME", home);

    // Offered by Codex, so listed.
    plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-cli.jsonl",
        rollout("cli", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-zst.jsonl.zst",
        COMPRESSED_ROLLOUT,
    );
    // The three components are integers, not a date: `thread/list` returns a
    // rollout under `2026/255/18` and casr must not be stricter than the tool.
    plant(
        home,
        "sessions/2026/255/18/rollout-2026-07-28T03-00-00-oddday.jsonl",
        rollout("oddday", serde_json::json!("cli"), "user").as_bytes(),
    );

    // Present, and not offered by Codex.
    plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-sub.jsonl",
        rollout("sub", subagent_source(), "subagent").as_bytes(),
    );
    plant(
        home,
        "archived_sessions/2026/07/28/rollout-2026-07-28T03-00-00-arch.jsonl",
        rollout("arch", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-legacy.json",
        br#"{"session":{"id":"legacy","cwd":"/tmp/ws"},"items":[{"role":"user","content":"hi"}]}"#,
    );
    plant(
        home,
        "sessions/2026/07/28/notarollout-2026-07-28T03-00-00-prefix.jsonl",
        rollout("prefix", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/rollout-2026-07-28T03-00-00-d0.jsonl",
        rollout("d0", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/2026/07/rollout-2026-07-28T03-00-00-d2.jsonl",
        rollout("d2", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/2026/07/28/29/rollout-2026-07-28T03-00-00-d4.jsonl",
        rollout("d4", serde_json::json!("cli"), "user").as_bytes(),
    );
    plant(
        home,
        "sessions/aaaa/bb/cc/rollout-2026-07-28T03-00-00-alpha.jsonl",
        rollout("alpha", serde_json::json!("cli"), "user").as_bytes(),
    );

    assert_eq!(
        listed(home),
        vec![
            "sessions/2026/07/28/rollout-2026-07-28T03-00-00-cli.jsonl".to_string(),
            "sessions/2026/07/28/rollout-2026-07-28T03-00-00-zst.jsonl.zst".to_string(),
            "sessions/2026/255/18/rollout-2026-07-28T03-00-00-oddday.jsonl".to_string(),
        ],
    );
}

/// A compressed rollout is a session the user has. Saying so and failing to
/// read it is the honest answer; leaving it out of the listing is not.
///
/// `cmd_list` turns this `Err` into a `skipped` row carrying the path and the
/// message, which is the only channel that can distinguish "casr cannot read
/// this one" from "there is nothing there".
#[test]
fn compressed_rollout_is_reported_rather_than_silently_dropped() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let home = tmp.path();
    let _env = EnvGuard::set("CODEX_HOME", home);

    let path = plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-zst.jsonl.zst",
        COMPRESSED_ROLLOUT,
    );

    assert_eq!(
        listed(home),
        vec!["sessions/2026/07/28/rollout-2026-07-28T03-00-00-zst.jsonl.zst".to_string()],
        "a compressed rollout is a session Codex still offers, so it must be listed"
    );

    for (label, error) in [
        ("read_session", Codex.read_session(&path).err()),
        ("read_session_ir", Codex.read_session_ir(&path).err()),
    ] {
        let error = error.unwrap_or_else(|| {
            panic!("{label} claimed to have decoded a zstd frame casr cannot decompress")
        });
        let text = format!("{error}");
        assert!(
            text.contains("zstd") && text.contains("rollout-2026-07-28T03-00-00-zst.jsonl.zst"),
            "{label} must name the file and say why it could not be read; got: {text}"
        );
    }
}

/// Listing and resolving are different questions.
///
/// Codex withholds an archived thread from a plain `thread/list` and a subagent
/// thread from the default `sourceKinds`, and it still resumes either one when
/// named. A user who asks casr to convert a session by id has named it, so the
/// exclusions above must not reach [`Provider::owns_session`].
#[test]
fn withheld_rollouts_are_still_resolvable_by_id() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let home = tmp.path();
    let _env = EnvGuard::set("CODEX_HOME", home);

    let archived = plant(
        home,
        "archived_sessions/2026/07/28/rollout-2026-07-28T03-00-00-arch.jsonl",
        rollout("arch", serde_json::json!("cli"), "user").as_bytes(),
    );
    let subagent = plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-sub.jsonl",
        rollout("sub", subagent_source(), "subagent").as_bytes(),
    );
    let legacy = plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-legacy.json",
        br#"{"session":{"id":"legacy","cwd":"/tmp/ws"},"items":[{"role":"user","content":"hi"}]}"#,
    );
    let compressed = plant(
        home,
        "sessions/2026/07/28/rollout-2026-07-28T03-00-00-zst.jsonl.zst",
        COMPRESSED_ROLLOUT,
    );

    assert!(
        listed(home).len() == 1,
        "only the compressed rollout is offered by Codex here; got {:?}",
        listed(home)
    );

    for (label, id, expected) in [
        ("archived", "arch", &archived),
        ("subagent", "sub", &subagent),
        ("legacy .json", "legacy", &legacy),
        ("compressed", "zst", &compressed),
    ] {
        assert_eq!(
            Codex.owns_session(id).as_deref(),
            Some(expected.as_path()),
            "a {label} rollout must still resolve when the user names it"
        );
    }

    // The archived root is a session root, so an explicit path to one is
    // attributed to Codex rather than falling through to signature sniffing.
    assert!(
        Codex
            .session_roots()
            .iter()
            .any(|root| archived.starts_with(root)),
        "archived_sessions/ must be one of Codex's session roots; got {:?}",
        Codex.session_roots()
    );

    // Reading one still works — the withholding is about enumeration only.
    let session = Codex
        .read_session(&archived)
        .expect("an archived rollout is an ordinary rollout to the reader");
    assert_eq!(session.session_id, "arch");
}

// ---------------------------------------------------------------------------
// #66 — the same rule, against the real corpus
// ---------------------------------------------------------------------------

/// The listing withholds every subagent rollout in a real Codex store, and
/// withholds nothing else.
///
/// Fixtures prove the rule fires on the shape someone wrote down. Only the
/// corpus proves it fires on the shape Codex actually emits, and the margin is
/// not small: on the store this was measured against, 576 of 660 rollouts are
/// subagent threads. A regression here does not lose a corner case, it turns
/// `casr list` for Codex into 87% noise.
///
/// The cross-check is the point. `Codex::list_sessions` keys on
/// `session_meta.payload.source`, the field `thread/list` derives its
/// `ThreadSourceKind` from; this test classifies independently on
/// `payload.thread_source`, the separate `user`/`subagent` field Codex writes
/// beside it. They agreed on all 660 files. If a future Codex changes one
/// shape and not the other, the totals stop adding up here rather than in a
/// user's listing.
///
/// The corpus is only ever read.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_corpus_listing_withholds_every_subagent_rollout() {
    let Ok(corpus) = std::env::var("AGSX_CODEX_CORPUS") else {
        eprintln!("AGSX_CODEX_CORPUS unset; skipping");
        return;
    };
    let corpus = PathBuf::from(corpus);
    if corpus.file_name().and_then(|n| n.to_str()) != Some("sessions") {
        eprintln!("AGSX_CODEX_CORPUS must name <CODEX_HOME>/sessions; skipping");
        return;
    }
    let Some(home) = corpus.parent().map(Path::to_path_buf) else {
        eprintln!("AGSX_CODEX_CORPUS has no parent to use as CODEX_HOME; skipping");
        return;
    };

    let _lock = CODEX_ENV.lock().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", &home);

    // Every rollout in the store, split on the field the implementation does
    // *not* consult.
    let mut subagent: Vec<PathBuf> = Vec::new();
    let mut offered: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(&corpus).into_iter().flatten() {
        let path = entry.path();
        if !entry.file_type().is_file() || !Codex.is_session_path(path) {
            continue;
        }
        let meta = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| {
                text.lines()
                    .take(64)
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .find(|value| {
                        value.get("type").and_then(|t| t.as_str()) == Some("session_meta")
                    })
            })
            .unwrap_or(serde_json::Value::Null);
        let thread_source = meta
            .pointer("/payload/thread_source")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if thread_source == "subagent" {
            subagent.push(path.to_path_buf());
        } else {
            offered.push(path.to_path_buf());
        }
    }

    if subagent.is_empty() {
        eprintln!(
            "the corpus at {} holds no subagent rollouts, so this test would \
             prove nothing about the rule it exists to cover; skipping",
            corpus.display()
        );
        return;
    }

    let listing = Codex.list_sessions().expect("Codex enumerates itself");
    let listed: std::collections::BTreeSet<PathBuf> =
        listing.sessions.iter().map(|(_, p)| p.clone()).collect();

    println!(
        "codex corpus: {} rollouts, {} subagent, {} offered, {} listed, {} unreadable",
        subagent.len() + offered.len(),
        subagent.len(),
        offered.len(),
        listed.len(),
        listing.unreadable.len(),
    );

    let leaked: Vec<&PathBuf> = subagent.iter().filter(|p| listed.contains(*p)).collect();
    assert!(
        leaked.is_empty(),
        "{} subagent rollouts are in the listing; `codex resume` offers none of \
         them. First few: {:?}",
        leaked.len(),
        leaked.iter().take(3).collect::<Vec<_>>(),
    );

    let dropped: Vec<&PathBuf> = offered.iter().filter(|p| !listed.contains(*p)).collect();
    assert!(
        dropped.is_empty(),
        "{} rollouts Codex would offer are missing from the listing — the \
         exclusion is over-reaching. First few: {:?}",
        dropped.len(),
        dropped.iter().take(3).collect::<Vec<_>>(),
    );
}

/// The path rule is three integer levels under a resolved root — not a glob,
/// and not a date.
///
/// Each row below was planted under 0.145.0 and its verdict read back from
/// `thread/list`. A glob would admit `aaaa/bb/cc`, which Codex rejects; a
/// calendar check would reject `0000/00/00` and `2026/255/18`, which Codex
/// accepts. The `u16`/`u8`/`u8` boundaries are the observation that
/// distinguishes them, so they are asserted rather than the widths trusted.
#[test]
fn rollout_layout_is_three_integer_levels_under_a_named_root() {
    let _lock = CODEX_ENV.lock().unwrap();
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let base = tmp.path();
    let _env = EnvGuard::set("CODEX_HOME", base);
    let name = "rollout-2026-07-28T03-00-00-abc.jsonl";

    for dirs in [
        "sessions/2026/07/28",
        "sessions/2026/7/8",
        "sessions/+026/07/28",
        "sessions/0000/00/00",
        "sessions/2026/255/18",
        "sessions/2026/07/255",
        "sessions/65535/07/28",
        "archived_sessions/2026/07/28",
    ] {
        assert!(
            Codex.is_session_path(&base.join(dirs).join(name)),
            "0.145.0 lists a rollout under {dirs}, so casr must claim it"
        );
    }

    for dirs in [
        // Wrong depth: zero, one, two, four and five levels all failed to list.
        "sessions",
        "sessions/2026",
        "sessions/2026/07",
        "sessions/2026/07/28/29",
        "sessions/2026/07/28/29/30",
        // Three levels, but not integers.
        "sessions/aaaa/bb/cc",
        "sessions/a026/07/28",
        "sessions/2026/07/1a",
        "sessions/2026/0_7/28",
        "sessions/2026/07/28.0",
        "sessions/-1/07/28",
        // Three integers, past the widths the artifact parses.
        "sessions/65536/07/28",
        "sessions/70000/07/28",
        "sessions/2026/256/18",
        "sessions/2026/07/256",
        "sessions/999999999999/07/28",
        // Right shape, wrong root. The second is four levels under `sessions/`
        // wearing the archived root's name, which lists as nothing at all.
        "notsessions/2026/07/28",
        "sessions/archived_sessions/2026/07/28",
    ] {
        assert!(
            !Codex.is_session_path(&base.join(dirs).join(name)),
            "0.145.0 does not list a rollout under {dirs}, so casr must not invent one"
        );
    }

    // Right shape, but not under this `CODEX_HOME` at all — a copy someone
    // made, not a session Codex would resume.
    assert!(
        !Codex.is_session_path(
            &Path::new("/somewhere/else")
                .join("sessions/2026/07/28")
                .join(name)
        ),
        "a rollout outside CODEX_HOME is not one of this install's sessions"
    );

    // Extensions, at a directory that is otherwise correct.
    let day = base.join("sessions/2026/07/28");
    for file in [
        "rollout-2026-07-28T03-00-00-abc.jsonl",
        "rollout-2026-07-28T03-00-00-abc.jsonl.zst",
    ] {
        assert!(
            Codex.is_session_path(&day.join(file)),
            "{file} is a rollout extension in 0.145.0"
        );
    }
    for file in [
        "rollout-2026-07-28T03-00-00-abc.json",
        "rollout-2026-07-28T03-00-00-abc.jsonl.ZST",
        "rollout-2026-07-28T03-00-00-abc.zst",
        "notarollout-2026-07-28T03-00-00-abc.jsonl",
        "history.jsonl",
    ] {
        assert!(
            !Codex.is_session_path(&day.join(file)),
            "{file} is not a rollout in 0.145.0, so listing it would invent a session"
        );
    }
}
