//! End-to-end CLI integration tests for casr.
//!
//! Uses `assert_cmd` to invoke the compiled binary and validate output.
//! All tests use temp directories with env overrides (`CLAUDE_HOME`,
//! `CODEX_HOME`, `GEMINI_HOME`, `CURSOR_HOME`, `CLINE_HOME`, `AIDER_HOME`,
//! `OPENCODE_HOME`, `CHATGPT_HOME`, `CLAWDBOT_HOME`, `VIBE_HOME`,
//! `FACTORY_HOME`, and `XDG_DATA_HOME` for Amp) so they never touch real
//! provider data.

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Root of the fixtures directory.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Build a `Command` for the casr binary with isolated provider homes.
///
/// Sets provider home overrides to subdirs of the provided temp dir so the
/// CLI never touches real provider data.
fn casr_cmd(tmp: &TempDir) -> Command {
    #[allow(deprecated)]
    let mut cmd = Command::cargo_bin("casr").expect("casr binary should be built");
    cmd.env("CLAUDE_HOME", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("GEMINI_HOME", tmp.path().join("gemini"))
        .env("CURSOR_HOME", tmp.path().join("cursor"))
        .env("CLINE_HOME", tmp.path().join("cline"))
        .env("AIDER_HOME", tmp.path().join("aider"))
        .env("OPENCODE_HOME", tmp.path().join("opencode"))
        .env("CHATGPT_HOME", tmp.path().join("chatgpt"))
        .env("CLAWDBOT_HOME", tmp.path().join("clawdbot"))
        .env("VIBE_HOME", tmp.path().join("vibe"))
        .env("FACTORY_HOME", tmp.path().join("factory"))
        .env("OPENCLAW_HOME", tmp.path().join("openclaw"))
        .env("PI_AGENT_HOME", tmp.path().join("pi-agent"))
        .env("GROK_HOME", tmp.path().join("grok"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        // Suppress colored output in tests.
        .env("NO_COLOR", "1");
    cmd
}

/// Set up a Claude Code session fixture in the temp dir.
///
/// Creates the expected directory structure:
/// `<claude_home>/projects/<project-key>/<session-id>.jsonl`
fn setup_cc_fixture(tmp: &TempDir, fixture_name: &str) -> String {
    setup_cc_fixture_custom(tmp, fixture_name, None, None)
}

/// Set up a Claude Code session fixture with optional workspace/session-id overrides.
fn setup_cc_fixture_custom(
    tmp: &TempDir,
    fixture_name: &str,
    workspace_override: Option<&str>,
    session_id_override: Option<&str>,
) -> String {
    let source = fixtures_dir().join(format!("claude_code/{fixture_name}.jsonl"));
    let original_content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    // Extract session ID and cwd from the fixture content.
    let first_line: serde_json::Value = original_content
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str(l).ok())
        .expect("fixture should have valid first line");

    let original_session_id = first_line["sessionId"].as_str().unwrap_or("unknown");
    let original_cwd = first_line["cwd"].as_str().unwrap_or("/tmp");
    let session_id = session_id_override
        .unwrap_or(original_session_id)
        .to_string();
    let cwd = workspace_override.unwrap_or(original_cwd);

    let content = original_content
        .replace(original_session_id, &session_id)
        .replace(original_cwd, cwd);

    // Derive project key: replace non-alphanumeric with dash.
    let project_key: String = cwd
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();

    let projects_dir = tmp.path().join("claude/projects").join(&project_key);
    fs::create_dir_all(&projects_dir).expect("create CC project dir");

    let target_path = projects_dir.join(format!("{session_id}.jsonl"));
    fs::write(&target_path, &content).expect("write CC fixture");

    session_id
}

/// Set up a Codex session fixture in the temp dir.
#[allow(dead_code)]
fn setup_codex_fixture(tmp: &TempDir, fixture_name: &str, ext: &str) -> String {
    let source = fixtures_dir().join(format!("codex/{fixture_name}.{ext}"));
    let content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    // For JSONL, extract session ID from session_meta payload.
    let session_id = if ext == "jsonl" {
        content
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "session_meta")
            .and_then(|v| v["payload"]["id"].as_str().map(String::from))
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        // Legacy JSON.
        let root: serde_json::Value = serde_json::from_str(&content).unwrap();
        root["session"]["id"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    };

    // Place in sessions dir with correct hierarchy.
    let sessions_dir = tmp.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions_dir).expect("create Codex sessions dir");

    let filename = format!("rollout-2026-01-01T00-00-00-{session_id}.{ext}");
    let target_path = sessions_dir.join(&filename);
    fs::write(&target_path, &content).expect("write Codex fixture");

    session_id
}

/// Set up a Gemini session fixture in the temp dir.
#[allow(dead_code)]
fn setup_gemini_fixture(tmp: &TempDir, fixture_name: &str) -> String {
    let source = fixtures_dir().join(format!("gemini/{fixture_name}.json"));
    let content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    let root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let session_id = root["sessionId"].as_str().unwrap_or("unknown").to_string();

    // Place in <hash>/chats/ directory.
    let hash_dir = tmp.path().join("gemini/tmp/testhash123/chats");
    fs::create_dir_all(&hash_dir).expect("create Gemini chats dir");

    let filename = format!("session-{session_id}.json");
    let target_path = hash_dir.join(&filename);
    fs::write(&target_path, &content).expect("write Gemini fixture");

    session_id
}

/// Put one Gemini session file in the chats directory, byte for byte.
///
/// Separate from [`setup_gemini_fixture`] because these tests need to write a
/// file the reader *cannot* parse, and every helper above derives the filename
/// from a session id it read out of valid JSON.
#[allow(dead_code)]
fn write_gemini_session_file(tmp: &TempDir, session_id: &str, content: &str) -> PathBuf {
    let hash_dir = tmp.path().join("gemini/tmp/testhash123/chats");
    fs::create_dir_all(&hash_dir).expect("create Gemini chats dir");
    let path = hash_dir.join(format!("session-{session_id}.json"));
    fs::write(&path, content).expect("write Gemini session file");
    path
}

/// A Gemini session file that parses, with the id the caller asks for.
#[allow(dead_code)]
fn write_good_gemini_session(tmp: &TempDir, session_id: &str) -> PathBuf {
    let source = fixtures_dir().join("gemini/gmi_simple.json");
    let content = fs::read_to_string(&source).expect("read gemini fixture");
    let mut root: serde_json::Value = serde_json::from_str(&content).expect("fixture parses");
    root["sessionId"] = serde_json::Value::String(session_id.to_string());
    write_gemini_session_file(tmp, session_id, &root.to_string())
}

/// A Gemini session file that does not parse: truncated mid-object.
#[allow(dead_code)]
fn write_corrupt_gemini_session(tmp: &TempDir, session_id: &str) -> PathBuf {
    write_gemini_session_file(
        tmp,
        session_id,
        "{\"sessionId\": \"x\", \"messages\": [ {\"type\": \"user\", \"content\": \"trunc",
    )
}

/// Set up a Vibe session fixture in the temp dir.
///
/// Vibe is the plainest case of a provider whose reader cannot determine a
/// workspace: `meta.json` has an explicitly null working directory. Its
/// default layout is
/// `<VIBE_HOME>/logs/session/session_<timestamp>_<id8>/messages.jsonl`.
#[allow(dead_code)]
fn setup_vibe_fixture(tmp: &TempDir, session_id: &str) -> String {
    let content =
        fs::read_to_string(fixtures_dir().join("vibe/messages.jsonl")).expect("read vibe fixture");
    let total_messages = content.lines().count();
    let short_id = session_id.get(..8).unwrap_or(session_id);
    let session_dir = tmp
        .path()
        .join("vibe/logs/session")
        .join(format!("session_20260101_000000_{short_id}"));
    fs::create_dir_all(&session_dir).expect("create Vibe session dir");
    fs::write(session_dir.join("messages.jsonl"), content).expect("write Vibe fixture");
    fs::write(
        session_dir.join("meta.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "session_id": session_id,
            "start_time": "2026-01-01T00:00:00+00:00",
            "end_time": null,
            "git_commit": null,
            "git_branch": null,
            "environment": {"working_directory": null},
            "username": "casr",
            "total_messages": total_messages,
        }))
        .expect("serialize Vibe metadata"),
    )
    .expect("write Vibe metadata");
    session_id.to_string()
}

