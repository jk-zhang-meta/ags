//! JSON contract tests for all `--json` CLI outputs.
//!
//! Validates that every `--json` subcommand emits structurally stable JSON
//! conforming to documented field names, types, and constraints.  These tests
//! act as a backward-compatibility guard: if a field is removed or its type
//! changes, the corresponding test breaks.
//!
//! Bead: bd-24z.11

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (fixture setup, command builder)
// ---------------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

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
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("NO_COLOR", "1");
    cmd
}

fn setup_cc_fixture(tmp: &TempDir, fixture_name: &str) -> String {
    let source = fixtures_dir().join(format!("claude_code/{fixture_name}.jsonl"));
    let content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    let first_line: serde_json::Value = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str(l).ok())
        .expect("fixture should have valid first line");

    let session_id = first_line["sessionId"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let cwd = first_line["cwd"].as_str().unwrap_or("/tmp");

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

fn setup_codex_fixture(tmp: &TempDir, fixture_name: &str, ext: &str) -> String {
    let source = fixtures_dir().join(format!("codex/{fixture_name}.{ext}"));
    let content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    let session_id = if ext == "jsonl" {
        content
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "session_meta")
            .and_then(|v| v["payload"]["id"].as_str().map(String::from))
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        let root: serde_json::Value = serde_json::from_str(&content).unwrap();
        root["session"]["id"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    };

    let sessions_dir = tmp.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions_dir).expect("create Codex sessions dir");

    let filename = format!("rollout-2026-01-01T00-00-00-{session_id}.{ext}");
    let target_path = sessions_dir.join(&filename);
    fs::write(&target_path, &content).expect("write Codex fixture");

    session_id
}

fn setup_gemini_fixture(tmp: &TempDir, fixture_name: &str) -> String {
    setup_gemini_fixture_custom(tmp, fixture_name, None)
}

fn setup_gemini_fixture_custom(
    tmp: &TempDir,
    fixture_name: &str,
    workspace_hint: Option<&str>,
) -> String {
    let source = fixtures_dir().join(format!("gemini/{fixture_name}.json"));
    let content = fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("Failed to read fixture {fixture_name}: {e}"));

    let mut root: serde_json::Value = serde_json::from_str(&content).unwrap();
    let session_id = root["sessionId"].as_str().unwrap_or("unknown").to_string();

    if let Some(workspace) = workspace_hint
        && let Some(messages) = root.get_mut("messages").and_then(|m| m.as_array_mut())
        && let Some(first) = messages.first_mut()
    {
        first["content"] = serde_json::Value::String(format!("Workspace: {workspace}"));
    }

    let hash_dir = tmp.path().join("gemini/tmp/testhash123/chats");
    fs::create_dir_all(&hash_dir).expect("create Gemini chats dir");

    let filename = format!("session-{session_id}.json");
    let target_path = hash_dir.join(&filename);
    fs::write(&target_path, serde_json::to_string_pretty(&root).unwrap())
        .expect("write Gemini fixture");

    session_id
}

// ---------------------------------------------------------------------------
// Type-assertion helpers
// ---------------------------------------------------------------------------

/// Assert a JSON value is a non-empty string.
fn assert_string(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_string(),
        "{ctx}: field '{field}' should be a string, got: {val}"
    );
}

/// Assert a JSON value is a string or null.
fn assert_string_or_null(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_string() || val.is_null(),
        "{ctx}: field '{field}' should be string|null, got: {val}"
    );
}

/// Assert a JSON value is a boolean.
fn assert_bool(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_boolean(),
        "{ctx}: field '{field}' should be a boolean, got: {val}"
    );
}

/// Assert a JSON value is a number (integer or float).
fn assert_number_or_null(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_number() || val.is_null(),
        "{ctx}: field '{field}' should be number|null, got: {val}"
    );
}

/// Assert a JSON value is an array.
fn assert_array(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_array(),
        "{ctx}: field '{field}' should be an array, got: {val}"
    );
}

/// Assert a JSON value is an array or null.
fn assert_array_or_null(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_array() || val.is_null(),
        "{ctx}: field '{field}' should be array|null, got: {val}"
    );
}

/// Assert a JSON value is a number (u64).
fn assert_uint(val: &serde_json::Value, field: &str, ctx: &str) {
    assert!(
        val.is_u64(),
        "{ctx}: field '{field}' should be a non-negative integer, got: {val}"
    );
}

/// Assert a JSON object contains exactly the expected keys (no extra, no missing).
fn assert_exact_keys(obj: &serde_json::Value, expected: &[&str], ctx: &str) {
    let map = obj
        .as_object()
        .unwrap_or_else(|| panic!("{ctx}: expected object"));
    let actual: std::collections::BTreeSet<&str> = map.keys().map(|k| k.as_str()).collect();
    let expect: std::collections::BTreeSet<&str> = expected.iter().copied().collect();

    let extra: Vec<&&str> = actual.difference(&expect).collect();
    let missing: Vec<&&str> = expect.difference(&actual).collect();

    assert!(
        extra.is_empty() && missing.is_empty(),
        "{ctx}: key mismatch.\n  Extra: {extra:?}\n  Missing: {missing:?}\n  Actual keys: {actual:?}"
    );
}

// ---------------------------------------------------------------------------
// Contract: `providers --json`
// ---------------------------------------------------------------------------
// Expected shape: Array of {name, slug, alias, installed, version, evidence}

fn assert_provider_object(obj: &serde_json::Value, idx: usize) {
    let ctx = format!("providers[{idx}]");
    assert_exact_keys(
        obj,
        &["name", "slug", "alias", "installed", "version", "evidence"],
        &ctx,
    );
    assert_string(&obj["name"], "name", &ctx);
    assert_string(&obj["slug"], "slug", &ctx);
    assert_string(&obj["alias"], "alias", &ctx);
    assert_bool(&obj["installed"], "installed", &ctx);
    assert_string_or_null(&obj["version"], "version", &ctx);
    assert_array(&obj["evidence"], "evidence", &ctx);

    // Evidence items are all strings.
    for (i, ev) in obj["evidence"].as_array().unwrap().iter().enumerate() {
        assert!(ev.is_string(), "{ctx}: evidence[{i}] should be a string");
    }
}

#[test]
fn contract_providers_json_shape() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "providers"])
        .output()
        .expect("providers should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from providers: {e}\nOutput: {stdout}"));

    let arr = parsed
        .as_array()
        .expect("providers --json should be an array");
    assert_eq!(
        arr.len(),
        17,
        "should list 17 providers (CC, Codex, Gemini, Antigravity, Cursor, Cline, Aider, Amp, OpenCode, ChatGPT, ClawdBot, Vibe, Factory, OpenClaw, Pi-Agent, Kiro, Grok)"
    );

    for (i, item) in arr.iter().enumerate() {
        assert_provider_object(item, i);
    }
}

