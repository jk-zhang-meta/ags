//! `casr list` must tell the truth about what it found.
//!
//! Three failures of that, all of which look identical to the user — a short
//! listing:
//!
//! * a directory that could not be read, counted as zero sessions;
//! * a file that is not a session, rendered as a session with zero messages;
//! * a file that is a known sidecar, reported as a session that could not be
//!   read.
//!
//! Every test here drives the compiled binary and asserts on `list --json`,
//! not on a provider method. That is deliberate: the fix changed
//! `Provider::list_sessions`'s return type, so a test written against the trait
//! could not be compiled against the code it is supposed to catch. These can,
//! and each one fails on the unfixed build for the reason it names.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

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

/// Run `casr list --json` and return the parsed envelope.
fn list_json(tmp: &TempDir) -> serde_json::Value {
    let output = casr_cmd(tmp)
        .args(["list", "--json"])
        .output()
        .expect("casr list should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "list --json should emit an envelope: {e}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Paths in `items`, i.e. what the listing rendered as sessions.
fn listed_paths(envelope: &serde_json::Value) -> Vec<String> {
    envelope["items"]
        .as_array()
        .expect("items is an array")
        .iter()
        .map(|item| item["path"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Paths in `skipped`, i.e. what the listing could not read.
fn skipped_paths(envelope: &serde_json::Value) -> Vec<String> {
    envelope["skipped"]
        .as_array()
        .expect("skipped is an array")
        .iter()
        .map(|item| item["path"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir -p");
    fs::write(path, contents).expect("write fixture");
}

/// A one-message ClawdBot/OpenClaw/pi transcript in `SessionManager` form.
fn pi_transcript(id: &str) -> String {
    format!(
        "{}\n{}\n",
        format_args!(
            r#"{{"type":"session","version":2,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z"}}"#
        ),
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#
    )
}

// ---------------------------------------------------------------------------
// #40 — a directory that could not be read is not zero sessions
// ---------------------------------------------------------------------------

/// A store path occupied by a regular file must be reported, not silently
/// counted as an empty store.
///
/// `ENOTDIR` rather than `EACCES` because this suite is run as root on at
/// least one machine, where `chmod 000` denies nothing (see
/// `unreadable_store_directory_is_reported` below, which skips itself there).
/// `ENOTDIR` is refused for every user including root, so this test always
/// runs — and it exercises exactly the same swallow: the guard was
/// `if !projects_dir.is_dir() { return Some(vec![]) }`, and `is_dir()` is
/// `false` for a file just as it is for a directory that is not there.
#[test]
fn store_path_occupied_by_a_file_is_reported_not_counted_as_zero() {
    let tmp = TempDir::new().expect("tempdir");
    let claude_home = tmp.path().join("claude");
    fs::create_dir_all(&claude_home).expect("mkdir claude home");
    // `~/.claude/projects` is where every Claude Code transcript lives. Here
    // it is a file, so it holds none and can enumerate none.
    fs::write(claude_home.join("projects"), "not a directory").expect("write");

    let envelope = list_json(&tmp);
    let skipped = skipped_paths(&envelope);

    assert!(
        skipped
            .iter()
            .any(|path| path.starts_with(claude_home.join("projects").to_str().expect("utf-8"))),
        "an unreadable Claude Code store must appear in `skipped`; got items={:?} skipped={:?}",
        listed_paths(&envelope),
        envelope["skipped"],
    );
}

/// The same fact through the failure that actually happens in the field: a
/// session directory the running user is not allowed to read.
///
/// Skipped where the test process can read a `0o000` directory anyway — root
/// with `CAP_DAC_OVERRIDE`. The probe is done rather than assumed because
/// "running as root" and "root can bypass this filesystem's permissions" are
/// not the same statement, and only the second one makes the test meaningless.
#[cfg(unix)]
#[test]
fn unreadable_store_directory_is_reported_not_counted_as_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("tempdir");

    // Capability probe: can this process read a directory it has no `r` bit for?
    let probe = tmp.path().join("dac-probe");
    fs::create_dir_all(&probe).expect("mkdir probe");
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).expect("chmod probe");
    let dac_enforced = fs::read_dir(&probe).is_err();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).expect("restore probe");
    if !dac_enforced {
        eprintln!(
            "skipping: this process can read a 0o000 directory (CAP_DAC_OVERRIDE), \
             so an unreadable directory cannot be constructed here"
        );
        return;
    }

    let projects = tmp.path().join("claude").join("projects");
    fs::create_dir_all(&projects).expect("mkdir projects");
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o000)).expect("chmod projects");

    let envelope = list_json(&tmp);
    let skipped = skipped_paths(&envelope);

    // Restore before asserting so the temp dir can always be cleaned up.
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o755)).expect("restore projects");

    assert!(
        skipped
            .iter()
            .any(|path| path.starts_with(projects.to_str().expect("utf-8"))),
        "a Claude Code store that denies reads must appear in `skipped`; got {:?}",
        envelope["skipped"],
    );
}

