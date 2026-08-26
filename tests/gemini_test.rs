//! Integration tests for Gemini CLI session *discovery*.
//!
//! Reading is covered in-crate and by the fixture corpus; what needs a real
//! directory is the thing the fixtures cannot express — a `chats/` tree holding
//! both formats at once, which is what every long-lived Gemini install looks
//! like. Resuming a legacy `.json` writes a `.jsonl` beside it and leaves the
//! original in place, so "old format" and "new format" is not a choice a user
//! ever made and not one this reader may make either.
//!
//! These live here rather than in the in-crate `#[cfg(test)]` module because
//! `src/lib.rs` declares `#![forbid(unsafe_code)]` and `std::env::set_var` is
//! `unsafe` in edition 2024 — the shared `EnvGuard`/`EnvLock` harness (see
//! `tests/test_env.rs`) serializes process-global env mutation here, in a
//! separate crate.

mod test_env;

use std::path::{Path, PathBuf};

use ags::providers::{Provider, gemini::Gemini};

static GEMINI_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: callers hold the `GEMINI_ENV` lock for the duration, so no
        // other thread reads or mutates the environment concurrently.
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

const HASH: &str = "9d3a7c1b5e2f48a06c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d";
const LEGACY_ID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const MODERN_ID: &str = "bbbbbbbb-5555-6666-7777-888888888888";
const SUBAGENT_ID: &str = "cccccccc-9999-0000-1111-222222222222";

fn chats_dir(home: &Path) -> PathBuf {
    home.join("tmp").join(HASH).join("chats")
}

/// The legacy whole-file form, as it sits on disk before anyone resumes it.
fn write_legacy(home: &Path, name: &str, session_id: &str, messages: &[&str]) -> PathBuf {
    let chats = chats_dir(home);
    std::fs::create_dir_all(&chats).unwrap();
    let body = serde_json::json!({
        "sessionId": session_id,
        "projectHash": HASH,
        "startTime": "2026-01-01T10:00:00.000Z",
        "lastUpdated": "2026-01-01T10:05:00.000Z",
        "messages": messages.iter().enumerate().map(|(i, text)| serde_json::json!({
            "id": format!("m{i}"),
            "type": if i % 2 == 0 { "user" } else { "gemini" },
            "content": text,
        })).collect::<Vec<_>>(),
    });
    let path = chats.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    path
}

/// The JSONL form: a header record followed by one record per line.
fn write_jsonl(
    home: &Path,
    relative: &str,
    session_id: &str,
    records: &[serde_json::Value],
) -> PathBuf {
    let path = chats_dir(home).join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = serde_json::to_string(&serde_json::json!({
        "sessionId": session_id,
        "projectHash": HASH,
        "startTime": "2026-07-27T10:00:00.000Z",
        "lastUpdated": "2026-07-27T10:00:00.000Z",
    }))
    .unwrap();
    body.push('\n');
    for record in records {
        body.push_str(&serde_json::to_string(record).unwrap());
        body.push('\n');
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn message(id: &str, kind: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "type": kind, "content": content})
}

/// Both formats are listed, and the subagent transcript is not.
///
/// A directory holding one of each used to yield exactly one session — the
/// legacy `.json` — because discovery required `.ends_with(".json")`. That is
/// not "ags reads the old format": it is ags showing whichever sessions
/// nobody has resumed lately, cut off at an arbitrary date.
#[test]
fn list_sessions_finds_both_formats() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", home.path());

    write_legacy(
        home.path(),
        "session-2026-01-01T10-00-aaaaaaaa.json",
        LEGACY_ID,
        &["legacy question", "legacy answer"],
    );
    write_jsonl(
        home.path(),
        "session-2026-07-27T10-00-bbbbbbbb.jsonl",
        MODERN_ID,
        &[message("u1", "user", "modern question")],
    );
    // A subagent transcript: one directory deeper, and named after its session
    // id with no `session-` prefix. Gemini's own `isSupportedSessionFile` does
    // not match it, so it is not a resumable session.
    write_jsonl(
        home.path(),
        &format!("{MODERN_ID}/{SUBAGENT_ID}.jsonl"),
        SUBAGENT_ID,
        &[message("s1", "user", "subagent task")],
    );

    let mut ids: Vec<String> = Gemini
        .list_sessions()
        .expect("gemini lists sessions")
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort();

    assert_eq!(ids, vec![LEGACY_ID.to_string(), MODERN_ID.to_string()]);
}

/// A migrated session is one session, and it is the `.jsonl`.
///
/// `ChatRecordingService.initialize` migrates a resumed `.json` by appending an
/// `l` to the filename and replaying the conversation into the new file. It
/// never deletes the original, so both halves sit in `chats/` under the same
/// `sessionId` — one live, one frozen at the moment of the migration. Listing
/// both shows the session twice; picking the `.json` resumes from a stale copy.
#[test]
fn list_sessions_collapses_a_migrated_pair_onto_the_jsonl() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", home.path());

    write_legacy(
        home.path(),
        "session-2026-01-01T10-00-aaaaaaaa.json",
        LEGACY_ID,
        &["question", "answer"],
    );
    let live = write_jsonl(
        home.path(),
        "session-2026-01-01T10-00-aaaaaaaa.jsonl",
        LEGACY_ID,
        &[
            message("m0", "user", "question"),
            message("m1", "gemini", "answer"),
            message("m2", "user", "asked again after migrating"),
        ],
    );

    let sessions = Gemini
        .list_sessions()
        .expect("gemini lists sessions")
        .sessions;
    assert_eq!(
        sessions.len(),
        1,
        "a migrated session is one session, not two: {sessions:?}"
    );
    assert_eq!(sessions[0].0, LEGACY_ID);
    assert_eq!(sessions[0].1, live);

    // And the same choice on the lookup path, so `resume` does not read the
    // frozen copy while `list` shows the live one.
    assert_eq!(
        Gemini.owns_session(LEGACY_ID).as_deref(),
        Some(live.as_path())
    );

    let session = Gemini.read_session(&live).expect("live session reads");
    assert_eq!(session.messages.len(), 3);
}

/// Directory order does not decide which half of a migrated pair wins.
///
/// The `.jsonl` is preferred because it is the live file, not because it
/// happened to be visited second. Seeding the pair the other way round is the
/// only way to say that.
#[test]
fn owns_session_prefers_the_jsonl_whichever_is_seen_first() {
    let _lock = GEMINI_ENV.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GEMINI_HOME", home.path());

    let live = write_jsonl(
        home.path(),
        "session-2026-01-01T10-00-aaaaaaaa.jsonl",
        LEGACY_ID,
        &[message("m0", "user", "question")],
    );
    write_legacy(
        home.path(),
        "session-2026-01-01T10-00-aaaaaaaa.json",
        LEGACY_ID,
        &["question"],
    );

    assert_eq!(
        Gemini.owns_session(LEGACY_ID).as_deref(),
        Some(live.as_path())
    );
}
