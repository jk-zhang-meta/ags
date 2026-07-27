//! Integration tests for the fixture corpus.
//!
//! Reads each fixture file through its provider reader and validates
//! the resulting CanonicalSession against the expected canonical summary.

use std::path::{Path, PathBuf};

use casr::model::{CanonicalSession, MessageRole};
use casr::providers::Provider;
use casr::providers::aider::Aider;
use casr::providers::amp::Amp;
use casr::providers::antigravity::Antigravity;
use casr::providers::chatgpt::ChatGpt;
use casr::providers::claude_code::ClaudeCode;
use casr::providers::clawdbot::ClawdBot;
use casr::providers::cline::Cline;
use casr::providers::codex::Codex;
use casr::providers::cursor::Cursor;
use casr::providers::factory::Factory;
use casr::providers::gemini::Gemini;
use casr::providers::grok::Grok;
use casr::providers::kiro::Kiro;
use casr::providers::openclaw::OpenClaw;
use casr::providers::opencode::OpenCode;
use casr::providers::pi_agent::PiAgent;
use casr::providers::vibe::Vibe;

/// Root of the fixtures directory (relative to workspace root).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Load and parse the expected canonical summary for a fixture.
fn load_expected(fixture_id: &str) -> serde_json::Value {
    let path = fixtures_dir().join(format!("expected/{fixture_id}.json"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read expected file for {fixture_id}: {e}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse expected JSON for {fixture_id}: {e}"))
}

/// Assert a session matches its expected canonical summary.
fn assert_session_matches(
    session: &CanonicalSession,
    expected: &serde_json::Value,
    fixture_id: &str,
) {
    // Provider slug.
    assert_eq!(
        session.provider_slug,
        expected["provider_slug"].as_str().unwrap(),
        "[{fixture_id}] provider_slug mismatch"
    );

    // Session ID.
    assert_eq!(
        session.session_id,
        expected["session_id"].as_str().unwrap(),
        "[{fixture_id}] session_id mismatch"
    );

    // Workspace.
    if expected["workspace"].is_null() {
        assert!(
            session.workspace.is_none(),
            "[{fixture_id}] expected workspace=None but got {:?}",
            session.workspace
        );
    } else {
        assert_eq!(
            session.workspace.as_deref(),
            Some(Path::new(expected["workspace"].as_str().unwrap())),
            "[{fixture_id}] workspace mismatch"
        );
    }

    // Title.
    if let Some(title_prefix) = expected.get("title_starts_with").and_then(|v| v.as_str()) {
        let title = session.title.as_deref().unwrap_or("");
        assert!(
            title.starts_with(title_prefix),
            "[{fixture_id}] title should start with '{title_prefix}' but was '{title}'"
        );
    } else if let Some(expected_title) = expected.get("title").and_then(|v| v.as_str()) {
        assert_eq!(
            session.title.as_deref(),
            Some(expected_title),
            "[{fixture_id}] title mismatch"
        );
    }

    // Message count.
    let expected_count = expected["message_count"].as_u64().unwrap() as usize;
    assert_eq!(
        session.messages.len(),
        expected_count,
        "[{fixture_id}] message_count mismatch (expected {expected_count}, got {})",
        session.messages.len()
    );

    // Roles.
    if let Some(expected_roles) = expected["roles"].as_array() {
        let actual_roles: Vec<String> = session
            .messages
            .iter()
            .map(|m| match &m.role {
                MessageRole::User => "User".to_string(),
                MessageRole::Assistant => "Assistant".to_string(),
                MessageRole::Tool => "Tool".to_string(),
                MessageRole::System => "System".to_string(),
                MessageRole::Other(s) => format!("Other({s})"),
            })
            .collect();
        let expected_role_strings: Vec<&str> =
            expected_roles.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            actual_roles, expected_role_strings,
            "[{fixture_id}] roles mismatch"
        );
    }

    // Timestamps.
    if let Some(started) = expected.get("started_at_present").and_then(|v| v.as_bool()) {
        assert_eq!(
            session.started_at.is_some(),
            started,
            "[{fixture_id}] started_at presence mismatch"
        );
    }
    if let Some(ended) = expected.get("ended_at_present").and_then(|v| v.as_bool()) {
        assert_eq!(
            session.ended_at.is_some(),
            ended,
            "[{fixture_id}] ended_at presence mismatch"
        );
    }

    // Model name.
    if expected["model_name"].is_null() {
        assert!(
            session.model_name.is_none(),
            "[{fixture_id}] expected model_name=None but got {:?}",
            session.model_name
        );
    } else if let Some(expected_model) = expected["model_name"].as_str() {
        assert_eq!(
            session.model_name.as_deref(),
            Some(expected_model),
            "[{fixture_id}] model_name mismatch"
        );
    }

    // Tool calls presence.
    if let Some(has_tc) = expected.get("has_tool_calls").and_then(|v| v.as_bool()) {
        let actual_has_tc = session.messages.iter().any(|m| !m.tool_calls.is_empty());
        assert_eq!(
            actual_has_tc, has_tc,
            "[{fixture_id}] has_tool_calls mismatch"
        );
    }

    // Tool results presence.
    if let Some(has_tr) = expected.get("has_tool_results").and_then(|v| v.as_bool()) {
        let actual_has_tr = session.messages.iter().any(|m| !m.tool_results.is_empty());
        assert_eq!(
            actual_has_tr, has_tr,
            "[{fixture_id}] has_tool_results mismatch"
        );
    }

    // Sequential indices.
    for (i, msg) in session.messages.iter().enumerate() {
        assert_eq!(
            msg.idx, i,
            "[{fixture_id}] message idx mismatch at position {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Claude Code fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_cc_simple() {
    let path = fixtures_dir().join("claude_code/cc_simple.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("cc_simple should parse");
    let expected = load_expected("cc_simple");
    assert_session_matches(&session, &expected, "cc_simple");
}

#[test]
fn fixture_cc_complex() {
    let path = fixtures_dir().join("claude_code/cc_complex.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("cc_complex should parse");
    let expected = load_expected("cc_complex");
    assert_session_matches(&session, &expected, "cc_complex");
}

#[test]
fn fixture_cc_missing_workspace() {
    let path = fixtures_dir().join("claude_code/cc_missing_workspace.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("cc_missing_workspace should parse");
    let expected = load_expected("cc_missing_workspace");
    assert_session_matches(&session, &expected, "cc_missing_workspace");
}

#[test]
fn fixture_cc_unicode() {
    let path = fixtures_dir().join("claude_code/cc_unicode.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("cc_unicode should parse");
    let expected = load_expected("cc_unicode");
    assert_session_matches(&session, &expected, "cc_unicode");

    // Extra: verify actual unicode content survived.
    assert!(
        session.messages[0]
            .content
            .contains("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"),
        "Japanese characters should be preserved"
    );
    assert!(
        session.messages[1].content.contains("\u{1f680}"),
        "Emoji should be preserved"
    );
}

#[test]
fn fixture_cc_malformed() {
    let path = fixtures_dir().join("claude_code/cc_malformed.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("cc_malformed should parse despite garbage");
    let expected = load_expected("cc_malformed");
    assert_session_matches(&session, &expected, "cc_malformed");
}

// ---------------------------------------------------------------------------
// Codex fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_codex_modern() {
    let path = fixtures_dir().join("codex/codex_modern.jsonl");
    let session = Codex
        .read_session(&path)
        .expect("codex_modern should parse");
    let expected = load_expected("codex_modern");
    assert_session_matches(&session, &expected, "codex_modern");
}

#[test]
fn fixture_codex_legacy() {
    let path = fixtures_dir().join("codex/codex_legacy.json");
    let session = Codex
        .read_session(&path)
        .expect("codex_legacy should parse");
    let expected = load_expected("codex_legacy");
    assert_session_matches(&session, &expected, "codex_legacy");
}

#[test]
fn fixture_codex_token_count() {
    let path = fixtures_dir().join("codex/codex_token_count.jsonl");
    let session = Codex
        .read_session(&path)
        .expect("codex_token_count should parse");
    let expected = load_expected("codex_token_count");
    assert_session_matches(&session, &expected, "codex_token_count");
}

#[test]
fn fixture_codex_reasoning() {
    let path = fixtures_dir().join("codex/codex_reasoning.jsonl");
    let session = Codex
        .read_session(&path)
        .expect("codex_reasoning should parse");
    let expected = load_expected("codex_reasoning");
    assert_session_matches(&session, &expected, "codex_reasoning");

    // Extra: verify reasoning messages have author="reasoning".
    let reasoning_msgs: Vec<_> = session
        .messages
        .iter()
        .filter(|m| m.author.as_deref() == Some("reasoning"))
        .collect();
    assert_eq!(
        reasoning_msgs.len(),
        2,
        "codex_reasoning should have exactly 2 reasoning messages"
    );
}

#[test]
fn fixture_codex_malformed() {
    let path = fixtures_dir().join("codex/codex_malformed.jsonl");
    let session = Codex
        .read_session(&path)
        .expect("codex_malformed should parse despite garbage");
    let expected = load_expected("codex_malformed");
    assert_session_matches(&session, &expected, "codex_malformed");
}

// ---------------------------------------------------------------------------
// Gemini fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_gmi_simple() {
    let path = fixtures_dir().join("gemini/gmi_simple.json");
    let session = Gemini.read_session(&path).expect("gmi_simple should parse");
    let expected = load_expected("gmi_simple");
    assert_session_matches(&session, &expected, "gmi_simple");
}

#[test]
fn fixture_gmi_grounding() {
    let path = fixtures_dir().join("gemini/gmi_grounding.json");
    let session = Gemini
        .read_session(&path)
        .expect("gmi_grounding should parse");
    let expected = load_expected("gmi_grounding");
    assert_session_matches(&session, &expected, "gmi_grounding");

    // Extra: verify grounding metadata is preserved in extra field.
    let model_msgs: Vec<_> = session
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    assert!(
        model_msgs
            .iter()
            .any(|m| m.extra.get("groundingMetadata").is_some()),
        "grounding metadata should be preserved in extra"
    );
}

#[test]
fn fixture_gmi_missing_workspace() {
    let path = fixtures_dir().join("gemini/gmi_missing_workspace.json");
    let session = Gemini
        .read_session(&path)
        .expect("gmi_missing_workspace should parse");
    let expected = load_expected("gmi_missing_workspace");
    assert_session_matches(&session, &expected, "gmi_missing_workspace");
}

#[test]
fn fixture_gmi_gemini_role() {
    let path = fixtures_dir().join("gemini/gmi_gemini_role.json");
    let session = Gemini
        .read_session(&path)
        .expect("gmi_gemini_role should parse");
    let expected = load_expected("gmi_gemini_role");
    assert_session_matches(&session, &expected, "gmi_gemini_role");
}

/// The format current Gemini actually writes, and the fold that reads it.
///
/// Every other `gmi_*` fixture is legacy whole-file JSON, which is why this
/// reader could be blind to `.jsonl` for twenty-odd releases with a green
/// suite. Against the old reader this fails at the first line —
/// `read_session` was a whole-file `serde_json::from_reader`, so a JSONL file
/// is `trailing characters at line 2 column 1`.
#[test]
fn fixture_gmi_jsonl_rewind() {
    let path = fixtures_dir().join("gemini/gmi_jsonl_rewind.jsonl");
    let session = Gemini
        .read_session(&path)
        .expect("gmi_jsonl_rewind should parse");
    let expected = load_expected("gmi_jsonl_rewind");
    assert_session_matches(&session, &expected, "gmi_jsonl_rewind");

    // The fixture's rewound turns are the only ones marked REWOUND, so this is
    // a whole-session statement and not a spot check on two indices.
    for message in &session.messages {
        assert!(
            !message.content.contains("REWOUND"),
            "the user rewound past this turn; Gemini has no record of it and \
             neither may we: {:?}",
            message.content
        );
    }

    // Unrecognised records are counted rather than dropped, and named.
    let unrecognized = &session.metadata["unrecognized_records"];
    assert_eq!(unrecognized["unclassified"], 1);
    assert_eq!(unrecognized["unparseable_lines"], 0);
    assert_eq!(
        unrecognized["keys"],
        serde_json::json!(["$recordTypeThisReaderHasNeverSeen"])
    );
}

// ---------------------------------------------------------------------------
// Grok Build (grk) fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_grok_simple() {
    let path = fixtures_dir().join(
        "grok/sessions/%2Fdata%2Fprojects%2Fdemo/019f75d0-aaaa-7bbb-8ccc-b0a1b2c3d4e5/updates.jsonl",
    );
    let session = Grok.read_session(&path).expect("grok_simple should parse");
    let expected = load_expected("grok_simple");
    assert_session_matches(&session, &expected, "grok_simple");

    // Chunk coalescing: both user_message_chunk fragments landed in one message.
    assert_eq!(
        session.messages[0].content,
        "Run the shell command: echo hi . Then reply with exactly: done"
    );
    // Thought chunk is a separate reasoning-authored assistant message.
    assert_eq!(session.messages[1].author.as_deref(), Some("reasoning"));
    // tool_call + tool_call_update merged into the same assistant message,
    // without breaking agent_message_chunk coalescing around them.
    let tool_msg = &session.messages[2];
    assert_eq!(tool_msg.content, "Running the command now. done");
    assert_eq!(tool_msg.tool_calls.len(), 1);
    assert_eq!(tool_msg.tool_calls[0].id.as_deref(), Some("call_1"));
    assert_eq!(tool_msg.tool_calls[0].name, "Run echo hi");
    assert_eq!(tool_msg.tool_results.len(), 1);
    assert_eq!(tool_msg.tool_results[0].content, "hi\n");
    assert!(!tool_msg.tool_results[0].is_error);

    // Resume command uses the documented flag.
    assert_eq!(
        Grok.resume_command(&session.session_id),
        "grok --resume 019f75d0-aaaa-7bbb-8ccc-b0a1b2c3d4e5"
    );

    // The generated title is surfaced as the provider-native session name.
    assert_eq!(
        casr::model::native_name_from_metadata(&session.metadata).as_deref(),
        Some("Echo hi probe session")
    );
}

// ---------------------------------------------------------------------------
// Antigravity (agy) fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_agy_simple() {
    let path = fixtures_dir()
        .join("antigravity/antigravity-cli/conversations/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.db");
    let session = Antigravity
        .read_session(&path)
        .expect("agy_simple should parse");
    let expected = load_expected("agy_simple");
    assert_session_matches(&session, &expected, "agy_simple");

    // Extra: the resume command is the conversation id and nothing else.
    // `agy --help` (1.1.7) documents `--conversation` for resume; `--model`
    // takes a slug and `--effort` is a separate low|medium|high flag, and casr
    // has no model information in the transcript to pin either from.
    let resume = Antigravity.resume_command(&session.session_id);
    assert_eq!(
        resume,
        "agy --conversation aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    );

    // Extra: the tool-only planner step extracted its tool call.
    assert!(
        session
            .messages
            .iter()
            .any(|m| m.tool_calls.iter().any(|tc| tc.name == "view_file")),
        "agy_simple should surface the view_file tool call"
    );
}

/// Disambiguation: a legacy gmi `tmp/.../chats/session-*.json` sibling under the
/// SAME `~/.gemini`-equivalent parent must NOT be enumerated by the agy provider.
#[test]
fn fixture_agy_does_not_list_legacy_gmi_sessions() {
    let gemini_home = fixtures_dir().join("antigravity");
    // SAFETY: env mutation in a test; casr fixture tests use HOME overrides.
    unsafe {
        std::env::set_var("GEMINI_HOME", &gemini_home);
    }

    let sessions = Antigravity
        .list_sessions()
        .expect("agy list_sessions returns Some");
    let ids: Vec<String> = sessions.sessions.into_iter().map(|(id, _)| id).collect();

    assert!(
        ids.contains(&"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()),
        "agy should list its own conversation uuid: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.contains("gmi-legacy")),
        "agy must NOT list the legacy gmi session: {ids:?}"
    );

    // And the gmi provider must NOT list the agy conversation uuid.
    let gmi_sessions = Gemini
        .list_sessions()
        .expect("gmi list_sessions returns Some");
    let gmi_ids: Vec<String> = gmi_sessions
        .sessions
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        gmi_ids.contains(&"gmi-legacy-001".to_string()),
        "gmi should list its own legacy session: {gmi_ids:?}"
    );
    assert!(
        !gmi_ids
            .iter()
            .any(|id| id.contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
        "gmi must NOT list the agy conversation uuid: {gmi_ids:?}"
    );

    unsafe {
        std::env::remove_var("GEMINI_HOME");
    }
}

// ---------------------------------------------------------------------------
// Cline fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_cline_simple() {
    let path = fixtures_dir().join("cline/tasks/1700001234567/api_conversation_history.json");
    let session = Cline
        .read_session(&path)
        .expect("cline_simple should parse");
    let expected = load_expected("cline_simple");
    assert_session_matches(&session, &expected, "cline_simple");
}

// ---------------------------------------------------------------------------
// Amp fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_amp_simple() {
    let path = fixtures_dir().join("amp/T-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.json");
    let session = Amp.read_session(&path).expect("amp_simple should parse");
    let expected = load_expected("amp_simple");
    assert_session_matches(&session, &expected, "amp_simple");
}

// ---------------------------------------------------------------------------
// Aider fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_aider_simple() {
    let path = fixtures_dir().join("aider/aider_simple.md");
    let session = Aider
        .read_session(&path)
        .expect("aider_simple should parse");
    let expected = load_expected("aider_simple");
    assert_session_matches(&session, &expected, "aider_simple");
}

// ---------------------------------------------------------------------------
// ChatGPT fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_chatgpt_simple() {
    let path = fixtures_dir().join("chatgpt/chatgpt_simple.json");
    let session = ChatGpt
        .read_session(&path)
        .expect("chatgpt_simple should parse");
    let expected = load_expected("chatgpt_simple");
    assert_session_matches(&session, &expected, "chatgpt_simple");
}

// ---------------------------------------------------------------------------
// ClawdBot fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_clawdbot_simple() {
    let path = fixtures_dir().join("clawdbot/clawdbot_simple.jsonl");
    let session = ClawdBot
        .read_session(&path)
        .expect("clawdbot_simple should parse");
    let expected = load_expected("clawdbot_simple");
    assert_session_matches(&session, &expected, "clawdbot_simple");
}

// ---------------------------------------------------------------------------
// Vibe fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_vibe_simple() {
    let path = fixtures_dir().join("vibe/messages.jsonl");
    let session = Vibe.read_session(&path).expect("vibe_simple should parse");
    let expected = load_expected("vibe_simple");
    assert_session_matches(&session, &expected, "vibe_simple");
}