#[test]
fn contract_providers_known_slugs() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "providers"])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let slugs: Vec<&str> = parsed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap())
        .collect();

    assert!(slugs.contains(&"claude-code"), "should contain claude-code");
    assert!(slugs.contains(&"codex"), "should contain codex");
    assert!(slugs.contains(&"gemini"), "should contain gemini");
    assert!(slugs.contains(&"antigravity"), "should contain antigravity");
    assert!(slugs.contains(&"cursor"), "should contain cursor");
    assert!(slugs.contains(&"cline"), "should contain cline");
    assert!(slugs.contains(&"aider"), "should contain aider");
    assert!(slugs.contains(&"amp"), "should contain amp");
    assert!(slugs.contains(&"opencode"), "should contain opencode");
    assert!(slugs.contains(&"clawdbot"), "should contain clawdbot");
    assert!(slugs.contains(&"vibe"), "should contain vibe");
    assert!(slugs.contains(&"factory"), "should contain factory");
    assert!(slugs.contains(&"openclaw"), "should contain openclaw");
    assert!(slugs.contains(&"pi-agent"), "should contain pi-agent");
    assert!(slugs.contains(&"kiro"), "should contain kiro");
    assert!(slugs.contains(&"grok"), "should contain grok");
}

#[test]
fn contract_providers_aliases_match_slugs() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "providers"])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let alias_map: Vec<(&str, &str)> = parsed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["slug"].as_str().unwrap(), p["alias"].as_str().unwrap()))
        .collect();

    // Verify known alias→slug pairings.
    for (slug, alias) in &alias_map {
        match *slug {
            "claude-code" => assert_eq!(*alias, "cc"),
            "codex" => assert_eq!(*alias, "cod"),
            "gemini" => assert_eq!(*alias, "gmi"),
            "antigravity" => assert_eq!(*alias, "agy"),
            "cursor" => assert_eq!(*alias, "cur"),
            "cline" => assert_eq!(*alias, "cln"),
            "aider" => assert_eq!(*alias, "aid"),
            "amp" => assert_eq!(*alias, "amp"),
            "opencode" => assert_eq!(*alias, "opc"),
            "chatgpt" => assert_eq!(*alias, "gpt"),
            "clawdbot" => assert_eq!(*alias, "cwb"),
            "vibe" => assert_eq!(*alias, "vib"),
            "factory" => assert_eq!(*alias, "fac"),
            "openclaw" => assert_eq!(*alias, "ocl"),
            "pi-agent" => assert_eq!(*alias, "pi"),
            "kiro" => assert_eq!(*alias, "kr"),
            "grok" => assert_eq!(*alias, "grk"),
            other => panic!("Unexpected slug: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Contract: `list --json`
// ---------------------------------------------------------------------------
// Expected shape: { schema_version: 6, items: [{ schema_version, session_id, provider, ... }],
//                   skipped: [{ provider, path, error }] }

fn assert_list_envelope(parsed: &serde_json::Value) -> &Vec<serde_json::Value> {
    let ctx = "list_envelope";
    assert_exact_keys(parsed, &["schema_version", "items", "skipped"], ctx);
    assert_uint(&parsed["schema_version"], "schema_version", ctx);
    assert_eq!(
        parsed["schema_version"].as_u64().unwrap(),
        6,
        "{ctx}: schema_version should be 6"
    );
    assert_array(&parsed["items"], "items", ctx);
    // Always an array, `[]` on a clean run: an absent key would say "this
    // build cannot tell you what it skipped", which is a different fact from
    // "nothing was skipped".
    assert_array(&parsed["skipped"], "skipped", ctx);
    for (idx, obj) in parsed["skipped"].as_array().unwrap().iter().enumerate() {
        let ctx = format!("list.skipped[{idx}]");
        assert_exact_keys(obj, &["provider", "path", "error"], &ctx);
        assert_string(&obj["provider"], "provider", &ctx);
        assert_string(&obj["path"], "path", &ctx);
        assert_string(&obj["error"], "error", &ctx);
    }
    parsed["items"].as_array().unwrap()
}

fn assert_list_item(obj: &serde_json::Value, idx: usize) {
    let ctx = format!("list[{idx}]");
    assert_exact_keys(
        obj,
        &[
            "schema_version",
            "session_id",
            "provider",
            "title",
            "native_name",
            "messages",
            "workspace",
            "started_at",
            "path",
            "avg_agent_response_chars",
            "avg_agent_response_chars_rounded",
            "file_size_bytes",
            "file_size_kb",
            "last_active_at",
            "tool_uses",
            "unique_user_messages",
            "workspace_name",
            "workspace_name_source",
        ],
        &ctx,
    );
    assert_uint(&obj["schema_version"], "schema_version", &ctx);
    assert_eq!(
        obj["schema_version"].as_u64().unwrap(),
        6,
        "{ctx}: per-item schema_version should be 6"
    );
    assert_string(&obj["session_id"], "session_id", &ctx);
    assert_string(&obj["provider"], "provider", &ctx);
    assert_string_or_null(&obj["title"], "title", &ctx);
    assert_string_or_null(&obj["native_name"], "native_name", &ctx);
    assert_uint(&obj["messages"], "messages", &ctx);
    assert_string_or_null(&obj["workspace"], "workspace", &ctx);
    assert_number_or_null(&obj["started_at"], "started_at", &ctx);
    assert_string(&obj["path"], "path", &ctx);
    assert_string_or_null(&obj["workspace_name"], "workspace_name", &ctx);
    assert_string_or_null(&obj["workspace_name_source"], "workspace_name_source", &ctx);
    // A count or `null` — never a `0` standing in for "no way to count these".
    assert_number_or_null(&obj["tool_uses"], "tool_uses", &ctx);
}

#[test]
fn contract_list_json_empty() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "list"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from list: {e}\nOutput: {stdout}"));

    let items = assert_list_envelope(&parsed);
    assert!(items.is_empty(), "empty env should yield empty items");
}

#[test]
fn contract_list_json_shape_cc() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/myapp"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from list: {e}\nOutput: {stdout}"));

    let items = assert_list_envelope(&parsed);
    assert!(!items.is_empty(), "should find at least one session");

    for (i, item) in items.iter().enumerate() {
        assert_list_item(item, i);
    }

    // First item should be from claude-code.
    assert_eq!(items[0]["provider"].as_str().unwrap(), "claude-code");
}

