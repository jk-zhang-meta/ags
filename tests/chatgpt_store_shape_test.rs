//! Pins which ChatGPT directories ags reads, which it refuses out loud, and
//! which files inside a readable one count as sessions.
//!
//! Verified against `ChatGPT.app` (`ChatGPT.dmg`, sha256 `49b33cad…`). Its
//! `ChatGPT.framework` binary carries the store's directory-name components as
//! standalone NUL-terminated literals beside
//! `cleanUpLegacyDirectoryIfNeeded(for:accountID:appGroupID:)`:
//! `conversations-v3-` (current, joined to the account id at runtime),
//! `conversations_v2_` and `conversations_v2_cache` (previous generation,
//! underscore-separated). Matching only `conversations-` made a real v2 store
//! invisible — neither read nor reported as refused.
//!
//! These live here rather than in an in-crate `#[cfg(test)]` module because
//! `src/lib.rs` declares `#![forbid(unsafe_code)]` and `std::env::set_var` is
//! `unsafe` in edition 2024. Each test holds the shared `EnvLock` (see
//! `tests/test_env.rs`) for as long as it mutates the environment *and* for as
//! long as it calls provider code that reads it.

mod test_env;

use std::path::Path;

use ags::providers::{Provider, chatgpt::ChatGpt};

static CHATGPT_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that overrides one env var and restores the original on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let guard = Self::capture(key);
        // SAFETY: callers hold the `CHATGPT_ENV` lock for the whole test, so no
        // other thread reads or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn capture(key: &'static str) -> Self {
        Self {
            key,
            original: std::env::var_os(key),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: the same lock covers the restore.
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// A conversation ags's own writer would produce: `<id>.json`, flat.
fn write_conversation(dir: &Path, id: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let body = format!(
        r#"{{"conversation_id":"{id}","title":"t","mapping":{{
            "n1":{{"id":"n1","parent":null,"children":["n2"],
                   "message":{{"author":{{"role":"user"}},"create_time":1767225600.0,
                               "content":{{"content_type":"text","parts":["hi"]}}}}}},
            "n2":{{"id":"n2","parent":"n1","children":[],
                   "message":{{"author":{{"role":"assistant"}},"create_time":1767225601.0,
                               "content":{{"content_type":"text","parts":["hello"]}}}}}}}}}}"#
    );
    std::fs::write(dir.join(format!("{id}.json")), body).unwrap();
}

/// Every store shape the artifact attests to, plus the one ags writes.
fn seed_chatgpt_home(home: &Path) {
    // Readable: the shape ags's own `write_session` produces.
    write_conversation(
        &home.join("conversations-13df5255-83ed-4749-921b-4565e9c12a7d"),
        "13df5255-83ed-4749-921b-4565e9c12a7d",
    );
    // The app's own stores. All encrypted; ags must refuse them by name.
    for encrypted in [
        "conversations-v3-acct_ABC123",
        "conversations_v2_acct_ABC123",
        "conversations_v2_cache",
    ] {
        std::fs::create_dir_all(home.join(encrypted)).unwrap();
    }
    // Not a conversation store at all.
    std::fs::create_dir_all(home.join("Cache")).unwrap();
}

/// `(readable dirs, refused dirs)` by directory name, sorted.
fn listing(home: &Path) -> (Vec<String>, Vec<String>) {
    let listing = ChatGpt.list_sessions().expect("chatgpt enumerates itself");
    let mut readable: Vec<String> = ChatGpt
        .session_roots()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let mut refused: Vec<String> = listing
        .unreadable
        .iter()
        .map(|u| u.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let _ = home;
    readable.sort();
    refused.sort();
    (readable, refused)
}

/// The defect the artifact exposed: the previous generation is underscore
/// separated, so a `conversations-` prefix test never saw it. It was not read
/// and not reported either — `list` said "no sessions" about a full store.
#[test]
fn the_underscore_v2_store_is_refused_out_loud_not_silently_ignored() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CHATGPT_HOME", tmp.path());
    seed_chatgpt_home(tmp.path());

    let (_readable, refused) = listing(tmp.path());

    assert_eq!(
        refused,
        vec![
            "conversations-v3-acct_ABC123",
            "conversations_v2_acct_ABC123",
            "conversations_v2_cache",
        ],
        "every versioned store the artifact attests to must be reported as \
         refused, not skipped in silence"
    );
}

/// The version token is what marks the app's store, and an id is hex, so the
/// plain form ags writes can never be mistaken for one.
#[test]
fn only_the_unversioned_store_ags_writes_is_read() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CHATGPT_HOME", tmp.path());
    seed_chatgpt_home(tmp.path());

    let (readable, _refused) = listing(tmp.path());

    assert_eq!(
        readable,
        vec!["conversations-13df5255-83ed-4749-921b-4565e9c12a7d"],
        "only the unversioned `conversations-<id>` tree is readable"
    );
}

/// The conversation in the readable store still reaches the listing. This is
/// what stops `is_session_path` being "corrected" to the artifact's
/// extension-less naming: that naming belongs to the app's `ObjectLoader`
/// module, whose directories are all refused above, while this tree is the one
/// ags's own `write_session` creates and the ten `*_to_chatgpt` roundtrip
/// tests depend on.
#[test]
fn a_conversation_ags_wrote_is_listed() {
    let _lock = CHATGPT_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CHATGPT_HOME", tmp.path());
    seed_chatgpt_home(tmp.path());

    let listing = ChatGpt.list_sessions().expect("chatgpt enumerates itself");
    let ids: Vec<&str> = listing.sessions.iter().map(|(id, _)| id.as_str()).collect();

    assert_eq!(
        ids,
        vec!["13df5255-83ed-4749-921b-4565e9c12a7d"],
        "a conversation ags wrote must keep appearing in `list`"
    );
    assert!(
        ChatGpt.is_session_path(Path::new("/store/conversations-x/abc.json")),
        "`<id>.json` is the shape ags writes"
    );
}