/// The other half of the distinction, and the reason `skipped` cannot simply
/// be "anything that produced no sessions": a store directory that is not
/// there yet is the ordinary state of an installed-but-unused tool, and must
/// stay silent.
///
/// Without this, the fix for the two tests above would trade one lie for
/// another — every provider a user has installed and not run would report a
/// failure on every listing.
#[test]
fn absent_store_directory_is_not_reported_as_a_failure() {
    let tmp = TempDir::new().expect("tempdir");
    // Installed — `detect` calls the home directory evidence — but never run,
    // so `projects/` does not exist.
    fs::create_dir_all(tmp.path().join("claude")).expect("mkdir claude home");

    let envelope = list_json(&tmp);

    assert_eq!(
        envelope["skipped"].as_array().map(Vec::len),
        Some(0),
        "a store that has never been created is not a read failure; got {:?}",
        envelope["skipped"],
    );
}

/// No registered provider inherits the blanket "any plausible extension" rule.
///
/// `Provider::is_session_path` has a default so that the three mock providers
/// in the test suite do not have to answer a question they have no store for.
/// That default is the defect: it is the extension list `cmd_list` used to
/// apply to every file under every root, and a provider that silently inherits
/// it is back to rendering `settings.json` as a session.
///
/// A design guard, not a revert-proof test: the method it checks does not
/// exist on the unfixed build, so it cannot be compiled against it. What it
/// asserts is that every one of the seventeen has *taken a decision* — each
/// must reject at least one of the six extensions the default accepts.
#[test]
fn every_registered_provider_narrows_the_default_session_file_rule() {
    use casr::discovery::ProviderRegistry;

    let registry = ProviderRegistry::default_registry();
    let inherited: Vec<&str> = registry
        .all_providers()
        .into_iter()
        .filter(|provider| {
            ["jsonl", "json", "vscdb", "md", "db", "sqlite"]
                .iter()
                .all(|ext| provider.is_session_path(Path::new(&format!("/store/probe.{ext}"))))
        })
        .map(|provider| provider.slug())
        .collect();

    assert!(
        inherited.is_empty(),
        "these providers accept every extension the old blanket rule accepted, \
         i.e. they state no rule of their own: {inherited:?}",
    );
}

// ---------------------------------------------------------------------------
// #47 — "installed" and "has a session store" are different facts
// ---------------------------------------------------------------------------

/// Detection evidence for a provider must name the directory `list` reads and
/// say whether it is there.
///
/// `detect` is satisfied by `claude` in `PATH` or by `~/.claude` existing;
/// `list` reads `~/.claude/projects`, and neither of those implies it. Without
/// this the user gets `✓ Claude Code — /root/.claude exists` from one command
/// and an empty listing from the other, and nothing anywhere distinguishes
/// "no sessions yet" from "casr is reading a directory that does not exist".
///
/// The evidence is checked, not `installed`: narrowing `detect` to require the
/// store would report `✗` for a CLI the user can run right now, which is a
/// worse answer than the one being fixed.
#[test]
fn detection_names_the_store_list_reads_when_it_is_absent() {
    let tmp = TempDir::new().expect("tempdir");
    fs::create_dir_all(tmp.path().join("claude")).expect("mkdir claude home");
    let projects = tmp.path().join("claude").join("projects");

    let output = casr_cmd(&tmp)
        .args(["providers", "--json"])
        .output()
        .expect("casr providers should run");
    let providers: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("providers --json is an array");
    let claude = providers
        .as_array()
        .expect("array")
        .iter()
        .find(|p| p["slug"] == "claude-code")
        .expect("claude-code is registered")
        .clone();

    assert_eq!(claude["installed"], serde_json::json!(true));
    let evidence = claude["evidence"]
        .as_array()
        .expect("evidence array")
        .iter()
        .filter_map(|e| e.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        evidence.contains(projects.to_str().expect("utf-8")),
        "detection must name the store `list` reads; got: {evidence}",
    );
}