// ---------------------------------------------------------------------------
// Basic CLI tests
// ---------------------------------------------------------------------------

#[test]
fn cli_version_outputs_metadata() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("casr"));
}

#[test]
fn cli_help_outputs_usage() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cross Agent Session Resumer"))
        .stdout(predicate::str::contains("resume"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("providers"));
}

#[test]
fn cli_no_args_shows_error() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn cli_invalid_subcommand_fails() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp).arg("nonexistent").assert().failure();
}

// ---------------------------------------------------------------------------
// Providers command
// ---------------------------------------------------------------------------

#[test]
fn cli_providers_succeeds() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude Code"))
        .stdout(predicate::str::contains("Codex"))
        .stdout(predicate::str::contains("Gemini"))
        .stdout(predicate::str::contains("Cursor"))
        .stdout(predicate::str::contains("Cline"))
        .stdout(predicate::str::contains("Aider"))
        .stdout(predicate::str::contains("Amp"))
        .stdout(predicate::str::contains("OpenCode"));
}

#[test]
fn cli_providers_json_is_valid() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "providers"])
        .output()
        .expect("providers should run");

    assert!(output.status.success(), "providers --json should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("providers --json should emit valid JSON");
    assert!(parsed.is_array(), "providers JSON should be an array");
}

// ---------------------------------------------------------------------------
// List command
// ---------------------------------------------------------------------------

#[test]
fn cli_list_empty_shows_helpful_message() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("No sessions found"));
}

#[test]
fn cli_list_finds_cc_sessions() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");
    casr_cmd(&tmp)
        .args(["list", "--workspace", "/data/projects/myapp"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&session_id));
}

#[test]
fn cli_list_shows_full_session_id_and_last_active_for_current_project_scope() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let workspace_str = workspace.to_string_lossy().to_string();
    let session_id = setup_cc_fixture_custom(
        &tmp,
        "cc_simple",
        Some(&workspace_str),
        Some("366bd160-20b3-4e69-b0be-5a559ef5ffec"),
    );

    casr_cmd(&tmp)
        .current_dir(&workspace)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "current working-directory project",
        ))
        .stdout(predicate::str::contains("Last Active"))
        .stdout(predicate::str::contains("Size KB"))
        .stdout(predicate::str::contains("Unique Users"))
        .stdout(predicate::str::contains("Agent Avg Chars"))
        .stdout(predicate::str::contains("Tool Uses"))
        .stdout(predicate::str::contains(&session_id))
        .stdout(predicate::str::contains("…").not());
}

#[test]
fn cli_list_json_is_valid_array() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");
    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/myapp"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list --json should emit valid JSON");
    assert!(
        parsed.is_object(),
        "list --json should be an envelope object"
    );
    assert_eq!(parsed["schema_version"], 6);
    let items = parsed["items"].as_array().expect("items should be array");
    assert!(!items.is_empty());
    let first = &items[0];
    assert!(first.get("file_size_kb").is_some());
    assert!(first.get("unique_user_messages").is_some());
    assert!(first.get("avg_agent_response_chars_rounded").is_some());
    assert!(first.get("tool_uses").is_some());
    assert!(first.get("schema_version").is_some());
    assert!(first.get("workspace_name").is_some());
    assert!(first.get("workspace_name_source").is_some());
}

#[test]
fn cli_list_limit_respects_bound() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");
    setup_cc_fixture_custom(&tmp, "cc_malformed", Some("/data/projects/myapp"), None);
    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--workspace",
            "/data/projects/myapp",
            "--limit",
            "1",
        ])
        .output()
        .expect("list should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["items"].as_array().unwrap().len(), 1);
}

#[test]
fn cli_list_limit_applies_per_provider() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture_custom(&tmp, "cc_simple", Some("/data/projects/backend"), None);
    setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--workspace",
            "/data/projects/backend",
            "--limit",
            "1",
        ])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = parsed["items"]
        .as_array()
        .expect("list --json items should be an array");

    let mut counts_by_provider: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for session in sessions {
        let provider = session["provider"]
            .as_str()
            .expect("provider should be present")
            .to_string();
        *counts_by_provider.entry(provider).or_insert(0) += 1;
    }

    assert!(
        counts_by_provider.len() >= 2,
        "expected at least two providers in scoped list"
    );
    for (provider, count) in &counts_by_provider {
        assert!(
            *count <= 1,
            "expected per-provider limit=1, got {count} for provider {provider}"
        );
    }
}

#[test]
fn cli_list_workspace_filter_filters_sessions() {
    let tmp = TempDir::new().unwrap();
    let myapp_id = setup_cc_fixture(&tmp, "cc_simple"); // /data/projects/myapp
    let webapp_id = setup_cc_fixture(&tmp, "cc_complex"); // /data/projects/webapp

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/myapp"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = parsed["items"]
        .as_array()
        .expect("list --json items should be an array");

    assert!(
        sessions
            .iter()
            .any(|s| s["session_id"].as_str() == Some(&myapp_id)),
        "expected myapp session to be present"
    );
    assert!(
        !sessions
            .iter()
            .any(|s| s["session_id"].as_str() == Some(&webapp_id)),
        "expected webapp session to be filtered out"
    );
}

// ---------------------------------------------------------------------------
// List: sessions whose workspace no source can determine
//
// Four providers never record a workspace (vibe, chatgpt, clawdbot,
// antigravity) and thirteen more record one only when the session file happens
// to carry a `cwd`. `list` always has a workspace filter — `--workspace` if
// given, the working directory otherwise — so how the filter treats a session
// it cannot place decides whether those sessions are listable at all.
// ---------------------------------------------------------------------------

/// Without `--workspace` the user asked for a list, not a workspace question.
/// A session no source can place is unclassified, not disqualified, and must
/// still be listed.
#[test]
fn cli_list_without_workspace_filter_includes_workspace_less_provider() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_vibe_fixture(&tmp, "vibe-unplaced-0001");

    let output = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["--json", "list", "--provider", "vibe"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let items = parsed["items"].as_array().expect("items should be array");
    assert!(
        items
            .iter()
            .any(|s| s["session_id"].as_str() == Some(session_id.as_str())),
        "a session whose workspace cannot be determined must still appear in an \
         unfiltered list; got {items:?}"
    );
    assert_eq!(
        items[0]["workspace"],
        serde_json::Value::Null,
        "the unknown workspace must be reported as null, not invented"
    );
}

/// The unfiltered listing says so, rather than letting the "Project-scoped
/// sessions" heading assert a placement nobody measured.
#[test]
fn cli_list_without_workspace_filter_says_which_sessions_are_unplaced() {
    let tmp = TempDir::new().unwrap();
    setup_vibe_fixture(&tmp, "vibe-unplaced-0002");

    casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["list", "--provider", "vibe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("record no workspace"));
}

/// `--workspace X` asks which sessions belong to X. "Cannot tell" is not an
/// answer to that, so the session stays out — but the exclusion is announced,
/// because a filter that silently deletes what it could not classify is
/// indistinguishable from the provider having no sessions.
#[test]
fn cli_list_explicit_workspace_filter_hides_unplaceable_sessions_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let vibe_id = setup_vibe_fixture(&tmp, "vibe-unplaced-0003");

    let assert = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args([
            "--json",
            "list",
            "--provider",
            "vibe",
            "--workspace",
            "/data/projects/myapp",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("hidden by --workspace"))
        .stderr(predicate::str::contains("vibe"));

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        !parsed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"].as_str() == Some(vibe_id.as_str())),
        "an explicit --workspace must not claim an unplaceable session belongs to it"
    );
}