// ---------------------------------------------------------------------------
// Factory fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_factory_simple() {
    let path = fixtures_dir().join("factory/factory_simple.jsonl");
    let session = Factory
        .read_session(&path)
        .expect("factory_simple should parse");
    let expected = load_expected("factory_simple");
    assert_session_matches(&session, &expected, "factory_simple");
}

// ---------------------------------------------------------------------------
// OpenClaw fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_openclaw_simple() {
    let path = fixtures_dir().join("openclaw/openclaw_simple.jsonl");
    let session = OpenClaw
        .read_session(&path)
        .expect("openclaw_simple should parse");
    let expected = load_expected("openclaw_simple");
    assert_session_matches(&session, &expected, "openclaw_simple");

    // Extra: verify tool calls were extracted.
    let tc_msgs: Vec<_> = session
        .messages
        .iter()
        .filter(|m| !m.tool_calls.is_empty())
        .collect();
    assert!(
        !tc_msgs.is_empty(),
        "openclaw_simple should have messages with tool calls"
    );

    let all = session
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // The user rewound this branch away. OpenClaw's own buildSessionContext
    // does not include it, so neither may casr: replaying it would show the
    // target model work the user deleted.
    assert!(
        !all.contains("switch the whole pool to a different crate"),
        "[openclaw_simple] abandoned branch was replayed"
    );
    assert!(
        !all.contains("full replacement built on another crate"),
        "[openclaw_simple] abandoned branch was replayed"
    );

    // Everything below reaches the model and used to be dropped on the floor.
    for needle in [
        // thinking blocks carry `.thinking`, not `.text`
        "The pool has no error path at all",
        // a bashExecution record has no `content` field at all
        "Ran `cargo test db::pool`",
        "Command exited with code 101",
        // an entry-level custom_message "DOES participate in LLM context"
        "Loaded skill: resilient-io",
        // a branch_summary is model-visible
        "Explored replacing the pool crate",
    ] {
        assert!(
            all.contains(needle),
            "[openclaw_simple] model-visible content dropped: {needle:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Pi-Agent fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_piagent_simple() {
    let path = fixtures_dir().join("pi_agent/2025-12-01T10-00-00_pi-uuid-001.jsonl");
    let session = PiAgent
        .read_session(&path)
        .expect("piagent_simple should parse");
    let expected = load_expected("piagent_simple");
    assert_session_matches(&session, &expected, "piagent_simple");

    // Extra: verify toolResult role was normalized to Tool.
    let tool_msgs: Vec<_> = session
        .messages
        .iter()
        .filter(|m| m.role == casr::model::MessageRole::Tool)
        .collect();
    assert!(
        !tool_msgs.is_empty(),
        "piagent_simple should have Tool role messages (from toolResult)"
    );
}

// ---------------------------------------------------------------------------
// SQLite provider fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_cur_simple() {
    let path = fixtures_dir().join("cursor/state.vscdb");
    let session = Cursor.read_session(&path).expect("cur_simple should parse");
    let expected = load_expected("cur_simple");
    assert_session_matches(&session, &expected, "cur_simple");
}