/// And it must say something *different* once the store is there, or the line
/// carries no information.
#[test]
fn detection_distinguishes_a_store_that_exists_from_one_that_does_not() {
    let with_store = TempDir::new().expect("tempdir");
    fs::create_dir_all(with_store.path().join("claude").join("projects")).expect("mkdir");
    let without_store = TempDir::new().expect("tempdir");
    fs::create_dir_all(without_store.path().join("claude")).expect("mkdir");

    let evidence_for = |tmp: &TempDir| -> String {
        let output = casr_cmd(tmp)
            .args(["providers", "--json"])
            .output()
            .expect("casr providers should run");
        let providers: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("providers --json is an array");
        providers
            .as_array()
            .expect("array")
            .iter()
            .find(|p| p["slug"] == "claude-code")
            .expect("claude-code is registered")["evidence"]
            .as_array()
            .expect("evidence array")
            .iter()
            .filter_map(|e| e.as_str())
            // Drop the temp-dir paths, which differ between the two runs for a
            // reason that has nothing to do with the store's state.
            .map(|line| line.replace(tmp.path().to_str().expect("utf-8"), "<tmp>"))
            .collect::<Vec<_>>()
            .join(" | ")
    };

    let present = evidence_for(&with_store);
    let absent = evidence_for(&without_store);
    assert_ne!(
        present, absent,
        "an existing store and a missing one must not produce identical evidence",
    );
}

// ---------------------------------------------------------------------------
// #41 — a file that is not a session is not a session
// ---------------------------------------------------------------------------

/// ClawdBot keeps its session *index* in the same directory as its
/// transcripts. `clawdbot@2026.1.24-3`, `dist/config/sessions/paths.js`:
/// `path.join(resolveAgentSessionsDir(agentId), "sessions.json")`.
///
/// ClawdBot's own rule for a transcript is `entry.isFile() &&
/// name.endsWith(".jsonl")` (`dist/memory/session-files.js`), which excludes it
/// — as it excludes the `sessions.json.lock`, `sessions.json.<pid>.<uuid>.tmp`
/// and `<sessionId>.jsonl.lock` neighbours the store also writes.
#[test]
fn clawdbot_session_index_is_not_rendered_as_a_session() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = tmp.path().join("clawdbot");
    write(
        &sessions.join("11111111-2222-3333-4444-555555555555.jsonl"),
        &pi_transcript("11111111-2222-3333-4444-555555555555"),
    );
    write(
        &sessions.join("sessions.json"),
        "{\n  \"sessions\": []\n}\n",
    );
    write(&sessions.join("sessions.json.lock"), "lock");
    write(&sessions.join("sessions.json.4242.abcd.tmp"), "{}");

    let envelope = list_json(&tmp);
    let listed = listed_paths(&envelope);

    assert!(
        !listed
            .iter()
            .any(|path| path.ends_with("clawdbot/sessions.json")),
        "ClawdBot's session index is not a session; listed {listed:?}",
    );
    assert_eq!(
        listed.len(),
        1,
        "exactly the one transcript should be listed; got {listed:?}",
    );
}

/// OpenClaw writes at least three other `.jsonl` files into the directory
/// holding its transcripts, and publishes the rule that tells them apart —
/// `isPrimarySessionTranscriptFileName` in `openclaw@2026.7.1-2`,
/// `dist/paths-C2C4lJH6.js`. A trajectory artifact and a compaction checkpoint
/// are both real content, and neither is a session a user can resume.
#[test]
fn openclaw_trajectory_and_checkpoint_artifacts_are_not_rendered_as_sessions() {
    let tmp = TempDir::new().expect("tempdir");
    let id = "11111111-2222-3333-4444-555555555555";
    let sessions = tmp
        .path()
        .join("openclaw-state")
        .join("agents")
        .join("main")
        .join("sessions");
    write(&sessions.join(format!("{id}.jsonl")), &pi_transcript(id));
    write(
        &sessions.join(format!("{id}.trajectory.jsonl")),
        &pi_transcript(id),
    );
    write(
        &sessions.join(format!(
            "{id}.checkpoint.aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"
        )),
        &pi_transcript(id),
    );
    write(
        &sessions.join("sessions.json"),
        "{\n  \"sessions\": []\n}\n",
    );

    let envelope = list_json(&tmp);
    let listed = listed_paths(&envelope);

    assert_eq!(
        listed.len(),
        1,
        "only the primary transcript is a session; got {listed:?}",
    );
    assert!(
        listed[0].ends_with(&format!("sessions/{id}.jsonl")),
        "the primary transcript should be the one listed; got {listed:?}",
    );
}