/// The other half of the contract: `--workspace` still means what it says for
/// the providers that do report one, and is not weakened by admitting the
/// unplaceable ones elsewhere.
#[test]
fn cli_list_explicit_workspace_filter_still_selects_among_reporting_providers() {
    let tmp = TempDir::new().unwrap();
    let myapp_id = setup_cc_fixture(&tmp, "cc_simple"); // /data/projects/myapp
    let webapp_id = setup_cc_fixture(&tmp, "cc_complex"); // /data/projects/webapp
    setup_vibe_fixture(&tmp, "vibe-unplaced-0004");

    let output = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["--json", "list", "--workspace", "/data/projects/myapp"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let items = parsed["items"].as_array().unwrap();

    assert!(
        items
            .iter()
            .any(|s| s["session_id"].as_str() == Some(myapp_id.as_str())),
        "the session recorded in the filtered workspace must be kept"
    );
    assert!(
        !items
            .iter()
            .any(|s| s["session_id"].as_str() == Some(webapp_id.as_str())),
        "a session recorded in a different workspace must still be filtered out"
    );
    assert!(
        !items.iter().any(|s| s["provider"].as_str() == Some("vibe")),
        "an unplaceable session must not leak into an explicitly filtered list"
    );
}

// ---------------------------------------------------------------------------
// A session file that will not parse is missing from the listing, and said so
// ---------------------------------------------------------------------------
//
// `list` used to read every candidate with `read_session(&path).ok()?`, so a
// file the reader rejected left the listing through the same door as a file
// that was never a session: no error, no warning, no count. The listing was
// short, and nothing anywhere said short of what. The same file read through
// `casr info` fails loudly and exits non-zero, which is what makes the silence
// a defect rather than a policy.

/// One bad file among good ones: the good ones still list, and the bad one is
/// named on stderr instead of disappearing.
#[test]
fn cli_list_names_the_session_file_it_could_not_read() {
    let tmp = TempDir::new().unwrap();
    write_good_gemini_session(&tmp, "gmi-readable-0001");
    write_good_gemini_session(&tmp, "gmi-readable-0002");
    let corrupt = write_corrupt_gemini_session(&tmp, "gmi-corrupt-0003");

    casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["list", "--provider", "gemini"])
        .assert()
        // One unreadable file must not take the listing with it.
        .success()
        .stdout(predicate::str::contains("gmi-readable-0001"))
        .stdout(predicate::str::contains("gmi-readable-0002"))
        .stderr(predicate::str::contains("1 path(s) could not be read"))
        .stderr(predicate::str::contains("gemini: 1"))
        .stderr(predicate::str::contains(
            corrupt.to_string_lossy().to_string(),
        ));
}

/// `--json` is the only channel a machine caller reads, so the same fact has to
/// be in the document, not only on stderr.
#[test]
fn cli_list_json_carries_the_session_files_it_could_not_read() {
    let tmp = TempDir::new().unwrap();
    write_good_gemini_session(&tmp, "gmi-readable-0011");
    let corrupt = write_corrupt_gemini_session(&tmp, "gmi-corrupt-0012");

    let output = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["--json", "list", "--provider", "gemini"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("list --json should emit valid JSON");

    let skipped = parsed["skipped"]
        .as_array()
        .expect("list --json must always carry a skipped array");
    assert_eq!(
        skipped.len(),
        1,
        "the one unreadable file should be reported once: {skipped:?}"
    );
    assert_eq!(skipped[0]["provider"], "gemini");
    assert_eq!(
        skipped[0]["path"].as_str().unwrap(),
        corrupt.to_string_lossy()
    );
    assert!(
        !skipped[0]["error"]
            .as_str()
            .expect("a reason, not a bare count")
            .is_empty(),
        "a skipped file without a reason is a count nobody can act on"
    );
    assert_eq!(parsed["schema_version"], 6);
}

/// Every file in the directory fails. The listing is empty and honest about
/// why, rather than reporting the provider as having nothing.
#[test]
fn cli_list_says_so_when_a_whole_directory_will_not_parse() {
    let tmp = TempDir::new().unwrap();
    write_corrupt_gemini_session(&tmp, "gmi-corrupt-0021");
    write_corrupt_gemini_session(&tmp, "gmi-corrupt-0022");
    write_corrupt_gemini_session(&tmp, "gmi-corrupt-0023");

    let output = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["list", "--provider", "gemini"])
        .output()
        .expect("list should run");

    // Partial results with an honest note, not an error page: a provider whose
    // files all fail is still not a reason to fail the whole command.
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("No sessions found"),
        "an empty listing is still an empty listing: {stdout}"
    );
    assert!(
        stderr.contains("3 path(s) could not be read"),
        "\"no sessions\" and \"three sessions I could not read\" must not print \
         the same thing: {stderr}"
    );
    assert!(
        stderr.contains("gemini: 3"),
        "the count has to name the provider it belongs to: {stderr}"
    );
}

/// A run with nothing to report says nothing. A directory of files that were
/// never sessions is not an error, and the warning must not become the line
/// nobody reads.
#[test]
fn cli_list_is_quiet_when_nothing_was_skipped() {
    let tmp = TempDir::new().unwrap();
    write_good_gemini_session(&tmp, "gmi-readable-0031");
    write_good_gemini_session(&tmp, "gmi-readable-0032");
    // Files under the provider's own root that are not sessions at all.
    let chats = tmp.path().join("gemini/tmp/testhash123/chats");
    fs::write(chats.join("notes.md"), "# not a session\n").unwrap();
    fs::write(
        tmp.path().join("gemini/settings.json"),
        "{\"theme\":\"dark\"}\n",
    )
    .unwrap();

    let output = casr_cmd(&tmp)
        .current_dir(tmp.path())
        .args(["--json", "list", "--provider", "gemini"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("could not be read"),
        "a clean run must not warn: {stderr}"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert!(
        parsed["skipped"]
            .as_array()
            .expect("skipped is present even when empty — absent would mean \"old build\"")
            .is_empty(),
        "nothing was unreadable, so skipped must be empty: {}",
        parsed["skipped"]
    );
    assert_eq!(parsed["items"].as_array().unwrap().len(), 2);
}

#[test]
fn cli_list_sort_messages_orders_descending() {
    let tmp = TempDir::new().unwrap();
    let simple_id = setup_cc_fixture(&tmp, "cc_simple");
    let complex_id =
        setup_cc_fixture_custom(&tmp, "cc_complex", Some("/data/projects/myapp"), None);

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--workspace",
            "/data/projects/myapp",
            "--sort",
            "messages",
            "--limit",
            "2",
        ])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = parsed["items"]
        .as_array()
        .expect("list --json items should be an array");

    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions[0]["session_id"].as_str(),
        Some(complex_id.as_str())
    );
    assert_eq!(sessions[1]["session_id"].as_str(), Some(simple_id.as_str()));
}

// ---------------------------------------------------------------------------
// Info command
// ---------------------------------------------------------------------------

#[test]
fn cli_info_shows_session_details() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");
    casr_cmd(&tmp)
        .args(["info", &session_id])
        .assert()
        .success()
        .stdout(predicate::str::contains(&session_id))
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("Messages:"));
}

#[test]
fn cli_info_json_is_valid() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");
    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id])
        .output()
        .expect("info should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json should emit valid JSON");
    assert_eq!(parsed["session_id"].as_str().unwrap(), session_id);
    assert_eq!(parsed["provider"].as_str().unwrap(), "claude-code");
}

#[test]
fn cli_info_unknown_session_fails() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["info", "nonexistent-session-id"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
}

#[test]
fn cli_info_unknown_session_json_error() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "info", "nonexistent-session-id"])
        .output()
        .expect("info should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("JSON error should be valid JSON");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error_type"].as_str().is_some());
}

// ---------------------------------------------------------------------------
// Resume command
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_dry_run_does_not_write() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    // Resume CC→Codex with dry run.
    casr_cmd(&tmp)
        .args(["resume", "cod", &session_id, "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would convert"));

    // Verify no Codex session files were written.
    let codex_sessions = tmp.path().join("codex/sessions");
    if codex_sessions.exists() {
        let entries: Vec<_> = walkdir::WalkDir::new(&codex_sessions)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert!(
            entries.is_empty(),
            "Dry run should not write any files, but found: {:?}",
            entries
        );
    }
}

