//! Pins which environment variable relocates each provider's session root, and
//! which one wins when several are set.
//!
//! These live here rather than in an in-crate `#[cfg(test)]` module because
//! `src/lib.rs` declares `#![forbid(unsafe_code)]` and `std::env::set_var` is
//! `unsafe` in edition 2024. Every test below holds the shared `EnvLock` (see
//! `tests/test_env.rs`) for as long as it mutates the environment *and* for as
//! long as it calls provider code that reads it.

mod test_env;

use std::path::{Path, PathBuf};

use casr::providers::{
    Provider, amp::Amp, claude_code::ClaudeCode, clawdbot::ClawdBot, cline::Cline,
    factory::Factory, gemini::Gemini, openclaw::OpenClaw, opencode::OpenCode, pi_agent::PiAgent,
    vibe::Vibe,
};

static PROVIDER_ENV: test_env::EnvLock = test_env::EnvLock;

/// RAII guard that overrides one env var and restores the original on drop.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let guard = Self::capture(key);
        // SAFETY: callers hold the `PROVIDER_ENV` lock for the whole test, so no
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

/// Create `<root>/projects/<key>/<id>.jsonl` holding one readable user turn,
/// mirroring the tree Claude Code writes.
fn seed_claude_home(root: &Path, project_key: &str, session_id: &str) -> PathBuf {
    let dir = root.join("projects").join(project_key);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{session_id}\",\"cwd\":\"/data/projects/demo\",\
             \"timestamp\":\"2026-07-26T10:00:00.000Z\",\
             \"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
        ),
    )
    .unwrap();
    path
}

const SESSION_ID: &str = "019f75d0-1111-7222-8333-a4b5c6d7e8f9";
const PROJECT_KEY: &str = "-data-projects-demo";

#[test]
fn claude_code_follows_claude_config_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("CLAUDE_HOME");
    let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", tmp.path());
    let expected = seed_claude_home(tmp.path(), PROJECT_KEY, SESSION_ID);

    // `CLAUDE_CONFIG_DIR` is what Claude Code itself honours to relocate
    // `~/.claude`, so a user who relocated it that way must still be found.
    assert_eq!(
        ClaudeCode.session_roots(),
        vec![tmp.path().join("projects")]
    );
    assert_eq!(ClaudeCode.owns_session(SESSION_ID), Some(expected));
}

#[test]
fn claude_home_wins_over_claude_config_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_home = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CLAUDE_HOME", casr_home.path());
    let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", agent_home.path());

    let expected = seed_claude_home(casr_home.path(), PROJECT_KEY, SESSION_ID);
    let decoy = seed_claude_home(agent_home.path(), PROJECT_KEY, SESSION_ID);

    // `CLAUDE_HOME` is casr's own override: it aims casr at one tree without
    // disturbing the Claude Code the rest of the shell talks to.
    assert_eq!(
        ClaudeCode.session_roots(),
        vec![casr_home.path().join("projects")]
    );
    let owned = ClaudeCode.owns_session(SESSION_ID).unwrap();
    assert_eq!(owned, expected);
    assert_ne!(owned, decoy);
}

#[test]
fn empty_claude_config_dir_counts_as_unset() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let _home = EnvGuard::unset("CLAUDE_HOME");
    let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", Path::new(""));

    // Claude Code 2.1.220 falls back to `~/.claude` when the variable is set but
    // empty; an empty value must not turn into a relative `projects/` path.
    let home = ClaudeCode::home_dir().expect("a home directory");
    assert_eq!(home, dirs::home_dir().unwrap().join(".claude"));
}

#[test]
fn gemini_joins_dot_gemini_onto_gemini_cli_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("GEMINI_HOME");
    let _cli_home = EnvGuard::set("GEMINI_CLI_HOME", tmp.path());

    // `GEMINI_CLI_HOME` replaces the *home directory*: Gemini CLI's `homedir()`
    // returns it and `getGlobalGeminiDir()` is `join(homedir(), '.gemini')`.
    // Treating it as the `.gemini` directory itself would look one level too high.
    assert_eq!(Gemini::home_dir(), Some(tmp.path().join(".gemini")));
}

#[test]
fn gemini_home_wins_over_gemini_cli_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_home = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GEMINI_HOME", casr_home.path());
    let _cli_home = EnvGuard::set("GEMINI_CLI_HOME", agent_home.path());

    // `GEMINI_HOME` is casr's own override and names the `.gemini` dir directly.
    assert_eq!(Gemini::home_dir(), Some(casr_home.path().to_path_buf()));
}

