//! Which directories `casr` will read a `pi` session out of, and which it will
//! only *resolve* one out of.
//!
//! Two different bugs, on two different code paths, plus the decision that
//! joins them.
//!
//! * **Resolve.** `PiAgent::sessions_dir` fell back to the whole of
//!   `<agent-dir>` whenever `<agent-dir>/sessions` was absent, and walked it
//!   with no `max_depth` at all. `detect` hid that on the automatic path, but
//!   `--source pi` goes straight to `owns_session`
//!   (`discovery.rs::resolve_with_alias`), so `casr info <id> --source pi`
//!   resolved a debug log and a cached tool output and rendered each as a
//!   session. `<agent-dir>` is also where `auth.json` lives.
//!
//! * **List.** `PI_CODING_AGENT_SESSION_DIR` is real —
//!   `@mariozechner/pi-coding-agent@0.73.1` builds the name at
//!   `dist/config.js:341` and reads it at `dist/main.js:384-387` — and casr
//!   ignored it on purpose. With it set, `pi` writes every session into a
//!   directory casr never looked at, so casr answered "no `pi` sessions".
//!
//! The join is that the fallback root was a guess standing in for the override:
//! "sessions are probably somewhere under here". Replacing the guess with the
//! directories `pi` actually reads makes the walk bounded *and* makes the
//! override work, and it is the same change.
//!
//! Every test drives the compiled binary, for the reason
//! `session_depth_scope_test.rs` gives: the `list --json` envelope is the only
//! place that shows both halves of a listing symptom — a bogus row and a bogus
//! "could not be read" warning — and a leak moves between the two depending on
//! whether the decoy happens to parse.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