#[test]
fn cli_resume_writes_target_session() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    // Resume CC→Codex (actual write).
    casr_cmd(&tmp)
        .args(["resume", "cod", &session_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("Resume:"));

    // Verify a Codex session file was written.
    let codex_sessions = tmp.path().join("codex/sessions");
    assert!(
        codex_sessions.exists(),
        "Codex sessions dir should exist after conversion"
    );
    let files: Vec<_> = walkdir::WalkDir::new(&codex_sessions)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "Exactly one Codex session file should be written"
    );
}

#[test]
fn cli_resume_json_output_is_valid() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--dry-run"])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("resume --json should emit valid JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source_provider"].as_str().unwrap(), "claude-code");
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "codex");
    assert_eq!(parsed["dry_run"], true);
}

#[test]
fn cli_resume_unknown_target_fails() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    casr_cmd(&tmp)
        .args(["resume", "nonexistent", &session_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
}

#[test]
fn cli_resume_unknown_session_fails() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["resume", "cod", "nonexistent-session"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
}

#[test]
fn cli_resume_cc_to_gemini_works() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    casr_cmd(&tmp)
        .args(["resume", "gmi", &session_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("gemini"));

    // Verify Gemini file was written.
    let gemini_tmp = tmp.path().join("gemini/tmp");
    assert!(gemini_tmp.exists());
    let files: Vec<_> = walkdir::WalkDir::new(&gemini_tmp)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "json")
        })
        .collect();
    assert_eq!(
        files.len(),
        1,
        "Exactly one Gemini session file should be written"
    );
}

#[test]
fn cli_resume_shorthand_cod_flag_works() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    casr_cmd(&tmp)
        .args(["-cod", &session_id, "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would convert"))
        .stdout(predicate::str::contains("codex"));
}

#[test]
fn cli_resume_shorthand_cc_flag_works() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    casr_cmd(&tmp)
        .args(["-cc", &session_id, "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would convert"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_shorthand_gmi_flag_works_in_json_mode() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "-gmi", &session_id, "--dry-run"])
        .output()
        .expect("shorthand -gmi should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("shorthand -gmi should emit valid JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source_provider"].as_str().unwrap(), "claude-code");
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "gemini");
    assert_eq!(parsed["dry_run"], true);
}

#[test]
fn cli_resume_standard_name_claude_target_works() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    casr_cmd(&tmp)
        .args(["resume", "claude", &session_id, "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would convert"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_source_standard_name_claude_works() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    casr_cmd(&tmp)
        .args([
            "resume",
            "codex",
            &session_id,
            "--source",
            "claude",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would convert"))
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("codex"));
}

#[test]
fn cli_list_defaults_to_current_workspace_and_top_10() {
    let tmp = TempDir::new().unwrap();
    let current_ws = std::env::current_dir().expect("current dir should be available");
    let current_ws_str = current_ws.display().to_string();

    // Create 12 sessions in the current workspace context.
    for i in 0..12 {
        let sid = format!("11111111-1111-4111-8111-{i:012}");
        setup_cc_fixture_custom(&tmp, "cc_simple", Some(&current_ws_str), Some(&sid));
    }

    // Create one session in another workspace; default list should exclude it.
    let out_of_scope_sid = "99999999-9999-4999-8999-999999999999";
    setup_cc_fixture_custom(
        &tmp,
        "cc_simple",
        Some("/tmp/not-current-casr-workspace"),
        Some(out_of_scope_sid),
    );

    // Default list: current workspace + top 10 recent.
    let output = casr_cmd(&tmp)
        .args(["--json", "list"])
        .output()
        .expect("list should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list --json should emit valid JSON");
    let arr = parsed["items"]
        .as_array()
        .expect("list --json items should be an array");

    assert_eq!(
        arr.len(),
        10,
        "default list should return the top 10 sessions"
    );
    for entry in arr {
        let ws = entry["workspace"].as_str().unwrap_or("");
        assert!(
            ws.starts_with(&current_ws_str),
            "default list should stay scoped to current workspace, got workspace={ws}"
        );
        let sid = entry["session_id"].as_str().unwrap_or("");
        assert_ne!(
            sid, out_of_scope_sid,
            "out-of-scope workspace session should not be included by default"
        );
    }

    // Explicit workspace override should include the out-of-scope session.
    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--workspace",
            "/tmp/not-current-casr-workspace",
        ])
        .output()
        .expect("list --workspace should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list --workspace --json should emit valid JSON");
    let arr = parsed["items"]
        .as_array()
        .expect("list --workspace --json items should be an array");
    assert!(
        arr.iter()
            .any(|e| e["session_id"].as_str() == Some(out_of_scope_sid)),
        "explicit workspace override should include out-of-scope fixture"
    );
}

#[test]
fn cli_list_provider_filter_accepts_claude_standard_name() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--provider",
            "claude",
            "--workspace",
            "/data/projects/myapp",
        ])
        .output()
        .expect("list --provider claude should run");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("list --json should emit valid JSON");
    let arr = parsed["items"]
        .as_array()
        .expect("list --json items should be an array");
    assert!(!arr.is_empty(), "should find at least one Claude session");
    assert!(
        arr.iter()
            .all(|entry| entry["provider"].as_str() == Some("claude-code")),
        "provider filter should only return Claude sessions"
    );
}

#[test]
fn cli_resume_cc_to_cursor_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cur", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→Cursor conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "cursor");
    let cursor_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    let cursor_db = tmp.path().join("cursor/User/globalStorage/state.vscdb");
    assert!(
        cursor_db.exists(),
        "Cursor DB should exist after CC→Cursor conversion"
    );

    casr_cmd(&tmp)
        .args(["--json", "info", cursor_session_id])
        .assert()
        .success();
}

#[test]
fn cli_resume_cursor_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let cursor_result = casr_cmd(&tmp)
        .args(["--json", "resume", "cur", &source_id])
        .output()
        .expect("CC→Cursor seed conversion should run");
    assert!(cursor_result.status.success());
    let cursor_json: serde_json::Value =
        serde_json::from_slice(&cursor_result.stdout).expect("seed conversion JSON should parse");
    let cursor_session_id = cursor_json["target_session_id"]
        .as_str()
        .expect("cursor target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", cursor_session_id, "--source", "cur"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("cursor"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_cc_to_cline_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cln", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→Cline conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "cline");
    let cline_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    let cline_api = tmp
        .path()
        .join("cline/tasks")
        .join(cline_session_id)
        .join("api_conversation_history.json");
    assert!(
        cline_api.exists(),
        "Cline task API history should exist after CC→Cline conversion"
    );

    casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cc",
            cline_session_id,
            "--source",
            "cln",
            "--dry-run",
        ])
        .assert()
        .success();
}

#[test]
fn cli_resume_cline_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let cline_result = casr_cmd(&tmp)
        .args(["--json", "resume", "cln", &source_id])
        .output()
        .expect("CC→Cline seed conversion should run");
    assert!(cline_result.status.success());
    let cline_json: serde_json::Value =
        serde_json::from_slice(&cline_result.stdout).expect("seed conversion JSON should parse");
    let cline_session_id = cline_json["target_session_id"]
        .as_str()
        .expect("cline target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", cline_session_id, "--source", "cln"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("cline"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_cc_to_amp_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "amp", &session_id])
        .output()
        .expect("resume should run");
    assert!(output.status.success(), "CC→Amp conversion should succeed");

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "amp");
    let amp_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    let amp_thread = tmp
        .path()
        .join("xdg-data/amp/threads")
        .join(format!("{amp_session_id}.json"));
    assert!(
        amp_thread.exists(),
        "Amp thread file should exist after CC→Amp conversion"
    );

    casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cc",
            amp_session_id,
            "--source",
            "amp",
            "--dry-run",
        ])
        .assert()
        .success();
}