#[test]
fn clawdbot_joins_sessions_onto_clawdbot_state_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("CLAWDBOT_HOME");
    let _state = EnvGuard::set("CLAWDBOT_STATE_DIR", tmp.path());
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{SESSION_ID}.jsonl")), "{}\n").unwrap();

    // `CLAWDBOT_STATE_DIR` replaces the `~/.clawdbot` state root, so `sessions`
    // is joined onto it — it is not the sessions directory itself.
    assert_eq!(ClawdBot.session_roots(), vec![sessions]);
}

/// Create `<data-dir>/tasks/<id>/api_conversation_history.json`, the shape both
/// of Cline's stores use.
fn seed_cline_task(data_dir: &Path, task_id: &str) -> PathBuf {
    let task_dir = data_dir.join("tasks").join(task_id);
    std::fs::create_dir_all(&task_dir).unwrap();
    let path = task_dir.join("api_conversation_history.json");
    std::fs::write(
        &path,
        r#"[{"role":"user","content":[{"type":"text","text":"hello"}]}]"#,
    )
    .unwrap();
    path
}

#[test]
fn cline_finds_the_sdk_store_via_cline_data_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("CLINE_HOME");
    let _dir = EnvGuard::unset("CLINE_DIR");
    let _data = EnvGuard::set("CLINE_DATA_DIR", tmp.path());
    let expected = seed_cline_task(tmp.path(), "1700000000000");

    // Cline's own `resolveDataDir()` reads `CLINE_DATA_DIR` first. Its SDK/CLI
    // store is a second task tree that casr used to miss entirely.
    assert_eq!(Cline.owns_session("1700000000000"), Some(expected));
}

#[test]
fn cline_joins_data_onto_cline_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("CLINE_HOME");
    let _data = EnvGuard::unset("CLINE_DATA_DIR");
    let _dir = EnvGuard::set("CLINE_DIR", tmp.path());
    // `CLINE_DIR` names the `.cline` root, so `data` is joined onto it.
    let expected = seed_cline_task(&tmp.path().join("data"), "1700000000001");

    assert_eq!(Cline.owns_session("1700000000001"), Some(expected));
}

#[test]
fn cline_data_dir_wins_over_cline_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let data_home = tempfile::tempdir().unwrap();
    let cline_dir = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("CLINE_HOME");
    let _data = EnvGuard::set("CLINE_DATA_DIR", data_home.path());
    let _dir = EnvGuard::set("CLINE_DIR", cline_dir.path());

    let expected = seed_cline_task(data_home.path(), "1700000000002");
    let decoy = seed_cline_task(&cline_dir.path().join("data"), "1700000000002");

    let owned = Cline.owns_session("1700000000002").unwrap();
    assert_eq!(owned, expected);
    assert_ne!(owned, decoy);
}

#[test]
fn cline_home_is_used_alone() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_data = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CLINE_HOME", casr_home.path());
    let _data = EnvGuard::set("CLINE_DATA_DIR", agent_data.path());
    let expected = seed_cline_task(casr_home.path(), "1700000000003");
    seed_cline_task(agent_data.path(), "1700000000003");

    // `CLINE_HOME` is casr's own override and is exclusive, so it stays the
    // single write target rather than merely being first among several roots.
    assert_eq!(Cline.session_roots(), vec![casr_home.path().join("tasks")]);
    assert_eq!(Cline.owns_session("1700000000003"), Some(expected));
}

#[test]
fn factory_joins_dot_factory_sessions_onto_factory_home_override() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("FACTORY_HOME");
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", tmp.path());
    let sessions = tmp.path().join(".factory").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    // `FACTORY_HOME_OVERRIDE` replaces the *home directory*: `droid` builds its
    // sessions path as `join(home, ".factory", "sessions")`.
    assert_eq!(Factory.session_roots(), vec![sessions]);
}

#[test]
fn factory_home_wins_over_factory_home_override() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_home = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("FACTORY_HOME", casr_home.path());
    let _override = EnvGuard::set("FACTORY_HOME_OVERRIDE", agent_home.path());
    std::fs::create_dir_all(agent_home.path().join(".factory").join("sessions")).unwrap();

    // `FACTORY_HOME` is casr's own override and names the sessions dir directly.
    assert_eq!(
        Factory.session_roots(),
        vec![casr_home.path().to_path_buf()]
    );
}