#[test]
fn contract_list_json_shape_codex() {
    let tmp = TempDir::new().unwrap();
    setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/backend"])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from list: {e}\nOutput: {stdout}"));

    let items = assert_list_envelope(&parsed);
    assert!(!items.is_empty(), "should find codex session");

    for (i, item) in items.iter().enumerate() {
        assert_list_item(item, i);
    }
    assert_eq!(items[0]["provider"].as_str().unwrap(), "codex");
}

#[test]
fn contract_list_json_shape_gemini() {
    let tmp = TempDir::new().unwrap();
    setup_gemini_fixture_custom(
        &tmp,
        "gmi_simple",
        Some("/data/projects/cross_agent_session_resumer"),
    );

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--workspace",
            "/data/projects/cross_agent_session_resumer",
        ])
        .output()
        .expect("list should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from list: {e}\nOutput: {stdout}"));

    let items = assert_list_envelope(&parsed);
    assert!(!items.is_empty(), "should find gemini session");

    for (i, item) in items.iter().enumerate() {
        assert_list_item(item, i);
    }
    assert_eq!(items[0]["provider"].as_str().unwrap(), "gemini");
}

#[test]
fn contract_list_json_messages_is_nonnegative() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/myapp"])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    let items = assert_list_envelope(&parsed);
    for item in items {
        let msgs = item["messages"].as_u64().unwrap();
        assert!(msgs > 0, "cc_simple fixture should have at least 1 message");
    }
}

// ---------------------------------------------------------------------------
// Contract: `info --json`
// ---------------------------------------------------------------------------
// Expected shape: {schema_version, session_id, provider, detected_format, title,
//                  native_name, workspace, messages, summary, started_at,
//                  ended_at, model_name, source_path, metadata, workspace_name,
//                  workspace_name_source}
// (transcript_tail is present only with --peek.)

/// The four `summary` keys an external consumer already parses.
///
/// Spelled here rather than derived, because the contract is the spelling.
const SUMMARY_CONTRACT_KEYS: [&str; 4] = ["message", "reasoning", "tool_call", "tool_result"];

/// Every `summary` key, contract-first then the rest of the `Body` variants.
const SUMMARY_KEYS: [&str; 13] = [
    "message",
    "reasoning",
    "tool_call",
    "tool_result",
    "compaction",
    "sealed_context",
    "turn_config",
    "env_snapshot",
    "attachment",
    "rollback",
    "abort",
    "control",
    "unknown",
];

fn assert_summary_object(obj: &serde_json::Value, ctx: &str) {
    assert_exact_keys(obj, &SUMMARY_KEYS, ctx);
    for key in SUMMARY_KEYS {
        assert!(
            obj[key].is_null() || obj[key].as_u64().is_some(),
            "{ctx}: summary.{key} should be a count or null, got {}",
            obj[key]
        );
    }
    for key in SUMMARY_CONTRACT_KEYS {
        assert!(
            obj.as_object().unwrap().contains_key(key),
            "{ctx}: summary.{key} is a contract key and must always be present"
        );
    }
}

fn assert_info_object(obj: &serde_json::Value) {
    let ctx = "info";
    assert_exact_keys(
        obj,
        &[
            "schema_version",
            "session_id",
            "provider",
            "detected_format",
            "title",
            "native_name",
            "workspace",
            "messages",
            "summary",
            "live_summary",
            "started_at",
            "ended_at",
            "model_name",
            "source_path",
            "metadata",
            "workspace_name",
            "workspace_name_source",
        ],
        ctx,
    );
    assert_uint(&obj["schema_version"], "schema_version", ctx);
    assert_eq!(
        obj["schema_version"].as_u64().unwrap(),
        6,
        "{ctx}: schema_version should be 6"
    );
    assert_string(&obj["session_id"], "session_id", ctx);
    assert_string(&obj["provider"], "provider", ctx);
    assert_string(&obj["detected_format"], "detected_format", ctx);
    assert_summary_object(&obj["summary"], ctx);
    assert_summary_object(&obj["live_summary"], "info.live_summary");
    // The fold cannot invent events, so `live_summary` is bounded by `summary`
    // key for key. A caller diffing the two relies on exactly this.
    for key in SUMMARY_KEYS {
        if let (Some(all), Some(live)) = (
            obj["summary"][key].as_u64(),
            obj["live_summary"][key].as_u64(),
        ) {
            assert!(
                live <= all,
                "{ctx}: live_summary.{key} ({live}) exceeds summary.{key} ({all})"
            );
        }
    }
    assert_string_or_null(&obj["title"], "title", ctx);
    assert_string_or_null(&obj["native_name"], "native_name", ctx);
    assert_string_or_null(&obj["workspace"], "workspace", ctx);
    assert_uint(&obj["messages"], "messages", ctx);
    assert_number_or_null(&obj["started_at"], "started_at", ctx);
    assert_number_or_null(&obj["ended_at"], "ended_at", ctx);
    assert_string_or_null(&obj["model_name"], "model_name", ctx);
    assert_string(&obj["source_path"], "source_path", ctx);
    // metadata is object or null.
    assert!(
        obj["metadata"].is_object() || obj["metadata"].is_null(),
        "{ctx}: metadata should be object|null"
    );
    assert_string_or_null(&obj["workspace_name"], "workspace_name", ctx);
    assert_string_or_null(&obj["workspace_name_source"], "workspace_name_source", ctx);
}

#[test]
fn contract_info_json_shape_cc() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id])
        .output()
        .expect("info should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from info: {e}\nOutput: {stdout}"));

    assert_info_object(&parsed);
    assert_eq!(parsed["session_id"].as_str().unwrap(), session_id);
    assert_eq!(parsed["provider"].as_str().unwrap(), "claude-code");
}

#[test]
fn contract_info_json_shape_codex() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id])
        .output()
        .expect("info should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from info: {e}\nOutput: {stdout}"));

    assert_info_object(&parsed);
    assert_eq!(parsed["provider"].as_str().unwrap(), "codex");
}

#[test]
fn contract_info_json_shape_gemini() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_gemini_fixture(&tmp, "gmi_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id])
        .output()
        .expect("info should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from info: {e}\nOutput: {stdout}"));

    assert_info_object(&parsed);
    assert_eq!(parsed["provider"].as_str().unwrap(), "gemini");
}

#[test]
fn contract_info_json_source_path_is_absolute() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    let path = parsed["source_path"].as_str().unwrap();
    assert!(
        path.starts_with('/'),
        "source_path should be absolute, got: {path}"
    );
}