/// `droid@0.180.0` writes a `<sessionId>.settings.json` beside every
/// transcript, and keeps side-chat forks in `sessions/btw/`, which its own
/// session list drops (`if (!session || session.isBtwFork) return []`).
#[test]
fn factory_settings_sidecar_and_btw_fork_are_not_rendered_as_sessions() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = tmp.path().join("factory");
    write(
        &sessions.join("fsess.jsonl"),
        "{\"type\":\"session\",\"id\":\"fsess\"}\n",
    );
    write(&sessions.join("fsess.settings.json"), "{\"model\":\"x\"}\n");
    write(&sessions.join(".favorites"), "[\"fsess\"]\n");
    write(
        &sessions.join("btw").join("bsess.jsonl"),
        "{\"type\":\"session\",\"id\":\"bsess\"}\n",
    );

    let envelope = list_json(&tmp);
    let listed = listed_paths(&envelope);

    assert_eq!(
        listed.len(),
        1,
        "only the transcript is a session; got {listed:?}",
    );
    assert!(
        listed[0].ends_with("factory/fsess.jsonl"),
        "the transcript should be the one listed; got {listed:?}",
    );
}

/// A Vibe session is a directory whose transcript is `messages.jsonl`
/// (`mistral-vibe==2.22.0`, `session_loader.py`: `MESSAGES_FILENAME =
/// "messages.jsonl"`). `meta.json` sits beside it and is not one.
#[test]
fn vibe_metadata_sidecar_is_not_rendered_as_a_session() {
    let tmp = TempDir::new().expect("tempdir");
    let session_dir = tmp
        .path()
        .join("vibe")
        .join("logs")
        .join("session")
        .join("session_20260101_000000_abc123");
    write(
        &session_dir.join("messages.jsonl"),
        "{\"role\":\"user\",\"content\":\"hello vibe\"}\n",
    );
    write(
        &session_dir.join("meta.json"),
        "{\n  \"id\": \"abc123\"\n}\n",
    );

    let envelope = list_json(&tmp);
    let listed = listed_paths(&envelope);

    assert_eq!(
        listed.len(),
        1,
        "only `messages.jsonl` is a session; got {listed:?}",
    );
    assert!(
        listed[0].ends_with("messages.jsonl"),
        "the transcript should be the one listed; got {listed:?}",
    );
}

/// The mirror-image failure: Cline's sidecars were not rendered as sessions —
/// its reader refuses them — but every refusal was reported as a session that
/// could not be read. Six lines of `skipped` per task, on every run, which is
/// how a channel meant for real failures becomes something a user learns to
/// ignore.
///
/// The names are from the shipped extension (`saoudrizwan.claude-dev@4.0.11`):
/// `GlobalFileNames` gives `context_history.json`, `task_metadata.json` and a
/// per-task `settings.json`; the focus-chain file is
/// `focus_chain_taskid_<id>.md`; and every atomic save writes
/// `<target>.tmp.<epochMs>.<rand>.json` before renaming, so an interrupted one
/// leaves a `.json` file whose name starts with the transcript's.
#[test]
fn cline_task_sidecars_are_neither_listed_nor_reported_as_unreadable() {
    let tmp = TempDir::new().expect("tempdir");
    let task = tmp.path().join("cline").join("tasks").join("1700000000000");
    write(
        &task.join("api_conversation_history.json"),
        r#"[{"role":"user","content":[{"type":"text","text":"hello"}]}]"#,
    );
    write(
        &task.join("ui_messages.json"),
        r#"[{"ts":1700000000000,"type":"say","say":"text","text":"hello"}]"#,
    );
    write(
        &task.join("task_metadata.json"),
        r#"{"files_in_context":[]}"#,
    );
    write(&task.join("context_history.json"), r#"{"0":{}}"#);
    write(&task.join("settings.json"), r#"{"model":"x"}"#);
    write(
        &task.join("focus_chain_taskid_1700000000000.md"),
        "# focus\n",
    );
    write(
        &task.join("api_conversation_history.json.tmp.1769000000000.k3j9x.json"),
        "[]",
    );
    write(&task.join("checkpoints").join("index.json"), "{}");

    let envelope = list_json(&tmp);
    let listed = listed_paths(&envelope);
    let skipped = skipped_paths(&envelope);

    assert_eq!(listed.len(), 1, "the task is one session; got {listed:?}",);
    assert!(
        listed[0].ends_with("api_conversation_history.json"),
        "the API history is the task's transcript; got {listed:?}",
    );
    assert!(
        skipped.is_empty(),
        "a file Cline is known to write beside a transcript is not an unreadable \
         session; got {:?}",
        envelope["skipped"],
    );
}