#[test]
fn pi_agent_follows_pi_coding_agent_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("PI_AGENT_HOME");
    let _agent_dir = EnvGuard::set("PI_CODING_AGENT_DIR", tmp.path());
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    // `PI_CODING_AGENT_DIR` names the agent dir, the same thing casr's own
    // `PI_AGENT_HOME` names, and `pi` puts sessions in `<dir>/sessions`.
    assert_eq!(PiAgent.session_roots(), vec![sessions]);
}

#[test]
fn pi_agent_home_wins_over_pi_coding_agent_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("PI_AGENT_HOME", casr_home.path());
    let _agent_dir = EnvGuard::set("PI_CODING_AGENT_DIR", agent_dir.path());
    let expected = casr_home.path().join("sessions");
    std::fs::create_dir_all(&expected).unwrap();
    std::fs::create_dir_all(agent_dir.path().join("sessions")).unwrap();

    assert_eq!(PiAgent.session_roots(), vec![expected]);
}

#[test]
fn opencode_follows_an_absolute_opencode_db() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _path = EnvGuard::unset("OPENCODE_DB_PATH");
    let _home = EnvGuard::unset("OPENCODE_HOME");
    let db = tmp.path().join("opencode-nightly.db");
    std::fs::write(&db, b"SQLite format 3\x00").unwrap();
    let _db_env = EnvGuard::set("OPENCODE_DB", &db);

    // OpenCode uses an absolute `OPENCODE_DB` verbatim.
    assert_eq!(OpenCode.session_roots(), vec![db]);
}

#[test]
fn a_relative_opencode_db_resolves_against_the_xdg_data_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _path = EnvGuard::unset("OPENCODE_DB_PATH");
    let _home = EnvGuard::unset("OPENCODE_HOME");
    let _xdg = EnvGuard::set("XDG_DATA_HOME", tmp.path());
    let data_dir = tmp.path().join("opencode");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db = data_dir.join("opencode-nightly.db");
    std::fs::write(&db, b"SQLite format 3\x00").unwrap();
    let _db_env = EnvGuard::set("OPENCODE_DB", Path::new("opencode-nightly.db"));

    // A bare filename is joined onto OpenCode's data dir, not the cwd.
    assert_eq!(OpenCode.session_roots(), vec![db]);
}

/// Discovery must find the database an ordinary OpenCode install actually
/// writes.
///
/// Verified against the released `opencode-linux-x64` 1.18.6 binary: run with a
/// sandboxed `HOME`, it creates `<XDG_DATA_HOME>/opencode/opencode.db`, and
/// `opencode db path` reports that same location. Discovery previously searched
/// only `~/.opencode`, the cwd's ancestors and a config key — so the one
/// location that is always right was the one place it never looked, and a
/// working install reported as not installed.
#[test]
fn opencode_discovery_finds_the_xdg_data_dir_database() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _path = EnvGuard::unset("OPENCODE_DB_PATH");
    let _home = EnvGuard::unset("OPENCODE_HOME");
    let _db_env = EnvGuard::unset("OPENCODE_DB");
    let _xdg = EnvGuard::set("XDG_DATA_HOME", tmp.path());

    let data_dir = tmp.path().join("opencode");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db = data_dir.join("opencode.db");
    std::fs::write(&db, b"SQLite format 3\x00").unwrap();

    assert!(
        OpenCode.session_roots().contains(&db),
        "discovery must include <XDG_DATA_HOME>/opencode/opencode.db; found {:?}",
        OpenCode.session_roots()
    );
}

#[test]
fn an_in_memory_opencode_db_yields_no_path() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let _path = EnvGuard::unset("OPENCODE_DB_PATH");
    let _home = EnvGuard::unset("OPENCODE_HOME");
    let _db_env = EnvGuard::set("OPENCODE_DB", Path::new(":memory:"));

    // `:memory:` is private to OpenCode's own process; it must not be treated as
    // a relative filename and turned into a bogus `<data-dir>/:memory:` path.
    assert!(
        !OpenCode
            .session_roots()
            .iter()
            .any(|p| p.to_string_lossy().contains(":memory:"))
    );
}