#[test]
fn fixture_opc_simple() {
    let path = fixtures_dir().join("opencode/.opencode/opencode.db");
    let session = OpenCode
        .read_session(&path)
        .expect("opc_simple should parse");
    let expected = load_expected("opc_simple");

    // Workspace is derived from db_path parent — skip the assertion in
    // assert_session_matches by patching expected to match the actual value.
    let expected_workspace = fixtures_dir().join("opencode");
    assert_eq!(
        session.workspace.as_deref(),
        Some(expected_workspace.as_path()),
        "[opc_simple] workspace should be parent of .opencode/"
    );
    let mut patched = expected.clone();
    patched["workspace"] =
        serde_json::Value::String(expected_workspace.to_string_lossy().into_owned());
    assert_session_matches(&session, &patched, "opc_simple");
}

/// The `opc_simple` fixture above pins the *legacy* OpenCode layout, and it
/// pins it at `<workspace>/.opencode/opencode.db`. Both of those facts are
/// historical: current OpenCode names its tables `session`/`message`/`part` and
/// keeps the file in its XDG data directory, so a reader that only satisfies
/// `opc_simple` reads nothing a current install has.
///
/// This fixture is a real database: it was produced by running the released
/// `opencode-linux-x64` 1.18.6 binary and letting its own `opencode import`
/// write the rows, then vacuuming it and rewriting the two absolute paths this
/// machine had baked into `session.directory` and `project.worktree`. Nothing
/// about the schema or the JSON payloads is reconstructed.
///
/// Note the path: no `.opencode/` component. The layout is detected from the
/// database, so the file's location must not be what decides it.
#[test]
fn fixture_opc_current_schema() {
    let path = fixtures_dir().join("opencode-current/opencode.db");
    let session = OpenCode
        .read_session(&path)
        .expect("a current-schema OpenCode database should parse");

    assert_eq!(session.session_id, "ses_imported000000000000001");
    assert_eq!(session.title.as_deref(), Some("Fix OpenCode adapter"));
    assert_eq!(
        session.metadata["opencode_schema"], "session/message/part",
        "the reader should report which layout it actually used"
    );

    // The current schema records the session's own directory, so the workspace
    // comes from the row rather than from where the file happens to sit.
    assert_eq!(
        session.workspace.as_deref(),
        Some(Path::new("/workspace/opencode-fixture")),
        "workspace should come from session.directory, not the db path"
    );

    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, MessageRole::User);
    assert_eq!(session.messages[0].content, "Please inspect src/main.rs");

    // Content comes from the `text` part; the `reasoning` and `step-start`
    // parts in the same message must not leak into it.
    assert_eq!(session.messages[1].role, MessageRole::Assistant);
    assert_eq!(session.messages[1].content, "Inspecting now.");

    // `tool` parts carry the call and its result together.
    let calls = &session.messages[1].tool_calls;
    assert_eq!(calls.len(), 2, "two tool parts in the assistant message");
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].id.as_deref(), Some("call-1"));
    assert_eq!(calls[0].arguments["filePath"], "src/main.rs");
    assert_eq!(calls[1].name, "bash");

    let results = &session.messages[1].tool_results;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "Read complete");
    assert!(!results[0].is_error);
    assert_eq!(results[1].content, "exit status 1");
    assert!(
        results[1].is_error,
        "an errored tool part is an error result"
    );

    assert_eq!(session.model_name.as_deref(), Some("gpt-5"));
    assert_eq!(session.metadata["prompt_tokens"], 1234);
    assert_eq!(session.metadata["completion_tokens"], 567);
}

