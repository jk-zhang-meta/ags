//! Pins how far into `~/.factory/sessions` a Factory session can live.
//!
//! `droid@0.180.0` enumerates transcripts from exactly two directory levels and
//! never recurses. Its scan set is `[this.sessionsDir,
//! ...this.state.projectDirectories]`, the project directories come from
//!
//! ```js
//! getAllProjectDirectories() {
//!     return readdirSync(this.sessionsDir, { withFileTypes: true })
//!         .filter($ => $.isDirectory() && $.name.startsWith("-"))
//!         .map($ => join(this.sessionsDir, $.name));
//! }
//! ```
//!
//! and the scan over that set skips anything that is not a file
//! (`if (!E.isFile()) continue;`) before testing `E.name.endsWith(".jsonl")`.
//!
//! casr's listing walk is recursive (`main.rs`, `max_depth(4)`), so without a
//! depth rule in `Provider::is_session_path` a transcript a user *attached* to
//! a session (`sessions/<slug>/attachments/*.jsonl`) — or any `.jsonl` under a
//! directory droid never opens — is rendered as a session.
//!
//! These live here rather than in an in-crate `#[cfg(test)]` module because
//! `src/lib.rs` declares `#![forbid(unsafe_code)]` and `std::env::set_var` is
//! `unsafe` in edition 2024. Each test holds the shared `EnvLock` (see
//! `tests/test_env.rs`) for as long as it mutates the environment *and* for as
//! long as it calls provider code that reads it.

mod test_env;

use std::path::{Path, PathBuf};

use ags::providers::{Provider, factory::Factory};

static FACTORY_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that overrides one env var and restores the original on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let guard = Self::capture(key);
        // SAFETY: callers hold the `FACTORY_ENV` lock for the whole test, so no
        // other thread reads or mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn unset(key: &'static str) -> Self {
        let guard = Self::capture(key);
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
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

/// Write a minimal but genuinely parseable Factory transcript.
fn write_session(path: &Path, id: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = format!(
        "{}\n{}\n",
        format_args!(
            r#"{{"type":"session_start","id":"{id}","title":"{id}","owner":"dev","cwd":"/tmp/proj"}}"#
        ),
        r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#
    );
    std::fs::write(path, body).unwrap();
}

/// Lay out one `.factory` home covering every level of the real store plus the
/// non-sessions that share it.
fn seed_factory_home(home: &Path) -> PathBuf {
    let sessions = home.join(".factory").join("sessions");

    // The two levels droid actually scans.
    write_session(&sessions.join("global-scope.jsonl"), "global-scope");
    write_session(&sessions.join("-tmp-proj").join("project.jsonl"), "project");

    // A "by the way" side-chat fork. droid resolves it by id but keeps it out
    // of every listing.
    write_session(&sessions.join("btw").join("btw-fork.jsonl"), "btw-fork");

    // Not sessions: a transcript attached to a session, and a `.jsonl` in a
    // directory whose name does not start with `-`, which droid never opens.
    write_session(
        &sessions
            .join("-tmp-proj")
            .join("attachments")
            .join("att.jsonl"),
        "attached",
    );
    write_session(&sessions.join("notaslug").join("stray.jsonl"), "stray");

    // Sidecars that share the tree, excluded on extension.
    std::fs::write(sessions.join(".favorites"), r#"["project"]"#).unwrap();
    std::fs::write(
        sessions.join("-tmp-proj").join("project.settings.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(sessions.join("-tmp-proj").join("local-signals.json"), "{}").unwrap();

    // Outside `sessions/` entirely: droid's own siblings under `~/.factory`.
    // The session root is `~/.factory/sessions`, so none of these are ever
    // walked — this asserts that stays true.
    for sibling in [
        "missions",
        "logs",
        "snapshots",
        "subagent-outputs",
        "temp",
        "artifacts",
        "telemetry",
        "automations",
        "crons",
        "droids",
        "skills",
        "commands",
        "docs",
        "hooks",
        "updates",
    ] {
        write_session(
            &home.join(".factory").join(sibling).join("sibling.jsonl"),
            "sibling",
        );
    }
    std::fs::create_dir_all(home.join(".factory").join("cache")).unwrap();
    std::fs::write(
        home.join(".factory")
            .join("cache")
            .join("session-discovery-index.json"),
        r#"{"version":1}"#,
    )
    .unwrap();

    sessions
}

/// Every `.jsonl` under the session root, classified by `is_session_path`, as
/// paths relative to that root so the assertions read like the store layout.
fn classify(sessions: &Path) -> (Vec<String>, Vec<String>) {
    let (mut accepted, mut rejected) = (Vec::new(), Vec::new());
    for entry in walkdir::WalkDir::new(sessions)
        .max_depth(4)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(sessions)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if Factory.is_session_path(entry.path()) {
            accepted.push(relative);
        } else {
            rejected.push(relative);
        }
    }
    accepted.sort();
    rejected.sort();
    (accepted, rejected)
}

#[test]
fn lists_only_the_two_levels_droid_scans() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("FACTORY_HOME");
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", tmp.path());

    let sessions = seed_factory_home(tmp.path());
    let (accepted, rejected) = classify(&sessions);

    assert_eq!(
        accepted,
        vec!["-tmp-proj/project.jsonl", "global-scope.jsonl"],
        "only a transcript directly in `sessions/` or directly in a `-`-prefixed \
         project directory is a session; got {accepted:?} (rejected {rejected:?})"
    );
}

#[test]
fn an_attached_transcript_is_not_a_session() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("FACTORY_HOME");
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", tmp.path());

    let sessions = seed_factory_home(tmp.path());

    // droid's scan skips every directory entry inside a project directory
    // (`if (!E.isFile()) continue;`), so `attachments/` is never opened.
    assert!(
        !Factory.is_session_path(
            &sessions
                .join("-tmp-proj")
                .join("attachments")
                .join("att.jsonl")
        ),
        "a transcript attached to a session must not be listed as a session"
    );

    // `getAllProjectDirectories()` keeps only names starting with `-`.
    assert!(
        !Factory.is_session_path(&sessions.join("notaslug").join("stray.jsonl")),
        "droid never opens a session subdirectory whose name does not start with `-`"
    );
}