/// `info` takes a session file path where it takes a session ID.
///
/// It appeared to already: `Codex::owns_session` joins its argument onto the
/// Codex sessions directory, and joining an *absolute* path throws the left
/// side away, so any absolute path resolved as Codex. A Claude Code transcript
/// was therefore parsed by the Codex reader and reported as
/// `provider: "codex"` with zero messages. The path form has to resolve by
/// path.
#[test]
fn contract_info_accepts_a_session_file_path() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let by_id: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        &casr_cmd(&tmp)
            .args(["--json", "info", &session_id])
            .output()
            .expect("info by id should run")
            .stdout,
    ))
    .expect("info by id emits JSON");
    let path = by_id["source_path"].as_str().unwrap().to_string();

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &path])
        .output()
        .expect("info by path should run");
    assert!(
        output.status.success(),
        "info by path failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let by_path: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("info by path emits JSON");

    assert_info_object(&by_path);
    assert_eq!(
        by_path["detected_format"], "claude-code",
        "a Claude transcript must not be read by the Codex parser: {by_path}"
    );
    assert_eq!(by_path["session_id"], by_id["session_id"]);
    assert_eq!(by_path["messages"], by_id["messages"]);
    assert_eq!(by_path["summary"], by_id["summary"]);
}

/// `list --json` says `null`, not `0`, when nothing could count tool uses.
///
/// `tool_uses` comes from `CanonicalMessage::tool_calls`, and falls back to a
/// per-provider scan of the source file when that is empty — because most flat
/// readers never populate the field, so an empty one does not mean "no tools".
/// Only four providers have a scan; the rest fell off the end of the match and
/// reported `0`, which claimed that every session on seventeen providers had
/// made no tool calls at all.
///
/// Amp is one of the seventeen: no scanner, and its reader leaves `tool_calls`
/// empty, so `0` here would be an answer nothing produced.
#[test]
fn contract_list_tool_uses_is_null_when_uncountable() {
    const AMP_THREAD: &str = "T-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let tmp = TempDir::new().unwrap();
    // `$XDG_DATA_HOME/amp/threads`, matching what `casr_cmd` exports. Not
    // `$AMP_HOME`: Amp means its *install* tree by that name, so casr does not
    // read it at all and a thread seeded under it belongs to nobody.
    let threads = tmp.path().join("xdg-data/amp/threads");
    fs::create_dir_all(&threads).expect("mkdir");
    fs::copy(
        fixtures_dir().join(format!("amp/{AMP_THREAD}.json")),
        threads.join(format!("{AMP_THREAD}.json")),
    )
    .expect("seed amp thread");

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "list",
            "--provider",
            "amp",
            "--workspace",
            "/data/projects/fixture-amp",
        ])
        .output()
        .expect("list should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("JSON");
    let items = parsed["items"].as_array().expect("items");
    assert!(
        !items.is_empty(),
        "the seeded session should be listed: {parsed}"
    );
    for item in items {
        assert!(
            item["messages"].as_u64().unwrap_or(0) > 0,
            "the session was really read: {item}"
        );
        assert!(
            item["tool_uses"].is_null(),
            "nothing can count Amp's tool uses, so any number here is invented: {item}"
        );
    }

    // A provider that *can* count still reports a number, so the null above is
    // the absence of an answer rather than the field having gone away.
    let gemini_tmp = TempDir::new().unwrap();
    setup_gemini_fixture(&gemini_tmp, "gmi_simple");
    let output = casr_cmd(&gemini_tmp)
        .args(["--json", "list", "--provider", "gemini"])
        .output()
        .expect("list should run");
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("JSON");
    for item in parsed["items"].as_array().expect("items") {
        assert!(
            item["tool_uses"].as_u64().is_some(),
            "gemini has a scanner and must answer with a count: {item}"
        );
    }
}

/// `--from` forces the reader, and `detected_format` reports what it forced.
#[test]
fn contract_info_from_forces_the_reader() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args(["--json", "info", &session_id, "--from", "cod"])
        .output()
        .expect("info --from should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("JSON");
    assert_info_object(&parsed);
    assert_eq!(parsed["detected_format"], "codex");

    // An unknown slug is refused up front rather than silently detected.
    let refused = casr_cmd(&tmp)
        .args(["--json", "info", &session_id, "--from", "not-an-agent"])
        .output()
        .expect("info --from bogus should run");
    assert!(!refused.status.success(), "an unknown --from must not pass");
    let error: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&refused.stderr)).expect("error JSON");
    assert_eq!(error["error_type"], "UnknownProviderAlias");
}

/// The structured track answers every count; the flat track answers `null`
/// where it cannot see, and never `0`.
///
/// Both halves are asserted in one test because the contract is the
/// *difference* between them: a caller diffing a source against its conversion
/// is only safe if `null` and `0` cannot be confused.
#[test]
fn contract_info_summary_null_means_unknowable_not_zero() {
    let tmp = TempDir::new().unwrap();

    let cc_id = setup_cc_fixture(&tmp, "cc_simple");
    let structured: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        &casr_cmd(&tmp)
            .args(["--json", "info", &cc_id])
            .output()
            .expect("claude-code info should run")
            .stdout,
    ))
    .expect("JSON");
    let summary = &structured["summary"];
    for key in SUMMARY_KEYS {
        assert!(
            summary[key].as_u64().is_some(),
            "the structured track knows every count; summary.{key} was {}",
            summary[key]
        );
    }
    assert_eq!(
        summary["message"], structured["messages"],
        "a structured reader that agrees with the flat one should say so: {structured}"
    );
    assert!(
        summary["message"].as_u64().unwrap() > 0,
        "the fixture has messages: {summary}"
    );

    let gemini_id = setup_gemini_fixture(&tmp, "gmi_simple");
    let flat: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        &casr_cmd(&tmp)
            .args(["--json", "info", &gemini_id])
            .output()
            .expect("gemini info should run")
            .stdout,
    ))
    .expect("JSON");
    let summary = &flat["summary"];
    assert_eq!(flat["detected_format"], "gemini");
    // What a `CanonicalSession` holds is countable.
    for key in ["message", "tool_call", "tool_result"] {
        assert!(
            summary[key].as_u64().is_some(),
            "the flat model has a field for {key}: {summary}"
        );
    }
    // What it does not hold is `null` — present, and never 0.
    for key in [
        "reasoning",
        "compaction",
        "sealed_context",
        "turn_config",
        "env_snapshot",
        "attachment",
        "rollback",
        "abort",
        "control",
        "unknown",
    ] {
        assert!(
            summary[key].is_null(),
            "summary.{key} must be null on the flat track, not {}: 0 would \
             let a conversion that deleted them compare clean",
            summary[key]
        );
    }
}