#[test]
fn cli_resume_amp_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let amp_result = casr_cmd(&tmp)
        .args(["--json", "resume", "amp", &source_id])
        .output()
        .expect("CC→Amp seed conversion should run");
    assert!(amp_result.status.success());
    let amp_json: serde_json::Value =
        serde_json::from_slice(&amp_result.stdout).expect("seed conversion JSON should parse");
    let amp_session_id = amp_json["target_session_id"]
        .as_str()
        .expect("amp target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", amp_session_id, "--source", "amp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("amp"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_cc_to_aider_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "aid", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→Aider conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "aider");
    let aider_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    let aider_history = tmp.path().join("aider/.aider.chat.history.md");
    assert!(
        aider_history.exists(),
        "Aider history file should exist after CC→Aider conversion"
    );

    casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cc",
            aider_session_id,
            "--source",
            "aid",
            "--dry-run",
        ])
        .assert()
        .success();
}

#[test]
fn cli_resume_aider_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let aider_result = casr_cmd(&tmp)
        .args(["--json", "resume", "aid", &source_id])
        .output()
        .expect("CC→Aider seed conversion should run");
    assert!(aider_result.status.success());
    let aider_json: serde_json::Value =
        serde_json::from_slice(&aider_result.stdout).expect("seed conversion JSON should parse");
    let aider_session_id = aider_json["target_session_id"]
        .as_str()
        .expect("aider target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", aider_session_id, "--source", "aid"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("aider"))
        .stdout(predicate::str::contains("claude-code"));
}

#[test]
fn cli_resume_cc_to_opencode_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "opc", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→OpenCode conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "opencode");
    let opencode_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    let opencode_db = tmp.path().join("opencode/opencode.db");
    assert!(
        opencode_db.exists(),
        "OpenCode DB should exist after CC→OpenCode conversion"
    );

    // The converted OpenCode session now carries a STABLE id derived from the
    // source session (the #14 fix), so it shares the source CC session's id and
    // exists under both providers. A bare lookup is therefore ambiguous...
    casr_cmd(&tmp)
        .args(["--json", "info", opencode_session_id])
        .assert()
        .failure();

    // ...and `--source` resolves it to the OpenCode copy specifically.
    let info = casr_cmd(&tmp)
        .args(["--json", "info", opencode_session_id, "--source", "opc"])
        .output()
        .expect("info --source should run");
    assert!(info.status.success(), "info --source opc should succeed");
    let info_json: serde_json::Value =
        serde_json::from_slice(&info.stdout).expect("info --json should parse");
    assert_eq!(
        info_json["provider"].as_str().unwrap(),
        "opencode",
        "--source opc must resolve to the OpenCode session, not the CC source"
    );
}

#[test]
fn cli_resume_opencode_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let opencode_result = casr_cmd(&tmp)
        .args(["--json", "resume", "opc", &source_id])
        .output()
        .expect("CC→OpenCode seed conversion should run");
    assert!(opencode_result.status.success());
    let opencode_json: serde_json::Value =
        serde_json::from_slice(&opencode_result.stdout).expect("seed conversion JSON should parse");
    let opencode_session_id = opencode_json["target_session_id"]
        .as_str()
        .expect("opencode target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", opencode_session_id, "--source", "opc"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("opencode"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// ChatGPT conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_chatgpt_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "gpt", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→ChatGPT conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "chatgpt");
    let gpt_session_id = parsed["target_session_id"]
        .as_str()
        .expect("target_session_id should be present for non-dry-run");

    casr_cmd(&tmp)
        .args(["--json", "info", gpt_session_id])
        .assert()
        .success();
}

#[test]
fn cli_resume_chatgpt_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let gpt_result = casr_cmd(&tmp)
        .args(["--json", "resume", "gpt", &source_id])
        .output()
        .expect("CC→ChatGPT seed conversion should run");
    assert!(gpt_result.status.success());
    let gpt_json: serde_json::Value =
        serde_json::from_slice(&gpt_result.stdout).expect("seed conversion JSON should parse");
    let gpt_session_id = gpt_json["target_session_id"]
        .as_str()
        .expect("chatgpt target_session_id should be present");

    casr_cmd(&tmp)
        .args(["resume", "cc", gpt_session_id, "--source", "gpt"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("chatgpt"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// ClawdBot conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_clawdbot_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cwb", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→ClawdBot conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "clawdbot");
    assert!(parsed["target_session_id"].as_str().is_some());

    // Verify written file exists on disk.
    let written_paths = parsed["written_paths"].as_array().unwrap();
    assert!(!written_paths.is_empty(), "should have written paths");
    let path = std::path::Path::new(written_paths[0].as_str().unwrap());
    assert!(path.exists(), "ClawdBot output file should exist on disk");
}

#[test]
fn cli_resume_clawdbot_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let cwb_result = casr_cmd(&tmp)
        .args(["--json", "resume", "cwb", &source_id])
        .output()
        .expect("CC→ClawdBot seed conversion should run");
    assert!(cwb_result.status.success());
    let cwb_json: serde_json::Value =
        serde_json::from_slice(&cwb_result.stdout).expect("seed conversion JSON should parse");
    let cwb_session_id = cwb_json["target_session_id"]
        .as_str()
        .expect("clawdbot target_session_id should be present");

    // Use --force since the session ID may match the source CC session.
    casr_cmd(&tmp)
        .args(["resume", "cc", cwb_session_id, "--source", "cwb", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("clawdbot"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// Vibe conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_vibe_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "vib", &session_id])
        .output()
        .expect("resume should run");
    assert!(output.status.success(), "CC→Vibe conversion should succeed");

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "vibe");
    assert!(parsed["target_session_id"].as_str().is_some());

    // Verify written file exists on disk.
    let written_paths = parsed["written_paths"].as_array().unwrap();
    assert!(!written_paths.is_empty(), "should have written paths");
    let path = std::path::Path::new(written_paths[0].as_str().unwrap());
    assert!(path.exists(), "Vibe output file should exist on disk");
}

#[test]
fn cli_resume_vibe_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let vibe_result = casr_cmd(&tmp)
        .args(["--json", "resume", "vib", &source_id])
        .output()
        .expect("CC→Vibe seed conversion should run");
    assert!(vibe_result.status.success());
    let vibe_json: serde_json::Value =
        serde_json::from_slice(&vibe_result.stdout).expect("seed conversion JSON should parse");
    let vibe_session_id = vibe_json["target_session_id"]
        .as_str()
        .expect("vibe target_session_id should be present");

    // Use --force since the session ID may match the source CC session.
    casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            vibe_session_id,
            "--source",
            "vib",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("vibe"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// Factory conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_factory_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "fac", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→Factory conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "factory");
    assert!(parsed["target_session_id"].as_str().is_some());

    // Verify written file exists on disk.
    let written_paths = parsed["written_paths"].as_array().unwrap();
    assert!(!written_paths.is_empty(), "should have written paths");
    let path = std::path::Path::new(written_paths[0].as_str().unwrap());
    assert!(path.exists(), "Factory output file should exist on disk");
}

#[test]
fn cli_resume_factory_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let factory_result = casr_cmd(&tmp)
        .args(["--json", "resume", "fac", &source_id])
        .output()
        .expect("CC→Factory seed conversion should run");
    assert!(factory_result.status.success());
    let factory_json: serde_json::Value =
        serde_json::from_slice(&factory_result.stdout).expect("seed conversion JSON should parse");
    let factory_session_id = factory_json["target_session_id"]
        .as_str()
        .expect("factory target_session_id should be present");

    // Use --force since the session ID may match the source CC session.
    casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            factory_session_id,
            "--source",
            "fac",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("factory"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// OpenClaw conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_openclaw_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "ocl", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→OpenClaw conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "openclaw");
    assert!(parsed["target_session_id"].as_str().is_some());

    // Verify written file exists on disk.
    let written_paths = parsed["written_paths"].as_array().unwrap();
    assert!(!written_paths.is_empty(), "should have written paths");
    let path = std::path::Path::new(written_paths[0].as_str().unwrap());
    assert!(path.exists(), "OpenClaw output file should exist on disk");
}

#[test]
fn cli_resume_openclaw_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let openclaw_result = casr_cmd(&tmp)
        .args(["--json", "resume", "ocl", &source_id])
        .output()
        .expect("CC→OpenClaw seed conversion should run");
    assert!(openclaw_result.status.success());
    let openclaw_json: serde_json::Value =
        serde_json::from_slice(&openclaw_result.stdout).expect("seed conversion JSON should parse");
    let openclaw_session_id = openclaw_json["target_session_id"]
        .as_str()
        .expect("openclaw target_session_id should be present");

    // Use --force since the session ID may match the source CC session.
    casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            openclaw_session_id,
            "--source",
            "ocl",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("openclaw"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// Pi-Agent conversions
// ---------------------------------------------------------------------------

#[test]
fn cli_resume_cc_to_piagent_works_and_is_discoverable() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "pi", &session_id])
        .output()
        .expect("resume should run");
    assert!(
        output.status.success(),
        "CC→PiAgent conversion should succeed"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resume --json output should parse");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "pi-agent");
    assert!(parsed["target_session_id"].as_str().is_some());

    // Verify written file exists on disk.
    let written_paths = parsed["written_paths"].as_array().unwrap();
    assert!(!written_paths.is_empty(), "should have written paths");
    let path = std::path::Path::new(written_paths[0].as_str().unwrap());
    assert!(path.exists(), "PiAgent output file should exist on disk");
}

#[test]
fn cli_resume_piagent_to_cc_works_with_source_hint() {
    let tmp = TempDir::new().unwrap();
    let source_id = setup_cc_fixture(&tmp, "cc_simple");

    let piagent_result = casr_cmd(&tmp)
        .args(["--json", "resume", "pi", &source_id])
        .output()
        .expect("CC→PiAgent seed conversion should run");
    assert!(piagent_result.status.success());
    let piagent_json: serde_json::Value =
        serde_json::from_slice(&piagent_result.stdout).expect("seed conversion JSON should parse");
    let piagent_session_id = piagent_json["target_session_id"]
        .as_str()
        .expect("piagent target_session_id should be present");

    // Use --force since the session ID may match the source CC session.
    casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            piagent_session_id,
            "--source",
            "pi",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Converted"))
        .stdout(predicate::str::contains("pi-agent"))
        .stdout(predicate::str::contains("claude-code"));
}

// ---------------------------------------------------------------------------
// Launching the target agent
//
// Every test here goes through `--launch-dry-run`, or through `--launch` with
// `PATH` emptied, so that a regression cannot start a real `claude` or `codex`
// and leave an interactive agent attached to the test runner's terminal.
// ---------------------------------------------------------------------------

use casr::discovery::ProviderRegistry;
use casr::launch::LaunchSpec;

/// A Codex rollout whose live context is a sealed compaction.
///
/// Written inline rather than committed as a fixture: the point of the case is
/// the `encrypted_content` blob, and a corpus file is the wrong place for
/// material that only looks like provider-sealed state. The compaction is not
/// the last line, because Codex's flat reader resets history to
/// `replacement_history` and a session ending there has no messages left to
/// convert — the refusal under test would never be reached.
const CODEX_COMPACTED_ROLLOUT: &str = concat!(
    r#"{"type":"session_meta","timestamp":1737100000.0,"payload":{"id":"codex-compacted-001","cwd":"/data/projects/backend","model_provider":"openai"}}"#,
    "\n",
    r#"{"type":"event_msg","timestamp":1737100001.0,"payload":{"type":"user_message","message":"Optimize the database query in users.rs"}}"#,
    "\n",
    r#"{"type":"response_item","timestamp":1737100010.0,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Indexing the lookup now."}]}}"#,
    "\n",
    r#"{"type":"compacted","timestamp":1737100020.0,"payload":{"window_id":"w2","previous_window_id":"w1","message":"Here is the summary produced by the other language model.","replacement_history":[{"type":"compaction","id":"cmp_test_001","encrypted_content":"c2VhbGVkLWNvbXBhY3RlZC1oaXN0b3J5"}]}}"#,
    "\n",
    r#"{"type":"event_msg","timestamp":1737100030.0,"payload":{"type":"user_message","message":"Now add connection pooling"}}"#,
    "\n",
    r#"{"type":"response_item","timestamp":1737100040.0,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Added an r2d2 pool."}]}}"#,
    "\n",
);

/// Place a literal Codex rollout where discovery will find it.
fn write_codex_rollout(tmp: &TempDir, session_id: &str, lines: &str) -> String {
    let dir = tmp.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&dir).expect("create Codex sessions dir");
    fs::write(
        dir.join(format!("rollout-2026-01-01T00-00-00-{session_id}.jsonl")),
        lines,
    )
    .expect("write Codex rollout");
    session_id.to_string()
}

/// The last non-empty line, which is where the launcher prints its command.
fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// The value of a `  Label → value` line in the human-readable output.
fn labelled(stdout: &str, label: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.split_once(label))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_else(|| panic!("no {label:?} line in:\n{stdout}"))
}

/// An empty directory, so no agent binary can be resolved from `PATH`.
///
/// Used by the tests that exercise the real `--launch` path: the assertions
/// are about what happens *before* the exec, and an accidental exec of the
/// developer's own `claude` would hang the suite rather than fail it.
fn empty_path(tmp: &TempDir) -> PathBuf {
    let dir = tmp.path().join("empty-bin");
    fs::create_dir_all(&dir).expect("create empty PATH dir");
    dir
}

#[test]
fn cli_launch_dry_run_prints_a_command_that_parses_back_to_the_spec() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["resume", "cod", &session_id, "--launch-dry-run"])
        .output()
        .expect("launch dry run should run");
    assert!(output.status.success(), "a dry-run launch must exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let target_id = labelled(&stdout, "Target →");

    // The whole point of the structured spec: what is shown must parse back to
    // what would be executed, program and arguments both.
    let printed = LaunchSpec::from_command_line(last_line(&stdout))
        .expect("the printed command must be a splittable command line");
    let expected = ProviderRegistry::default_registry()
        .find_by_alias("cod")
        .expect("codex in registry")
        .launch_spec(&target_id)
        .expect("codex spec");
    assert_eq!(
        (printed.program, printed.args),
        (expected.program, expected.args),
        "the printed command and the spec disagree"
    );
}