/// A `casr` invocation whose every provider home points inside `tmp`.
///
/// `HOME` is redirected too, which the sibling suites do not need to do: the
/// point of several of these tests is what casr does when `PI_AGENT_HOME` is
/// *absent*, and without `PI_AGENT_HOME` the default agent dir is `~/.pi/agent`.
///
/// `XDG_DATA_HOME` matters twice over — it is Amp's store *and* casr's own
/// session store — so leaving it unset would have these tests write into
/// `~/.local/share/ags` on the machine running them.
fn casr_cmd(tmp: &TempDir) -> Command {
    #[allow(deprecated)]
    let mut cmd = Command::cargo_bin("ags").expect("ags binary should be built");
    // 转换那套收在 `ags convert` 底下（`ags` 本身是会话运行时）。前缀加在这里
    // 而不是每个用例里：这个文件测的全是转换。
    cmd.arg("convert");
    cmd.env("HOME", tmp.path().join("home"))
        .env("USERPROFILE", tmp.path().join("home"))
        .env("CLAUDE_HOME", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("GEMINI_HOME", tmp.path().join("gemini"))
        .env("CURSOR_HOME", tmp.path().join("cursor"))
        .env("CURSOR_CONFIG_DIR", tmp.path().join("cursor-cli-config"))
        .env("CURSOR_DATA_DIR", tmp.path().join("cursor-cli-data"))
        .env("CLINE_HOME", tmp.path().join("cline"))
        .env("CLINE_DATA_DIR", tmp.path().join("cline-data"))
        .env("AIDER_HOME", tmp.path().join("aider"))
        .env("OPENCODE_HOME", tmp.path().join("opencode"))
        .env("CHATGPT_HOME", tmp.path().join("chatgpt"))
        .env("CLAWDBOT_HOME", tmp.path().join("clawdbot"))
        .env("CLAWDBOT_STATE_DIR", tmp.path().join("clawdbot-state"))
        .env("VIBE_HOME", tmp.path().join("vibe"))
        .env("FACTORY_HOME", tmp.path().join("factory"))
        .env("OPENCLAW_HOME", tmp.path().join("openclaw"))
        .env("OPENCLAW_STATE_DIR", tmp.path().join("openclaw-state"))
        .env("KIRO_HOME", tmp.path().join("kiro"))
        .env("GROK_HOME", tmp.path().join("grok"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("NO_COLOR", "1")
        .env_remove("AGS_STORE")
        .env_remove("PI_AGENT_HOME")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .current_dir(tmp.path());
    cmd
}

/// `casr` aimed at `<tmp>/pi-agent` as the `pi` agent directory, the way a user
/// with a real `pi` install is aimed at `~/.pi/agent`.
///
/// `PI_CODING_AGENT_DIR` rather than casr's own `PI_AGENT_HOME`, because
/// `PI_AGENT_HOME` deliberately suppresses the session-dir override and most of
/// these tests are about the override.
fn pi_cmd(tmp: &TempDir) -> Command {
    let mut cmd = casr_cmd(tmp);
    cmd.env("PI_CODING_AGENT_DIR", agent_dir(tmp));
    cmd
}

fn agent_dir(tmp: &TempDir) -> PathBuf {
    tmp.path().join("pi-agent")
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir -p");
    fs::write(path, contents).expect("write fixture");
}

/// A one-message transcript in the envelope `pi`'s `SessionManager` writes.
///
/// `cwd` is the test's own temp directory throughout, because `cmd_list` scopes
/// to the working-directory project unless told otherwise and these runs happen
/// there.
fn pi_transcript(cwd: &Path, id: &str) -> String {
    let cwd = cwd.display().to_string().replace('\\', "/");
    format!(
        "{}\n{}\n",
        format_args!(
            r#"{{"type":"session","version":2,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}","provider":"anthropic","modelId":"claude-3-opus"}}"#
        ),
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello pi"}}"#
    )
}

/// Run `casr list --json` for one provider and return the parsed envelope.
fn list_json(mut cmd: Command, tmp: &TempDir) -> serde_json::Value {
    let output = cmd
        .args(["list", "--provider", "pi-agent", "--json"])
        .current_dir(tmp.path())
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

/// Paths one provider contributed to `key`, relative to `tmp` so the assertions
/// read like the store layout.
fn paths_under(envelope: &serde_json::Value, key: &str, tmp: &TempDir) -> Vec<String> {
    let mut out: Vec<String> = envelope[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` is an array"))
        .iter()
        .filter(|item| item["provider"].as_str() == Some("pi-agent"))
        .map(|item| {
            let path = item["path"].as_str().unwrap_or_default();
            Path::new(path)
                .strip_prefix(tmp.path())
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string())
        })
        .collect();
    out.sort();
    out
}

/// What the listing rendered as sessions.
fn rows(envelope: &serde_json::Value, tmp: &TempDir) -> Vec<String> {
    paths_under(envelope, "items", tmp)
}

/// What the listing reported as a session it could not read — the
/// `⚠ N path(s) could not be read` line, itemised.
fn warnings(envelope: &serde_json::Value, tmp: &TempDir) -> Vec<String> {
    paths_under(envelope, "skipped", tmp)
}

/// The path `casr info <id> --source pi` resolved, or `None` if it refused.
fn resolved_path(mut cmd: Command, tmp: &TempDir, session_id: &str) -> Option<String> {
    let output = cmd
        .args(["--json", "info", session_id, "--source", "pi"])
        .current_dir(tmp.path())
        .output()
        .expect("casr info should run");
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("info --json should emit an envelope: {e}\nstdout:\n{stdout}"));
    Some(
        parsed["source_path"]
            .as_str()
            .expect("info reports the path it read")
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Resolve: the walk that had no bound
// ---------------------------------------------------------------------------

/// `--source pi` bypasses `detect`, so `owns_session` is reachable with no
/// `sessions/` directory in existence at all. Everything under `<agent-dir>`
/// was fair game, to any depth.
///
/// Both decoys are shapes casr itself has been measured resolving. Neither is a
/// place `pi` puts a session: `pi` reads them out of `<agent-dir>/sessions`
/// (`SessionManager.listAll`), out of the directory `--session-dir` /
/// `PI_CODING_AGENT_SESSION_DIR` / `settings.json:sessionDir` names
/// (`SessionManager.list`), and — until the next startup migration moves them —
/// out of `<agent-dir>/*.jsonl` itself.
#[test]
fn resolve_by_id_does_not_wander_out_of_the_directories_pi_reads() {
    let tmp = TempDir::new().expect("tempdir");
    let agent = agent_dir(&tmp);

    write(
        &agent.join("logs/deep/deeper/2026-01-01T00-00-00_buried.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_buried"),
    );
    write(
        &agent.join("cache/2026-01-01T00-00-00_tooloutput.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_tooloutput"),
    );

    assert_eq!(
        resolved_path(pi_cmd(&tmp), &tmp, "2026-01-01T00-00-00_buried"),
        None,
        "a file three directories below the agent root is not a pi session"
    );
    assert_eq!(
        resolved_path(pi_cmd(&tmp), &tmp, "2026-01-01T00-00-00_tooloutput"),
        None,
        "a file in a sibling of sessions/ is not a pi session"
    );
}

/// The control for the test above: narrowing the walk did not narrow it onto
/// nothing.
#[test]
fn resolve_by_id_still_finds_a_session_in_both_layouts() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = agent_dir(&tmp).join("sessions");

    // What `pi` writes: sessions/--<encoded-cwd>--/<stamp>_<id>.jsonl.
    write(
        &sessions.join("--tmp--/2026-01-01T00-00-00_vendor.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_vendor"),
    );
    // What casr's own writer produces: sessions/<id>.jsonl.
    write(
        &sessions.join("2026-01-01T00-00-00_casr.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_casr"),
    );

    for id in ["2026-01-01T00-00-00_vendor", "2026-01-01T00-00-00_casr"] {
        assert!(
            resolved_path(pi_cmd(&tmp), &tmp, id).is_some(),
            "{id} should still resolve"
        );
    }
}

/// The one place listing and ownership are allowed to disagree.
///
/// `pi` 0.30.0 saved sessions to `~/.pi/agent/` instead of
/// `~/.pi/agent/sessions/<encoded-cwd>/` (pi-mono issue #320), and
/// `migrateSessionsFromAgentRoot` (`dist/migrations.js:75-116`) still runs on
/// every startup to move them. Until it does, the file is a real session in a
/// place neither of `pi`'s listers looks — so casr must not list it (it would
/// appear twice the moment the migration ran, once from each location) and must
/// still resolve it when a user names it.
#[test]
fn a_session_the_startup_migration_has_not_moved_yet_resolves_but_is_not_listed() {
    let tmp = TempDir::new().expect("tempdir");
    let agent = agent_dir(&tmp);

    write(
        &agent.join("2026-01-01T00-00-00_stranded.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_stranded"),
    );
    // A listing root has to exist for the provider to be detected at all.
    write(
        &agent.join("sessions/--tmp--/2026-01-01T00-00-01_live.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-01_live"),
    );

    let envelope = list_json(pi_cmd(&tmp), &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-agent/sessions/--tmp--/2026-01-01T00-00-01_live.jsonl"],
        "the pre-migration file is not something pi lists; warnings were {:?}",
        warnings(&envelope, &tmp)
    );

    assert!(
        resolved_path(pi_cmd(&tmp), &tmp, "2026-01-01T00-00-00_stranded")
            .is_some_and(|path| path.ends_with("2026-01-01T00-00-00_stranded.jsonl")),
        "but naming it by id should still find it"
    );
}

// ---------------------------------------------------------------------------
// List: depth under the sessions tree
// ---------------------------------------------------------------------------

/// `SessionManager.listAll` is exactly two levels —
/// `readdir(getSessionsDir()).filter(isDirectory)` then
/// `readdir(dir).filter(f => f.endsWith(".jsonl"))`,
/// `dist/core/session-manager.js:1065-1081` — and casr's own writer puts a
/// converted session one level up at `sessions/<id>.jsonl`.
///
/// Both are listed. Transcribing only the vendor half would make every session
/// casr has ever written unlistable by casr, which is the trap `vibe.rs`
/// documents for the `session_` prefix and `meta.json` requirement. What the
/// rule does exclude is a third level, which no `pi` and no casr writes.
#[test]
fn the_sessions_tree_is_listed_two_levels_deep_and_no_further() {
    let tmp = TempDir::new().expect("tempdir");
    let sessions = agent_dir(&tmp).join("sessions");

    write(
        &sessions.join("2026-01-01T00-00-00_casr.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_casr"),
    );
    write(
        &sessions.join("--tmp--/2026-01-01T00-00-01_vendor.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-01_vendor"),
    );
    write(
        &sessions.join("--tmp--/archive/2026-01-01T00-00-02_deep.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-02_deep"),
    );

    let envelope = list_json(pi_cmd(&tmp), &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec![
            "pi-agent/sessions/--tmp--/2026-01-01T00-00-01_vendor.jsonl",
            "pi-agent/sessions/2026-01-01T00-00-00_casr.jsonl",
        ],
        "the third level is not a place pi lists from; warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

// ---------------------------------------------------------------------------
// PI_CODING_AGENT_SESSION_DIR
// ---------------------------------------------------------------------------

/// The variable names the **leaf** directory, not the `sessions/` tree above it.
///
/// `dist/main.js:384-387` reads it into `sessionDir`, and `sessionDir` is the
/// `??` alternative to `getDefaultSessionDir(cwd)` — which is
/// `join(agentDir, "sessions", "--<cwd>--")`,
/// `dist/core/session-manager.js:211-219`. So `.jsonl` files sit directly in it,
/// and `SessionManager.list` reads it with a flat `readdir` (`:391-402`).
///
/// The old behaviour was not "casr lists a little less". `detect` tests the
/// listing roots, and with no `<agent-dir>/sessions` on disk it reported `pi` as
/// not installed, so `resolve_auto` skipped the provider entirely: every session
/// the user had was both unlistable and unresolvable.
#[test]
fn the_session_dir_override_is_a_leaf_directory_and_is_both_listed_and_resolvable() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = tmp.path().join("pi-elsewhere");

    write(
        &leaf.join("2026-01-01T00-00-00_override.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_override"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-elsewhere/2026-01-01T00-00-00_override.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    assert!(
        resolved_path(cmd, &tmp, "2026-01-01T00-00-00_override")
            .is_some_and(|path| path.ends_with("2026-01-01T00-00-00_override.jsonl")),
        "and the same session should resolve by id"
    );
}

/// The override root is read flat, so an override aimed at a busy directory
/// cannot turn the whole subtree into a session list.
///
/// `listSessionsFromDir` (`dist/core/session-manager.js:391-402`) is
/// `readdir(dir).filter(f => f.endsWith(".jsonl"))` with no recursion at all,
/// and casr's writer puts its file directly in the leaf, so nothing casr writes
/// needs the looser rule either.
#[test]
fn the_session_dir_override_is_read_flat() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = tmp.path().join("pi-elsewhere");

    write(
        &leaf.join("2026-01-01T00-00-00_flat.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_flat"),
    );
    write(
        &leaf.join("nested/2026-01-01T00-00-01_nested.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-01_nested"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-elsewhere/2026-01-01T00-00-00_flat.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

/// `SessionManager.listAll` calls `getSessionsDir()` directly and never consults
/// the override (`dist/core/session-manager.js:1066`), so `pi` itself still
/// lists the default tree when the override is set. Both roots stay live.
#[test]
fn the_default_sessions_tree_survives_the_override() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = tmp.path().join("pi-elsewhere");

    write(
        &leaf.join("2026-01-01T00-00-00_override.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_override"),
    );
    write(
        &agent_dir(&tmp).join("sessions/--tmp--/2026-01-01T00-00-01_default.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-01_default"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec![
            "pi-agent/sessions/--tmp--/2026-01-01T00-00-01_default.jsonl",
            "pi-elsewhere/2026-01-01T00-00-00_override.jsonl",
        ],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

/// An override pointing *inside* the default tree — the shape `pi`'s own
/// default has — must not make the same file arrive twice. `cmd_list`
/// accumulates candidates across roots without de-duplicating them, so the two
/// roots have to be arranged never to overlap.
#[test]
fn an_override_inside_the_sessions_tree_does_not_list_a_session_twice() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = agent_dir(&tmp).join("sessions/--tmp--");

    write(
        &leaf.join("2026-01-01T00-00-00_once.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_once"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-agent/sessions/--tmp--/2026-01-01T00-00-00_once.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

/// `pi` gates the variable on `envSessionDir ? … : undefined`
/// (`dist/main.js:386`), so an empty value falls through to the next source
/// rather than naming the current directory.
#[test]
fn an_empty_session_dir_override_counts_as_unset() {
    let tmp = TempDir::new().expect("tempdir");

    write(
        &agent_dir(&tmp).join("sessions/--tmp--/2026-01-01T00-00-00_default.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_default"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", "");
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-agent/sessions/--tmp--/2026-01-01T00-00-00_default.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

/// `pi` runs the value through `expandTildePath` (`dist/config.js:342-348`,
/// applied at `dist/main.js:386`), so `~/x` is `<home>/x` even when no shell
/// expanded it — which is the case for a value set in a config file, a
/// `systemd` unit, or an editor's integrated terminal profile.
#[test]
fn a_tilde_in_the_session_dir_override_is_expanded() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");

    write(
        &home.join("pi-sessions/2026-01-01T00-00-00_tilde.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_tilde"),
    );

    let mut cmd = pi_cmd(&tmp);
    cmd.env("PI_CODING_AGENT_SESSION_DIR", "~/pi-sessions");
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["home/pi-sessions/2026-01-01T00-00-00_tilde.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

/// `PI_AGENT_HOME` is casr's own knob for aiming casr at a tree, and an aiming
/// knob an ambient `pi` variable can drag elsewhere does not aim. It suppresses
/// the override rather than losing to it.
#[test]
fn pi_agent_home_suppresses_the_session_dir_override() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = tmp.path().join("pi-elsewhere");

    write(
        &leaf.join("2026-01-01T00-00-00_override.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-00_override"),
    );
    write(
        &agent_dir(&tmp).join("sessions/--tmp--/2026-01-01T00-00-01_aimed.jsonl"),
        &pi_transcript(tmp.path(), "2026-01-01T00-00-01_aimed"),
    );

    let mut cmd = casr_cmd(&tmp);
    cmd.env("PI_AGENT_HOME", agent_dir(&tmp))
        .env("PI_CODING_AGENT_SESSION_DIR", &leaf);
    let envelope = list_json(cmd, &tmp);
    assert_eq!(
        rows(&envelope, &tmp),
        vec!["pi-agent/sessions/--tmp--/2026-01-01T00-00-01_aimed.jsonl"],
        "warnings were {:?}",
        warnings(&envelope, &tmp)
    );
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Honouring the override on the read side alone would be half a fix: casr
/// would list the sessions `pi` has and then write a converted one into the
/// default tree `pi` has been configured away from, where neither
/// `SessionManager.list` nor `SessionManager.listAll` would ever show it.
///
/// The vendor artifact is the oracle for the *shape*, not casr's read-back: the
/// assertion is on where the file lands, and `SessionManager.list` reads that
/// directory with a flat `readdir`.
#[test]
fn a_conversion_lands_in_the_overridden_session_dir() {
    let tmp = TempDir::new().expect("tempdir");
    let leaf = tmp.path().join("pi-elsewhere");
    fs::create_dir_all(&leaf).expect("mkdir override dir");

    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_code/cc_simple.jsonl");
    let content = fs::read_to_string(&fixture).expect("read cc_simple fixture");
    write(
        &tmp.path()
            .join("claude/projects/-data-projects-myapp/cc-simple-001.jsonl"),
        &content,
    );

    let mut cmd = pi_cmd(&tmp);
    let output = cmd
        .env("PI_CODING_AGENT_SESSION_DIR", &leaf)
        .args(["--json", "resume", "pi", "cc-simple-001"])
        .output()
        .expect("casr resume should run");
    assert!(
        output.status.success(),
        "resume failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let written: Vec<String> = fs::read_dir(&leaf)
        .expect("override dir readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        written.len(),
        1,
        "one converted session, directly in the override dir, got {written:?}"
    );
    assert!(
        written[0].ends_with(".jsonl"),
        "pi reads `.jsonl` and nothing else out of that directory, got {written:?}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).expect("resume --json emits an envelope");
    let resume_command = serde_json::to_string(&envelope).expect("re-serialise");
    assert!(
        resume_command.contains(&leaf.display().to_string()),
        "the resume command has to name the file that was written, envelope was:\n{stdout}"
    );
}