// ---------------------------------------------------------------------------
// Contract: `resume --json` (success)
// ---------------------------------------------------------------------------
// Expected shape: {ok, source_provider, target_provider, source_session_id,
//                  target_session_id, written_paths, resume_command, dry_run,
//                  fidelity, verified_fidelity, losses, launch_command,
//                  launch_targets_session, launch_error, warnings}

/// Every grade `Fidelity` can serialize to, so a renamed variant fails here
/// rather than silently becoming an unknown string in a caller's parser.
/// Every `LossKind` a caller can be handed, for the same reason as
/// `FIDELITY_GRADES`: a renamed variant has to break here rather than turn into
/// an unrecognised string in somebody's parser.
const LOSS_KINDS: &[&str] = &[
    "sealed_context",
    "conversation",
    "reasoning",
    "media",
    "tool_protocol",
    "metadata",
];

const FIDELITY_GRADES: &[&str] = &[
    "byte_identical",
    "native_equivalent",
    "context_complete",
    "context_no_reasoning",
    "conversation_only",
    "transcript_only",
    "history_incomplete",
];

fn assert_resume_success_object(obj: &serde_json::Value) {
    let ctx = "resume_success";
    assert_exact_keys(
        obj,
        &[
            "ok",
            "source_provider",
            "target_provider",
            "source_session_id",
            "target_session_id",
            "written_paths",
            "resume_command",
            "dry_run",
            "fidelity",
            "verified_fidelity",
            "losses",
            "launch_command",
            "launch_targets_session",
            "launch_error",
            "warnings",
        ],
        ctx,
    );
    assert_bool(&obj["ok"], "ok", ctx);
    assert_eq!(obj["ok"], true, "{ctx}: ok should be true");
    assert_string(&obj["source_provider"], "source_provider", ctx);
    assert_string(&obj["target_provider"], "target_provider", ctx);
    assert_string(&obj["source_session_id"], "source_session_id", ctx);
    assert_string_or_null(&obj["target_session_id"], "target_session_id", ctx);
    assert_array_or_null(&obj["written_paths"], "written_paths", ctx);
    assert_string_or_null(&obj["resume_command"], "resume_command", ctx);
    assert_bool(&obj["dry_run"], "dry_run", ctx);
    assert_string(&obj["fidelity"], "fidelity", ctx);
    let grade = obj["fidelity"].as_str().unwrap();
    assert!(
        FIDELITY_GRADES.contains(&grade),
        "{ctx}: fidelity {grade:?} is not one of {FIDELITY_GRADES:?}"
    );
    if !obj["verified_fidelity"].is_null() {
        let verified = obj["verified_fidelity"].as_str().unwrap_or_else(|| {
            panic!(
                "{ctx}: verified_fidelity should be a grade string or null, got {:?}",
                obj["verified_fidelity"]
            )
        });
        assert!(
            FIDELITY_GRADES.contains(&verified),
            "{ctx}: verified_fidelity {verified:?} is not one of {FIDELITY_GRADES:?}"
        );
    }
    assert_array(&obj["losses"], "losses", ctx);
    for loss in obj["losses"].as_array().unwrap() {
        assert_exact_keys(
            loss,
            &["kind", "events", "capsules", "bytes", "grade", "note"],
            "resume_success.losses[]",
        );
        assert!(
            LOSS_KINDS.contains(&loss["kind"].as_str().unwrap_or_default()),
            "{ctx}: loss kind {:?} is not one of {LOSS_KINDS:?}",
            loss["kind"]
        );
        assert!(
            FIDELITY_GRADES.contains(&loss["grade"].as_str().unwrap_or_default()),
            "{ctx}: loss grade {:?} is not one of {FIDELITY_GRADES:?}",
            loss["grade"]
        );
        assert!(loss["events"].is_u64() && loss["capsules"].is_u64() && loss["bytes"].is_u64());
        assert_string(&loss["note"], "note", "resume_success.losses[]");
    }
    assert_string_or_null(&obj["launch_error"], "launch_error", ctx);
    assert_string_or_null(&obj["launch_command"], "launch_command", ctx);
    assert!(
        obj["launch_targets_session"].is_boolean() || obj["launch_targets_session"].is_null(),
        "{ctx}: launch_targets_session should be a bool or null, got {:?}",
        obj["launch_targets_session"]
    );
    assert_array(&obj["warnings"], "warnings", ctx);
}

/// `--no-store` emits exactly the key set `resume --json` emitted before the
/// store existed.
///
/// The pin for the escape hatch, at the level a script sees it. `source_selection`
/// is `skip_serializing_if = "Option::is_none"` precisely so this holds: a caller
/// that worked yesterday gets the same object today, and the field's mere presence
/// is the signal that the source was substituted.
#[test]
fn contract_resume_json_no_store_adds_no_field() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--no-store"])
        .output()
        .expect("resume --no-store should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    // `assert_resume_success_object` asserts the exact key set, so this is the
    // whole assertion: an extra field would fail it.
    assert_resume_success_object(&parsed);
}