#[test]
fn cli_launch_dry_run_appends_passthrough_flags() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args([
            "resume",
            "cod",
            &session_id,
            "--launch-dry-run",
            "--",
            "--model",
            "o3",
            "--search",
        ])
        .output()
        .expect("launch dry run should run");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = last_line(&stdout);
    let target_id = labelled(&stdout, "Target →");
    assert_eq!(
        command,
        format!("codex resume {target_id} --model o3 --search"),
        "user flags belong after the resume arguments, unmodified"
    );
}

#[test]
fn cli_launch_refuses_a_passthrough_flag_that_would_retarget_the_session() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    // The real `--launch` path, not the dry run: the claim is that the conflict
    // is caught *before* anything is started. `PATH` is empty so a regression
    // that got as far as spawning would fail on the program instead.
    let output = casr_cmd(&tmp)
        .env("PATH", empty_path(&tmp))
        .args([
            "resume",
            "cc",
            &session_id,
            "--launch",
            "--",
            "--resume",
            "a-different-session",
        ])
        .output()
        .expect("launch should run");

    assert!(
        !output.status.success(),
        "re-specifying --resume must not be silently accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already set by the resume command"),
        "expected the conflict message, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("is not installed"),
        "the conflict must be reported before the launch is attempted, got:\n{stderr}"
    );
}

#[test]
fn cli_launch_refuses_a_conversion_that_lost_part_of_the_conversation() {
    let tmp = TempDir::new().unwrap();
    let session_id = write_codex_rollout(&tmp, "codex-compacted-001", CODEX_COMPACTED_ROLLOUT);

    let output = casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            &session_id,
            "--source",
            "cod",
            "--launch-dry-run",
        ])
        .output()
        .expect("launch dry run should run");

    assert!(
        !output.status.success(),
        "launching into a session with a hole in its history must not be the default"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Written →"),
        "the conversion itself succeeded and must still say where it wrote:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("compacted-history capsule")
            && stderr.contains("missing history")
            && stderr.contains("--launch-anyway"),
        "the refusal must say what is missing and how to override it, got:\n{stderr}"
    );

    // And the override is an override, not a suggestion.
    let output = casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            &session_id,
            "--source",
            "cod",
            "--force",
            "--launch-dry-run",
            "--launch-anyway",
        ])
        .output()
        .expect("launch dry run should run");
    assert!(
        output.status.success(),
        "--launch-anyway must let the launch through: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        last_line(&stdout).starts_with("claude --resume "),
        "expected the claude command, got:\n{stdout}"
    );
}