#[test]
fn a_btw_fork_is_not_listed_but_stays_resolvable_by_id() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("FACTORY_HOME");
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", tmp.path());

    let sessions = seed_factory_home(tmp.path());
    let fork = sessions.join("btw").join("btw-fork.jsonl");

    // droid drops btw forks from its listing twice over:
    // `if (!B || B.isBtwFork) return []` and `.filter(E => !E.isBtwFork)`.
    assert!(
        !Factory.is_session_path(&fork),
        "a btw fork must not be listed"
    );

    // But `findSessionFile` probes `join(getBtwSessionsDirectory(),
    // `${id}.jsonl`)`, so resolving one by id must keep working. That path does
    // not consult `is_session_path`.
    assert_eq!(
        Factory.owns_session("btw-fork").as_deref(),
        Some(fork.as_path()),
        "a btw fork must still resolve by id"
    );
}

/// `is_session_path` now resolves the session root itself, so it has to follow
/// the root wherever the override precedence puts it. `FACTORY_HOME` is casr's
/// own override and names the *sessions directory* directly, with no
/// `.factory/sessions` joined onto it.
#[test]
fn the_depth_rule_follows_the_casr_owned_override() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("elsewhere");
    let _home = EnvGuard::set("FACTORY_HOME", &sessions);
    let _override = EnvGuard::unset("FACTORY_HOME_OVERRIDE");

    write_session(&sessions.join("global-scope.jsonl"), "global-scope");
    write_session(&sessions.join("-tmp-proj").join("project.jsonl"), "project");
    write_session(
        &sessions
            .join("-tmp-proj")
            .join("attachments")
            .join("att.jsonl"),
        "attached",
    );

    assert_eq!(
        Factory.session_roots(),
        vec![sessions.clone()],
        "`FACTORY_HOME` names the sessions directory itself"
    );
    let (accepted, rejected) = classify(&sessions);
    assert_eq!(
        accepted,
        vec!["-tmp-proj/project.jsonl", "global-scope.jsonl"],
        "the two-level rule must be measured against whichever root won the \
         override precedence; got {accepted:?} (rejected {rejected:?})"
    );
}

#[test]
fn the_session_root_excludes_the_rest_of_dot_factory() {
    let _lock = FACTORY_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("FACTORY_HOME");
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", tmp.path());

    seed_factory_home(tmp.path());

    // `droid` builds its store as `join(getFactoryHome(), ".factory",
    // "sessions")`, so `missions/`, `logs/`, `snapshots/`,
    // `subagent-outputs/`, `cache/` and the rest of `~/.factory` are outside
    // the walk entirely rather than being filtered out of it.
    assert_eq!(
        Factory.session_roots(),
        vec![tmp.path().join(".factory").join("sessions")],
        "the session root is `~/.factory/sessions`, not `~/.factory`"
    );

    // Belt and braces: even if a walk did reach one, it is not a session.
    assert!(
        !Factory.is_session_path(
            &tmp.path()
                .join(".factory")
                .join("missions")
                .join("sibling.jsonl")
        ),
        "a `.jsonl` outside `sessions/` is not a session"
    );
}