/// A store-backed second hop reports the substituted source as fields.
///
/// `codex → claude → codex`: the store has both incarnations by the third command
/// and the origin needs no conversion at all, so it reads the origin and says so.
/// The sentence is for a human; this is the contract a caller gets.
#[test]
fn contract_resume_json_reports_a_substituted_source() {
    let tmp = TempDir::new().unwrap();
    let codex_id = setup_codex_fixture(&tmp, "codex_reasoning", "jsonl");

    // Hop one: out of Codex, which teaches the store that both sessions are one
    // conversation.
    let first = casr_cmd(&tmp)
        .args(["--json", "resume", "cc", &codex_id])
        .output()
        .expect("codex -> claude should run");
    assert!(
        first.status.success(),
        "hop one failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&first.stdout)).expect("hop one JSON");
    let cc_id = first["target_session_id"]
        .as_str()
        .expect("hop one wrote a Claude session")
        .to_string();

    // Hop two: back into Codex, naming the Claude session.
    let second = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &cc_id])
        .output()
        .expect("claude -> codex should run");
    assert!(
        second.status.success(),
        "hop two failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("Invalid JSON: {e}\n{stdout}"));

    let selection = &parsed["source_selection"];
    assert!(
        selection.is_object(),
        "hop two read a session the user did not name and must say so in the JSON, got:\n{stdout}"
    );
    assert_exact_keys(
        selection,
        &[
            "record_id",
            "provider",
            "session_id",
            "role",
            "availability",
            "origin_state",
            "origin_detail",
            "capsules",
            "capsule_bytes",
            "named_provider",
            "named_session_id",
            "cost_capsules",
            "cost_capsule_bytes",
        ],
        "source_selection",
    );
    assert_eq!(selection["provider"], "codex");
    assert_eq!(selection["session_id"], codex_id.as_str());
    assert_eq!(selection["role"], "origin");
    assert_eq!(selection["availability"], "ready");
    // The cheap resolution, and it says which check it actually ran: same size
    // and mtime, bytes not re-read.
    assert_eq!(selection["origin_state"], "unchanged");
    assert_eq!(selection["named_provider"], "claude-code");
    assert_eq!(selection["named_session_id"], cc_id.as_str());
    assert!(selection["cost_capsules"].is_u64());
    assert!(selection["cost_capsule_bytes"].is_u64());
    // Nothing was written: the best source for a Codex target was already a
    // Codex session, so the resume command points back at its own bytes.
    assert_eq!(parsed["fidelity"], "byte_identical");
    assert_eq!(parsed["target_session_id"], codex_id.as_str());

    // And the escape hatch on the very same command reverts to today's answer.
    let bare = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &cc_id, "--no-store"])
        .output()
        .expect("claude -> codex --no-store should run");
    let bare: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&bare.stdout)).expect("no-store JSON");
    assert_resume_success_object(&bare);
    assert_eq!(bare["source_provider"], "claude-code");
    assert_ne!(bare["target_session_id"], codex_id.as_str());
}

#[test]
fn contract_resume_json_dry_run_cc_to_codex() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--dry-run"])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    assert_eq!(parsed["source_provider"].as_str().unwrap(), "claude-code");
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "codex");
    assert_eq!(parsed["dry_run"], true);
    // Dry run: no target session, no written paths, no resume command.
    assert!(parsed["target_session_id"].is_null());
    assert!(parsed["written_paths"].is_null());
    assert!(parsed["resume_command"].is_null());
    // The grade of the conversion this dry run describes, which for a
    // Claude Code → Codex pair is the structured writer's. It used to read
    // `conversation_only` — the flat projection's grade, from a track the real
    // run does not take. `contract_resume_json_dry_run_matches_the_real_run`
    // pins the two together; this pins the value.
    assert_eq!(parsed["fidelity"], "context_complete");
    // No launch was asked for. Null, not false: "not applicable" and "will not
    // resume the converted session" are different answers.
    assert!(parsed["launch_command"].is_null());
    assert!(parsed["launch_targets_session"].is_null());
}

/// A dry run has to predict the run it is a dry run of.
///
/// The point of `--dry-run` is to answer "what will this cost me before I let it
/// write", so the two numbers a decision is made on — the grade and the losses
/// behind it — must be the ones the same command line produces without the flag.
/// They were not: the dry run returned before track selection *and* before the
/// budget, so it answered with the flat projection's grade on every conversion,
/// including the ones the real run hands to a structured writer and the ones a
/// `--max-context-tokens` would gut.
///
/// Run across three shapes because the old branch was wrong in a different way
/// in each: a structured pair, a flat target with a binding budget, and a
/// same-provider conversion that is not a conversion at all.
///
/// `verified_fidelity` is deliberately not compared. A dry run writes nothing,
/// so there is nothing to read back, and reporting a verification that never ran
/// would be the same class of lie in the other direction.
#[test]
fn contract_resume_json_dry_run_matches_the_real_run() {
    let grades = |args: &[&str], fixture: &str| -> (serde_json::Value, serde_json::Value) {
        let mut out = Vec::new();
        for dry in [true, false] {
            let tmp = TempDir::new().unwrap();
            let session_id = setup_cc_fixture(&tmp, fixture);
            let mut argv: Vec<String> = vec!["--json".into(), "resume".into()];
            argv.extend(args.iter().map(|a| (*a).to_string()));
            argv.push(session_id);
            if dry {
                argv.push("--dry-run".into());
            }
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
            out.push(serde_json::json!({
                "fidelity": parsed["fidelity"],
                "losses": parsed["losses"],
            }));
        }
        (out.remove(0), out.remove(0))
    };

    // Structured track: the writer's grade, not the projection's.
    let (dry, real) = grades(&["cod"], "cc_simple");
    assert_eq!(dry, real, "cc → codex, no flags");

    // Flat track, with a budget small enough to delete something. This is the
    // command line the fix is for: someone deciding whether the cap is
    // survivable, before letting it write.
    let (dry, real) = grades(
        &["gmi", "--max-tool-output", "20", "--drop-reasoning"],
        "cc_complex",
    );
    assert_eq!(dry, real, "cc → gemini under a binding budget");
    assert!(
        dry["losses"]
            .as_array()
            .is_some_and(|losses| !losses.is_empty()),
        "this budget has to remove something for the comparison to test anything: {dry}"
    );

    // Same provider: nothing is converted and nothing is rewritten, and a dry
    // run of that used to report the flat projection's grade instead.
    let (dry, real) = grades(&["cc"], "cc_simple");
    assert_eq!(dry, real, "cc → cc");
    assert_eq!(dry["fidelity"], "byte_identical");
}

#[test]
fn contract_resume_json_actual_write_cc_to_codex() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    assert_eq!(parsed["dry_run"], false);
    assert!(parsed["target_session_id"].is_string());
    let paths = parsed["written_paths"]
        .as_array()
        .expect("written_paths should be array on actual write");
    assert!(!paths.is_empty(), "should have at least one written path");
    for (i, p) in paths.iter().enumerate() {
        assert!(p.is_string(), "written_paths[{i}] should be a string");
    }
    assert!(parsed["resume_command"].is_string());
}

#[test]
fn contract_resume_json_actual_write_cc_to_gemini() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "gmi", &session_id])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "gemini");
    assert_eq!(parsed["dry_run"], false);
    assert!(parsed["target_session_id"].is_string());
}

#[test]
fn contract_resume_json_warnings_are_strings() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--dry-run"])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    let warnings = parsed["warnings"].as_array().unwrap();
    for (i, w) in warnings.iter().enumerate() {
        assert!(w.is_string(), "warnings[{i}] should be a string");
    }
}

#[test]
fn contract_resume_json_codex_to_cc() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_codex_fixture(&tmp, "codex_modern", "jsonl");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cc", &session_id])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    assert_eq!(parsed["source_provider"].as_str().unwrap(), "codex");
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "claude-code");
}