#[test]
fn cli_launch_refusal_keeps_the_json_envelope_parseable() {
    let tmp = TempDir::new().unwrap();
    let session_id = write_codex_rollout(&tmp, "codex-compacted-002", CODEX_COMPACTED_ROLLOUT);

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cc",
            &session_id,
            "--source",
            "cod",
            "--launch-dry-run",
        ])
        .output()
        .expect("launch dry run should run");

    assert!(!output.status.success(), "the refusal still fails the run");

    // The conversion happened, so its envelope is on stdout and is still the
    // only thing there — the refusal is an error, and errors have their own
    // envelope on stderr.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must stay parseable JSON: {e}\nOutput: {stdout}"));
    // `ok` follows the exit code. It used to be `true` on a run that exited
    // non-zero, so a script reading only stdout was told a refused launch had
    // succeeded; the reason for the refusal is in the envelope too, so nothing
    // has to be recovered by parsing stderr.
    assert_eq!(parsed["ok"], false);
    assert!(
        parsed["launch_error"]
            .as_str()
            .is_some_and(|e| e.contains("refusing to launch")),
        "the envelope has to carry why: {parsed}"
    );
    assert_eq!(
        parsed["fidelity"], "history_incomplete",
        "the grade a script would gate on must be in the envelope"
    );
    assert!(parsed["written_paths"].as_array().is_some_and(|p| !p.is_empty()));
    assert!(
        parsed["losses"]
            .as_array()
            .is_some_and(|losses| losses.iter().any(|loss| loss["kind"] == "sealed_context"
                && loss["capsules"].as_u64().is_some_and(|n| n > 0))),
        "the counts behind the grade have to reach a machine consumer: {parsed}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr must be an error envelope: {e}\nOutput: {stderr}"));
    assert_eq!(error["ok"], false);
    let message = error["message"].as_str().expect("message string");
    assert!(
        message.contains("compacted-history capsule")
            && message.contains("missing history")
            && message.contains("--launch-anyway"),
        "the refusal must survive into JSON mode intact: {message}"
    );
}

#[test]
fn cli_launch_warns_when_the_agent_cannot_be_pointed_at_the_session() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["resume", "cur", &session_id, "--launch-dry-run"])
        .output()
        .expect("launch dry run should run");
    assert!(
        output.status.success(),
        "an untargetable provider is a warning, not a refusal"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no way to be pointed at a specific session")
            && stdout.contains("without resuming the converted one"),
        "Cursor cannot open the converted session and the user has to be told:\n{stdout}"
    );
    let written = labelled(&stdout, "Written →");
    assert!(
        stdout.contains(&format!("Converted session: {written}")),
        "the warning must name the file so the user can open it themselves:\n{stdout}"
    );
    assert_eq!(
        last_line(&stdout),
        "cursor .",
        "the command is still printed; it just will not resume anything"
    );
}

#[test]
fn cli_launch_reports_a_missing_agent_as_missing_rather_than_as_a_failed_conversion() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .env("PATH", empty_path(&tmp))
        .args(["resume", "cod", &session_id, "--launch"])
        .output()
        .expect("launch should run");

    assert!(!output.status.success(), "there is nothing to start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Converted") && stdout.contains("Written →"),
        "the conversion succeeded and the written path is the useful part:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not installed") && stderr.contains("not on PATH"),
        "expected a missing-agent report, got:\n{stderr}"
    );
}

#[test]
fn cli_launch_flags_are_refused_where_they_would_do_nothing() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    // Silently ignoring these is the failure mode worth preventing: the user
    // asked for a launch and would get a conversion.
    for extra in [
        vec!["--launch-anyway"],
        vec!["--launch", "--launch-dry-run"],
        vec!["--launch", "--dry-run"],
    ] {
        let mut cmd = casr_cmd(&tmp);
        cmd.args(["resume", "cod", &session_id]).args(&extra);
        cmd.assert()
            .failure()
            .stderr(predicate::str::contains("error:"));
    }
}

// ---------------------------------------------------------------------------
// Completions command
// ---------------------------------------------------------------------------

#[test]
fn cli_completions_bash() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("casr"));
}

#[test]
fn cli_completions_invalid_shell() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["completions", "ksh"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// Verbose / trace flags
// ---------------------------------------------------------------------------

#[test]
fn cli_verbose_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["--verbose", "providers"])
        .assert()
        .success();
}

#[test]
fn cli_trace_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    casr_cmd(&tmp)
        .args(["--trace", "providers"])
        .assert()
        .success();
}

#[test]
fn cli_verbose_emits_debug_logs() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");
    casr_cmd(&tmp)
        .args(["--verbose", "resume", "cod", &session_id, "--dry-run"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("DEBUG")
                .and(predicate::str::contains("source session resolved")),
        );
}

#[test]
fn cli_trace_emits_trace_logs() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");
    casr_cmd(&tmp)
        .args(["--trace", "resume", "cod", &session_id, "--dry-run"])
        .assert()
        .success()
        .stderr(predicate::str::contains("TRACE").and(predicate::str::contains("searching")));
}

// ---------------------------------------------------------------------------
// Context budget on the structured track
// ---------------------------------------------------------------------------

/// The whole chain under a binding budget: writer trims, grade follows the loss
/// list, and the structural read-back verifier does not mistake the trim for
/// damage.
///
/// That last part is the reason this is an end-to-end test and not a writer
/// test. `verify_structured_write` compares the source's replay against the file
/// it wrote; if it compared the *unbudgeted* replay, every dropped turn would
/// land in the comparator's `unexplained` bucket, and the conversion would roll
/// itself back and fail as a writer bug. A budget that cannot be used is not a
/// budget.
#[test]
fn cli_resume_structured_track_honours_the_context_budget() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_complex");

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cod",
            &session_id,
            // Small enough that only the newest turn survives.
            "--max-context-tokens",
            "30",
        ])
        .output()
        .expect("resume should run");

    assert!(
        output.status.success(),
        "a trimmed structured write must not fail verification: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("resume --json should emit valid JSON");
    let warnings = parsed["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .filter_map(|warning| warning.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        warnings.contains("context budget"),
        "the trim has to be visible: {warnings}"
    );
    assert_eq!(
        parsed["fidelity"], "history_incomplete",
        "dropping conversation to fit a cap is a hole, and the grade says so"
    );

    // And the file that landed is the trimmed one, not the whole session.
    let written = parsed["written_paths"][0].as_str().expect("a written path");
    let lines = fs::read_to_string(written)
        .expect("the rollout is on disk")
        .lines()
        .filter(|line| line.contains("response_item"))
        .count();
    assert_eq!(lines, 1, "only the newest turn was in budget");
}

/// The same conversion with no budget flags at all, as the contrast.
///
/// Naming no flag is the whole test. The caps shipped as defaults —
/// `--max-context-tokens 200000 --max-tool-output 4000`, with reasoning dropped
/// unless `--keep-reasoning` was passed — so a plain `resume` trimmed every
/// conversion it ran. Over the local corpus that removed something from 815 of
/// 832 sessions and deleted conversation outright from 147 of them, graded
/// `HistoryIncomplete`, which is the grade `--launch` refuses on. A converter
/// whose product is fidelity does not trim a session nobody asked it to trim,
/// so this asserts the untouched command carries the whole replay.
#[test]
fn cli_resume_structured_track_without_flags_carries_everything() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_complex");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("resume --json should emit valid JSON");
    let warnings = parsed["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .filter_map(|warning| warning.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !warnings.contains("context budget"),
        "an absent budget removes nothing and therefore reports nothing: {warnings}"
    );
    let written = parsed["written_paths"][0].as_str().expect("a written path");
    let lines = fs::read_to_string(written)
        .expect("the rollout is on disk")
        .lines()
        .filter(|line| line.contains("response_item"))
        .count();
    assert!(lines > 1, "the whole replay crossed, {lines} events");
}