/// Both layouts must stay readable, and the legacy one must keep reporting
/// itself as legacy rather than being quietly read through the new path.
#[test]
fn fixture_opc_both_layouts_are_readable() {
    let legacy = OpenCode
        .read_session(&fixtures_dir().join("opencode/.opencode/opencode.db"))
        .expect("legacy layout should still parse");
    let current = OpenCode
        .read_session(&fixtures_dir().join("opencode-current/opencode.db"))
        .expect("current layout should parse");

    assert_eq!(
        legacy.metadata["opencode_schema"],
        "sessions/messages/files"
    );
    assert_eq!(current.metadata["opencode_schema"], "session/message/part");
    assert!(!legacy.messages.is_empty());
    assert!(!current.messages.is_empty());
}

// ---------------------------------------------------------------------------
// Edge-case fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_edge_empty_content_cc() {
    let path = fixtures_dir().join("edge/edge_empty_content_cc.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("edge_empty_content should parse");
    let expected = load_expected("edge_empty_content_cc");
    assert_session_matches(&session, &expected, "edge_empty_content_cc");
}

#[test]
fn fixture_edge_null_timestamps_cc() {
    let path = fixtures_dir().join("edge/edge_null_timestamps_cc.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("edge_null_timestamps should parse");
    let expected = load_expected("edge_null_timestamps_cc");
    assert_session_matches(&session, &expected, "edge_null_timestamps_cc");

    // Extra: all message timestamps should be None.
    for msg in &session.messages {
        assert!(
            msg.timestamp.is_none(),
            "All message timestamps should be None in edge_null_timestamps fixture"
        );
    }
}

#[test]
fn fixture_edge_long_message_cc() {
    let path = fixtures_dir().join("edge/edge_long_message_cc.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("edge_long_message should parse");
    let expected = load_expected("edge_long_message_cc");
    assert_session_matches(&session, &expected, "edge_long_message_cc");

    // Extra: verify the long content is preserved without truncation.
    assert!(
        session.messages[0].content.len() > 900,
        "Long message should be preserved without truncation"
    );
    assert!(
        session.messages[0].content.contains("end of long content."),
        "Long message should contain the full text including end marker"
    );
}

#[test]
fn fixture_edge_single_sided_cc() {
    let path = fixtures_dir().join("edge/edge_single_sided_cc.jsonl");
    let session = ClaudeCode
        .read_session(&path)
        .expect("edge_single_sided should parse");
    let expected = load_expected("edge_single_sided_cc");
    assert_session_matches(&session, &expected, "edge_single_sided_cc");

    // Extra: verify validation flags this as an error.
    let validation = casr::pipeline::validate_session(&session);
    assert!(
        validation.has_errors(),
        "Single-sided session should produce validation errors"
    );
}

// ---------------------------------------------------------------------------
// Manifest integrity test
// ---------------------------------------------------------------------------

#[test]
fn manifest_all_fixtures_have_expected_files() {
    let manifest_path = fixtures_dir().join("fixtures_manifest.json");
    let content = std::fs::read_to_string(&manifest_path).expect("manifest should exist");
    let manifest: serde_json::Value =
        serde_json::from_str(&content).expect("manifest should be valid JSON");

    let fixtures = manifest["fixtures"]
        .as_object()
        .expect("fixtures should be an object");
    for (fixture_id, entry) in fixtures {
        let fixture_path = fixtures_dir().join(entry["path"].as_str().unwrap());
        assert!(
            fixture_path.exists(),
            "Fixture file missing for {fixture_id}: {}",
            fixture_path.display()
        );

        // Most fixtures have a golden. One does not, and cannot: a
        // `cursor-agent` chat store's correct outcome is a *read failure* that
        // reaches `list`'s `skipped` list, and there is no canonical session to
        // write down. An entry may therefore omit `expected` only by saying so,
        // which keeps "no golden" a decision someone made rather than one
        // someone forgot.
        match (entry.get("expected"), entry.get("no_expected_because")) {
            (Some(expected), _) => {
                let expected_path = fixtures_dir().join(expected.as_str().unwrap());
                assert!(
                    expected_path.exists(),
                    "Expected file missing for {fixture_id}: {}",
                    expected_path.display()
                );
            }
            (None, Some(reason)) => assert!(
                reason.as_str().is_some_and(|r| !r.trim().is_empty()),
                "{fixture_id} omits `expected` but its `no_expected_because` is empty"
            ),
            (None, None) => panic!(
                "{fixture_id} has no `expected` golden and no `no_expected_because` saying why"
            ),
        }

        // Provenance is not optional for a fixture that claims to be evidence
        // about a tool. `synthetic` and `edge` fixtures are openly written for
        // casr and claim nothing; `real` and `schema-derived` both assert that
        // the bytes came from somewhere outside this repository, and an
        // assertion like that has to say where — otherwise the next reader
        // cannot tell a capture from a guess, which is how a provider ends up
        // with fixtures that encode casr's expectations instead of the tool's
        // behaviour.
        if matches!(
            entry["category"].as_str(),
            Some("real") | Some("schema-derived")
        ) {
            assert!(
                entry
                    .get("provenance")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| !p.trim().is_empty()),
                "{fixture_id} is category {} but records no provenance",
                entry["category"]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Second stores: Kiro IDE and cursor-agent
//
// Each of these tools keeps two session stores under one home directory, and
// `detect()` fires on the home directory. Reading only one of them therefore
// renders as "installed, 0 sessions", which a user cannot tell apart from
// having none. These fixtures are the store that was not being read.
// ---------------------------------------------------------------------------

/// Lay a Kiro IDE session out the way `SessionPersistence.saveSession` does:
/// `<root>/<bucket>/<sess_id>/{session.json,messages.jsonl}`.
fn seed_kiro_ide_session(root: &Path, bucket: &str, meta: &str, messages: &str) -> PathBuf {
    let meta_json = std::fs::read_to_string(fixtures_dir().join("kiro").join(meta)).unwrap();
    let id: String = serde_json::from_str::<serde_json::Value>(&meta_json).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = root.join(bucket).join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.json"), &meta_json).unwrap();
    std::fs::copy(
        fixtures_dir().join("kiro").join(messages),
        dir.join("messages.jsonl"),
    )
    .unwrap();
    dir.join("session.json")
}

#[test]
fn fixture_kiro_ide_session() {
    let tmp = tempfile::tempdir().unwrap();
    // `b550143462be8201` is not a guess: it is what Kiro's own
    // `workspace-hash-Dq3QXXpU.js` returns for `["/home/u/demo-project"]`.
    let anchor = seed_kiro_ide_session(
        tmp.path(),
        "b550143462be8201",
        "ide_session.json",
        "ide_messages.jsonl",
    );

    let session = Kiro
        .read_session(&anchor)
        .expect("kiro_ide_session should parse");
    let expected = load_expected("kiro_ide_session");
    assert_session_matches(&session, &expected, "kiro_ide_session");

    // Extra: the failing tool result keeps its failure. `success: false` is the
    // only place the outcome is recorded — the payload has no status string.
    let failed: Vec<bool> = session
        .messages
        .iter()
        .flat_map(|m| m.tool_results.iter().map(|r| r.is_error))
        .collect();
    assert_eq!(
        failed,
        vec![false, true],
        "the pytest tool result reported success:false and must read as an error"
    );

    // Extra: reasoning is attributed as reasoning, not as prose from a model.
    assert_eq!(
        session.messages[1].author.as_deref(),
        Some("reasoning"),
        "operationType Reasoning marks the message as reasoning"
    );
}

#[test]
fn fixture_kiro_ide_session_global_has_no_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let anchor = seed_kiro_ide_session(
        tmp.path(),
        "_global",
        "ide_session_global.json",
        "ide_messages_global.jsonl",
    );

    let session = Kiro
        .read_session(&anchor)
        .expect("kiro_ide_session_global should parse");
    let expected = load_expected("kiro_ide_session_global");
    assert_session_matches(&session, &expected, "kiro_ide_session_global");
}

#[test]
fn fixture_cursor_agent_transcript() {
    // The conversation id is the filename, in every one of the three layouts
    // `agent-transcript/dist/paths.js` can produce, so the reader takes it from
    // the stem and the fixture has to be placed under its own id.
    let id = "7a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(format!("{id}.jsonl"));
    std::fs::copy(
        fixtures_dir().join("cursor/cursor_agent_transcript.jsonl"),
        &path,
    )
    .unwrap();

    let session = Cursor
        .read_session(&path)
        .expect("cursor_agent_transcript should parse");
    let expected = load_expected("cursor_agent_transcript");
    assert_session_matches(&session, &expected, "cursor_agent_transcript");

    // Extra: the `metadata` and `turn_ended` lines are not messages. Counting
    // them would inflate every cursor-agent session by two.
    assert_eq!(session.messages.len(), 4);

    // Extra: the transcript records no tool-call id, so none is invented.
    let call = session
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .next()
        .expect("the assistant turn carries a tool_use block");
    assert_eq!(call.name, "read_file");
    assert!(
        call.id.is_none(),
        "the transcript has no tool-call id to report"
    );
}