#[test]
fn contract_resume_json_gemini_to_codex() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_gemini_fixture(&tmp, "gmi_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    assert_eq!(parsed["source_provider"].as_str().unwrap(), "gemini");
    assert_eq!(parsed["target_provider"].as_str().unwrap(), "codex");
}

// ---------------------------------------------------------------------------
// Contract: the launch fields
//
// These go through `--launch-dry-run`, never `--launch`, so that a regression
// cannot start a real agent and attach it to the test runner's terminal.
// ---------------------------------------------------------------------------

#[test]
fn contract_resume_json_launch_command_is_in_the_envelope_not_on_stderr() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--launch-dry-run"])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    assert_resume_success_object(&parsed);
    let command = parsed["launch_command"]
        .as_str()
        .expect("a launch was asked for, so the envelope must carry its command");
    let target_id = parsed["target_session_id"].as_str().unwrap();
    assert_eq!(command, format!("codex resume {target_id}"));
    assert_eq!(parsed["launch_targets_session"], true);

    // The whole reason the fields exist: the command used to be printed to
    // stderr because the envelope had nowhere to put it.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("codex resume"),
        "the command belongs in the envelope, not on stderr: {stderr}"
    );
}

#[test]
fn contract_resume_json_launch_passthrough_flags_are_in_the_command() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args([
            "--json",
            "resume",
            "cod",
            &session_id,
            "--launch-dry-run",
            "--",
            "--model",
            "o3",
        ])
        .output()
        .expect("resume should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON from resume: {e}\nOutput: {stdout}"));

    let target_id = parsed["target_session_id"].as_str().unwrap();
    assert_eq!(
        parsed["launch_command"].as_str().unwrap(),
        format!("codex resume {target_id} --model o3"),
        "the envelope must report the command that would actually run"
    );
}

#[test]
fn contract_resume_json_refuses_cursor_target() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cur", &session_id, "--launch-dry-run"])
        .output()
        .expect("resume should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = parse_json_from_maybe_logged_stream(&stderr, "stderr");

    assert_error_envelope(&parsed);
    assert!(
        parsed["message"]
            .as_str()
            .is_some_and(|message| message.contains("allComposers"))
    );
}

// ---------------------------------------------------------------------------
// Contract: error JSON envelope
// ---------------------------------------------------------------------------
// Expected shape: {ok: false, error_type: string, message: string}

fn assert_error_envelope(obj: &serde_json::Value) {
    let ctx = "error_envelope";
    assert_exact_keys(obj, &["ok", "error_type", "message"], ctx);
    assert_bool(&obj["ok"], "ok", ctx);
    assert_eq!(obj["ok"], false, "{ctx}: ok should be false");
    assert_string(&obj["error_type"], "error_type", ctx);
    assert_string(&obj["message"], "message", ctx);
}

fn parse_json_from_maybe_logged_stream(raw: &str, stream_name: &str) -> serde_json::Value {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
        return parsed;
    }

    if let Some(idx) = raw.find('{') {
        let candidate = &raw[idx..];
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(candidate) {
            return parsed;
        }
    }

    panic!("Invalid JSON in {stream_name}: {raw}");
}

#[test]
fn contract_error_json_unknown_session() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "info", "nonexistent-session-id-12345"])
        .output()
        .expect("info should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = parse_json_from_maybe_logged_stream(&stderr, "stderr");

    assert_error_envelope(&parsed);
    assert_eq!(parsed["error_type"].as_str().unwrap(), "SessionNotFound");
}

#[test]
fn contract_error_json_unknown_provider() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "nonexistent", &session_id])
        .output()
        .expect("resume should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = parse_json_from_maybe_logged_stream(&stderr, "stderr");

    assert_error_envelope(&parsed);
    assert_eq!(
        parsed["error_type"].as_str().unwrap(),
        "UnknownProviderAlias"
    );
}

#[test]
fn contract_error_json_unknown_resume_session() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", "nonexistent-session-99999"])
        .output()
        .expect("resume should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = parse_json_from_maybe_logged_stream(&stderr, "stderr");

    assert_error_envelope(&parsed);
    assert_eq!(parsed["error_type"].as_str().unwrap(), "SessionNotFound");
}

#[test]
fn contract_error_json_message_is_nonempty() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "info", "nonexistent-session-id-12345"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = parse_json_from_maybe_logged_stream(&stderr, "stderr");

    let msg = parsed["message"].as_str().unwrap();
    assert!(!msg.is_empty(), "error message should not be empty");
    assert!(
        msg.contains("nonexistent-session-id-12345"),
        "error message should reference the session id"
    );
}

#[test]
fn contract_error_json_known_error_types() {
    // Verify all error types map to valid CasrError variant names.
    let known_types = [
        "SessionNotFound",
        "AmbiguousSessionId",
        "UnknownProviderAlias",
        "ProviderUnavailable",
        "SessionReadError",
        "SessionWriteError",
        "SessionConflict",
        "ValidationError",
        "VerifyFailed",
        "InternalError",
    ];

    // Trigger SessionNotFound.
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "info", "no-such-session"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    let error_type = parsed["error_type"].as_str().unwrap();
    assert!(
        known_types.contains(&error_type),
        "error_type '{error_type}' not in known types: {known_types:?}"
    );
}

// ---------------------------------------------------------------------------
// Cross-cutting: JSON output goes to stdout (success) or stderr (error)
// ---------------------------------------------------------------------------

#[test]
fn contract_success_json_on_stdout_not_stderr() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "providers"])
        .output()
        .unwrap();

    assert!(output.status.success());
    // Success JSON should be on stdout.
    assert!(
        !output.stdout.is_empty(),
        "success JSON should be on stdout"
    );
    // Stderr should be empty or contain only trace/debug logs (not JSON).
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        assert!(
            serde_json::from_str::<serde_json::Value>(&stderr).is_err(),
            "stderr should not contain JSON on success"
        );
    }
}