/// The flat track's half of the same promise, both ways round.
///
/// The nineteen providers with no structured writer are budgeted in the pipeline
/// rather than in a writer, from the same [`ContextBudget`] — so "off unless
/// asked" has to hold on that path too, and asking still has to work. Measured
/// over the local corpus the shipped `--max-tool-output 4000` default elided
/// 11,206 tool observations across 747 of 833 sessions on this track alone,
/// which is what makes the first half of this test worth having.
#[test]
fn cli_resume_flat_track_budget_is_opt_in() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_complex");

    let plain = casr_cmd(&tmp)
        .args(["--json", "resume", "gmi", &session_id, "--no-store"])
        .output()
        .expect("resume should run");
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&plain.stdout))
        .expect("resume --json should emit valid JSON");
    assert_eq!(
        plain["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .filter(|warning| {
                let warning = warning.as_str().unwrap_or_default();
                warning.contains("Truncated") || warning.contains("Context budget")
            })
            .count(),
        0,
        "no flag was given, so nothing may be trimmed: {}",
        plain["warnings"]
    );

    let asked = casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "gmi",
            &session_id,
            "--no-store",
            "--force",
            "--max-tool-output",
            "20",
        ])
        .output()
        .expect("resume should run");
    assert!(
        asked.status.success(),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    let asked: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&asked.stdout))
        .expect("resume --json should emit valid JSON");
    let losses = asked["losses"].as_array().expect("losses array");
    assert!(
        losses.iter().any(|loss| {
            loss["kind"] == "tool_protocol"
                && loss["events"].as_u64().unwrap_or(0) > 0
                && loss["grade"] == "conversation_only"
        }),
        "and when it is given, the trim is still counted as a loss: {asked}"
    );
}

/// Reasoning survives unless someone writes down that it should not.
///
/// The caps were made opt-in first, and reasoning was left riding along on them
/// — so `--max-tool-output`, a command line about oversized tool output, still
/// deleted every reasoning event in the session. Over the local corpus that was
/// 35,303 events and 88.6 MB of sealed capsules, removed because a flag was
/// *absent*, which is the same defect one flag over. The inverted sense had to
/// go with it: an absent `--keep-reasoning` cannot mean "delete".
///
/// `--keep-reasoning` stays accepted so that an existing casr command line does
/// not start erroring, and it now names the default. Passing both is a genuine
/// contradiction and is refused rather than resolved by picking one.
#[test]
fn cli_resume_reasoning_is_kept_unless_dropping_is_asked_for() {
    /// The reasoning losses a conversion reported, by their note.
    fn reasoning_notes(args: &[&str]) -> Vec<String> {
        let tmp = TempDir::new().unwrap();
        let session_id = setup_codex_fixture(&tmp, "codex_reasoning", "jsonl");
        let mut argv: Vec<String> = vec!["--json".into(), "resume".into(), "cc".into(), session_id];
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let output = casr_cmd(&tmp)
            .args(&argv)
            .output()
            .expect("resume should run");
        assert!(
            output.status.success(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
                .expect("resume --json should emit valid JSON");
        parsed["losses"]
            .as_array()
            .expect("losses array")
            .iter()
            .filter(|loss| loss["kind"] == "reasoning")
            .map(|loss| loss["note"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    assert!(
        reasoning_notes(&[]).is_empty(),
        "nothing was asked for, so nothing is dropped"
    );
    assert!(
        reasoning_notes(&["--max-tool-output", "20"]).is_empty(),
        "a cap on tool output is not a request to delete reasoning"
    );
    assert!(
        reasoning_notes(&["--max-context-tokens", "100000"]).is_empty(),
        "nor is a token cap this session fits inside"
    );
    assert!(
        reasoning_notes(&["--keep-reasoning"]).is_empty(),
        "the compatibility flag asks for the default and still gets it"
    );

    let dropped = reasoning_notes(&["--drop-reasoning"]);
    assert_eq!(dropped.len(), 1, "asking removes it: {dropped:?}");
    assert!(
        dropped[0].contains("--drop-reasoning"),
        "and the note names the request rather than advising a flag that would do \
         nothing: {dropped:?}"
    );

    // Contradictory, so refused — clap's own usage error, exit code 2.
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_reasoning", "jsonl");
    let both = casr_cmd(&tmp)
        .args([
            "resume",
            "cc",
            &session_id,
            "--keep-reasoning",
            "--drop-reasoning",
        ])
        .output()
        .expect("resume should run");
    assert!(
        !both.status.success(),
        "keeping and dropping reasoning at once is not a request that can be honoured"
    );
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("--drop-reasoning"),
        "and the refusal has to name the pair: {}",
        String::from_utf8_lossy(&both.stderr)
    );
}

// ---------------------------------------------------------------------------
// One invocation, one machine-readable answer
// ---------------------------------------------------------------------------

/// A launch that cannot even be prepared must not be reported as a success.
///
/// The envelope is printed before the launch is attempted, so the failure has to
/// be decided before the printing. It was not: stdout said `{"ok": true}` and
/// stderr said `{"ok": false}` for the same run.
#[test]
fn cli_json_launch_preparation_failure_is_one_coherent_object() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .env("PATH", empty_path(&tmp))
        .args([
            "--json",
            "resume",
            "cc",
            &session_id,
            "--launch",
            "--",
            "--resume",
            "a-different-session",
        ])
        .output()
        .expect("launch should run");

    assert!(!output.status.success(), "the run failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be parseable JSON: {e}\nOutput: {stdout}"));
    assert_eq!(
        parsed["ok"], false,
        "ok must follow the exit code, not the conversion alone: {parsed}"
    );
    assert!(
        parsed["launch_error"]
            .as_str()
            .is_some_and(|e| e.contains("already set by the resume command")),
        "the reason belongs in the object that reports the failure: {parsed}"
    );
    // The conversion did happen, and its output is the part worth keeping.
    assert!(
        parsed["written_paths"]
            .as_array()
            .is_some_and(|paths| !paths.is_empty()),
        "a launch that could not be prepared does not un-write the session: {parsed}"
    );
}

/// A `--json` run that launches cleanly still says `ok: true`.
#[test]
fn cli_json_launch_dry_run_still_reports_success() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cc", &session_id, "--launch-dry-run"])
        .output()
        .expect("launch dry run should run");

    assert!(output.status.success(), "nothing blocked this launch");
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("stdout is JSON");
    assert_eq!(parsed["ok"], true);
    assert!(parsed["launch_error"].is_null());
    assert!(parsed["launch_command"].is_string());
}

// ---------------------------------------------------------------------------
// Resuming a store record id
// ---------------------------------------------------------------------------

/// `resume <record-id>` says which session it actually converted.
///
/// A record id is ours; no provider has heard of it, so it is translated into a
/// provider session before the pipeline runs. Nothing reported that: the store's
/// own "I read something else" line only fires when the store disagrees with the
/// pipeline, and by then the pipeline has already been handed the substituted
/// session, so it agrees. The user named one identifier and a different one was
/// converted, in silence.
#[test]
fn cli_resume_by_record_id_says_which_session_it_converted() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    // First conversion, with the store on, files this conversation as a record.
    let first = casr_cmd(&tmp)
        .args(["--json", "resume", "cc", &session_id])
        .output()
        .expect("first conversion");
    assert!(
        first.status.success(),
        "seeding conversion failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let records_dir = tmp.path().join("xdg-data/agsx/records");
    let record_id = fs::read_dir(&records_dir)
        .unwrap_or_else(|e| panic!("store records at {}: {e}", records_dir.display()))
        .filter_map(Result::ok)
        .find(|entry| entry.path().join("record.json").is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .expect("the first conversion filed a record");
    assert_ne!(
        record_id, session_id,
        "the record id is ours and is not any provider's session id"
    );

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &record_id])
        .output()
        .expect("resume by record id");
    assert!(
        output.status.success(),
        "resume by record id failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("stdout is JSON");
    let warnings = parsed["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|w| {
            let w = w.as_str().unwrap_or_default();
            w.contains("session-store record id") && w.contains(&session_id)
        }),
        "the substitution has to be reported, naming what was converted: {parsed}"
    );
}