/// Write one readable OpenClaw session into an agent's sessions directory.
fn seed_openclaw_session(state_dir: &Path, agent_id: &str, session_id: &str) -> PathBuf {
    let dir = state_dir.join("agents").join(agent_id).join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"session\",\"id\":\"{session_id}\",\
             \"timestamp\":\"2026-07-27T10:00:00.000Z\",\"cwd\":\"/data/projects/demo\"}}\n\
             {{\"type\":\"message\",\"id\":\"m1\",\
             \"timestamp\":\"2026-07-27T10:00:01.000Z\",\
             \"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn openclaw_joins_dot_openclaw_onto_openclaw_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _state = EnvGuard::unset("OPENCLAW_STATE_DIR");
    let _home = EnvGuard::set("OPENCLAW_HOME", tmp.path());
    let expected = seed_openclaw_session(&tmp.path().join(".openclaw"), "main", SESSION_ID);

    // `OPENCLAW_HOME` overrides the *home* directory, so state is at
    // `$OPENCLAW_HOME/.openclaw` and sessions at `<state>/agents/main/sessions`.
    assert_eq!(OpenClaw.owns_session(SESSION_ID), Some(expected));
}

#[test]
fn openclaw_state_dir_wins_over_openclaw_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _state_env = EnvGuard::set("OPENCLAW_STATE_DIR", state.path());
    let _home_env = EnvGuard::set("OPENCLAW_HOME", home.path());

    let expected = seed_openclaw_session(state.path(), "main", SESSION_ID);
    let decoy = seed_openclaw_session(&home.path().join(".openclaw"), "main", SESSION_ID);

    // OpenClaw treats explicit path variables as outranking `OPENCLAW_HOME`.
    let owned = OpenClaw.owns_session(SESSION_ID).unwrap();
    assert_eq!(owned, expected);
    assert_ne!(owned, decoy);
}

#[test]
fn openclaw_finds_sessions_of_a_non_default_agent() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    let _home = EnvGuard::unset("OPENCLAW_HOME");
    let _state_env = EnvGuard::set("OPENCLAW_STATE_DIR", state.path());
    let expected = seed_openclaw_session(state.path(), "work", SESSION_ID);

    // The agent id is part of the path and `main` is only the default, so a
    // session under `--agent work` must still be found.
    assert_eq!(OpenClaw.owns_session(SESSION_ID), Some(expected));
}

#[test]
fn vibe_joins_logs_session_onto_vibe_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("VIBE_HOME", tmp.path());
    let session_dir = tmp
        .path()
        .join("logs")
        .join("session")
        .join(format!("session_20260727_100000_{}", &SESSION_ID[..8]));
    std::fs::create_dir_all(&session_dir).unwrap();
    let expected = session_dir.join("messages.jsonl");
    std::fs::write(
        &expected,
        "{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":\"2026-07-27T10:00:00Z\"}\n",
    )
    .unwrap();
    std::fs::write(
        session_dir.join("meta.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "session_id": SESSION_ID,
            "start_time": "2026-07-27T10:00:00+00:00",
            "end_time": null,
            "git_commit": null,
            "git_branch": null,
            "environment": {"working_directory": null},
            "username": "casr",
            "total_messages": 1,
        }))
        .unwrap(),
    )
    .unwrap();

    // `VIBE_HOME` names the `~/.vibe` root, not the session-log directory; Vibe
    // puts session logs in `<root>/logs/session`.
    assert_eq!(Vibe.owns_session(SESSION_ID), Some(expected));
}

#[test]
fn amp_follows_xdg_data_home_and_ignores_amp_home() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let install = tempfile::tempdir().unwrap();
    let _xdg_env = EnvGuard::set("XDG_DATA_HOME", xdg.path());
    // A user who set `AMP_HOME` correctly for Amp points it at the *install*
    // tree. casr must not mistake that for the data directory.
    let _amp_home = EnvGuard::set("AMP_HOME", install.path());
    let threads = xdg.path().join("amp").join("threads");
    std::fs::create_dir_all(&threads).unwrap();
    std::fs::create_dir_all(install.path().join("threads")).unwrap();

    assert!(
        Amp.session_roots().contains(&threads),
        "expected {} in {:?}",
        threads.display(),
        Amp.session_roots()
    );
    assert!(
        !Amp.session_roots()
            .contains(&install.path().join("threads")),
        "AMP_HOME must not be read as the data directory"
    );
}

#[test]
fn clawdbot_home_wins_over_clawdbot_state_dir() {
    let _lock = PROVIDER_ENV.lock().unwrap();
    let casr_home = tempfile::tempdir().unwrap();
    let agent_state = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("CLAWDBOT_HOME", casr_home.path());
    let _state = EnvGuard::set("CLAWDBOT_STATE_DIR", agent_state.path());
    std::fs::create_dir_all(agent_state.path().join("sessions")).unwrap();

    // `CLAWDBOT_HOME` is casr's own override and names the sessions dir directly.
    assert_eq!(
        ClawdBot.session_roots(),
        vec![casr_home.path().to_path_buf()]
    );
}