#[test]
fn contract_error_json_on_stderr_not_stdout() {
    let tmp = TempDir::new().unwrap();
    let output = casr_cmd(&tmp)
        .args(["--json", "info", "no-such-session"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    // Error JSON should be on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        serde_json::from_str::<serde_json::Value>(&stderr).is_ok(),
        "error JSON should be on stderr"
    );
    // Stdout should be empty on error.
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty on error, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ---------------------------------------------------------------------------
// Field stability: verify key fields are present across provider types
// ---------------------------------------------------------------------------

#[test]
fn contract_list_provider_field_matches_slug() {
    let tmp = TempDir::new().unwrap();
    setup_cc_fixture(&tmp, "cc_simple");
    setup_codex_fixture(&tmp, "codex_modern", "jsonl");
    setup_gemini_fixture(&tmp, "gmi_simple");

    let output = casr_cmd(&tmp).args(["--json", "list"]).output().unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    let items = parsed["items"].as_array().expect("should have items array");

    let valid_slugs = [
        "claude-code",
        "codex",
        "gemini",
        "antigravity",
        "cursor",
        "cline",
        "aider",
        "amp",
        "opencode",
        "chatgpt",
        "clawdbot",
        "vibe",
        "factory",
        "openclaw",
        "pi-agent",
        "kiro",
        "grok",
    ];
    for item in items {
        let provider = item["provider"].as_str().unwrap();
        assert!(
            valid_slugs.contains(&provider),
            "list item provider '{provider}' not in known slugs"
        );
    }
}

#[test]
fn contract_resume_source_session_id_matches_input() {
    let tmp = TempDir::new().unwrap();
    let session_id = setup_cc_fixture(&tmp, "cc_simple");

    let output = casr_cmd(&tmp)
        .args(["--json", "resume", "cod", &session_id, "--dry-run"])
        .output()
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    assert_eq!(
        parsed["source_session_id"].as_str().unwrap(),
        session_id,
        "source_session_id should match the input session ID"
    );
}

// ---------------------------------------------------------------------------
// #17: provider-native session name (Claude Code `/rename`)
// ---------------------------------------------------------------------------

/// Install a raw Claude Code session file (`content`) under a workspace and
/// return its session ID.
fn install_cc_raw_session(tmp: &TempDir, session_id: &str, cwd: &str, content: &str) {
    let project_key: String = cwd
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let projects_dir = tmp.path().join("claude/projects").join(&project_key);
    fs::create_dir_all(&projects_dir).expect("create CC project dir");
    let target = projects_dir.join(format!("{session_id}.jsonl"));
    fs::write(&target, content).expect("write raw CC session");
}

const CC_RENAMED_SESSION: &str = concat!(
    r#"{"type":"custom-title","customTitle":"My Renamed Session","sessionId":"rename-1"}"#,
    "\n",
    r#"{"type":"ai-title","aiTitle":"Auto Title","sessionId":"rename-1"}"#,
    "\n",
    r#"{"type":"user","sessionId":"rename-1","cwd":"/data/projects/named","message":{"role":"user","content":"First question"},"uuid":"u1","timestamp":"2026-01-01T00:00:00Z"}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"rename-1","cwd":"/data/projects/named","message":{"role":"assistant","content":"An answer","model":"m1"},"uuid":"u2","timestamp":"2026-01-01T00:00:01Z"}"#,
    "\n",
    r#"{"type":"user","sessionId":"rename-1","cwd":"/data/projects/named","message":{"role":"user","content":"A follow up"},"uuid":"u3","timestamp":"2026-01-01T00:00:02Z"}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"rename-1","cwd":"/data/projects/named","message":{"role":"assistant","content":"Final reply","model":"m1"},"uuid":"u4","timestamp":"2026-01-01T00:00:03Z"}"#,
);

const CC_UNNAMED_SESSION: &str = concat!(
    r#"{"type":"user","sessionId":"plain-1","cwd":"/data/projects/plain","message":{"role":"user","content":"Just a question"},"uuid":"u1","timestamp":"2026-01-01T00:00:00Z"}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"plain-1","cwd":"/data/projects/plain","message":{"role":"assistant","content":"Just an answer","model":"m1"},"uuid":"u2","timestamp":"2026-01-01T00:00:01Z"}"#,
);

#[test]
fn contract_list_json_native_name_present() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/named"])
        .output()
        .expect("list should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let items = assert_list_envelope(&parsed);
    assert!(
        !items.is_empty(),
        "expected the renamed session in the list"
    );
    // `/rename` custom title wins over the auto-generated ai-title.
    assert_eq!(items[0]["native_name"].as_str(), Some("My Renamed Session"));
}

#[test]
fn contract_list_json_native_name_absent_is_null() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "plain-1", "/data/projects/plain", CC_UNNAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["--json", "list", "--workspace", "/data/projects/plain"])
        .output()
        .expect("list should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let items = assert_list_envelope(&parsed);
    assert!(!items.is_empty(), "expected the session in the list");
    assert!(
        items[0]["native_name"].is_null(),
        "a session with no native name must be null: {:?}",
        items[0]["native_name"]
    );
}

#[test]
fn contract_list_human_shows_name_column() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["list", "--workspace", "/data/projects/named"])
        .output()
        .expect("list should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Name"), "table should carry a Name column");
    assert!(
        stdout.contains("My Renamed Session"),
        "human list should render the native name: {stdout}"
    );
}

#[test]
fn contract_info_human_shows_name_line() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["info", "rename-1"])
        .output()
        .expect("info should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Name:") && stdout.contains("My Renamed Session"),
        "info should show the native Name line: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// #18: `info --peek` transcript tail
// ---------------------------------------------------------------------------

#[test]
fn contract_info_peek_json_includes_ordered_tail() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["--json", "info", "rename-1", "--peek", "--peek-lines", "2"])
        .output()
        .expect("info --peek should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();

    let tail = parsed["transcript_tail"]
        .as_array()
        .expect("transcript_tail should be an array with --peek");
    assert_eq!(tail.len(), 2, "should respect --peek-lines count");
    // Tail = the LAST two turns, in chronological order.
    assert_eq!(tail[0]["role"].as_str(), Some("User"));
    assert_eq!(tail[0]["snippet"].as_str(), Some("A follow up"));
    assert_eq!(tail[1]["role"].as_str(), Some("Assistant"));
    assert_eq!(tail[1]["snippet"].as_str(), Some("Final reply"));
}

#[test]
fn contract_info_without_peek_has_no_tail() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["--json", "info", "rename-1"])
        .output()
        .expect("info should run");
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert!(
        parsed.get("transcript_tail").is_none(),
        "transcript_tail must be omitted without --peek"
    );
}

#[test]
fn contract_info_human_peek_shows_tail_section() {
    let tmp = TempDir::new().unwrap();
    install_cc_raw_session(&tmp, "rename-1", "/data/projects/named", CC_RENAMED_SESSION);

    let output = casr_cmd(&tmp)
        .args(["info", "rename-1", "--peek", "--peek-lines", "3"])
        .output()
        .expect("info --peek should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Transcript Tail"),
        "peek should append a Transcript Tail section: {stdout}"
    );
    assert!(stdout.contains("[Assistant]") && stdout.contains("Final reply"));
    // Original Session Info layout is preserved above the tail.
    assert!(stdout.contains("Session Info") && stdout.contains("Roles:"));
}
