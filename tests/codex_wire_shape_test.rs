//! The Codex rollout's *wire shape*, asserted where the IR oracle cannot see it.
//!
//! `tests/roundtrip_ir_test.rs` proves the writer is the inverse of the reader.
//! That is necessary and it is not sufficient, because the two share a parser:
//! [`ags::ir::ToolInput::from_json_field`] maps both `"{\"a\":1}"` and
//! `{"a":1}` onto the same `ToolInput::Json`, so a writer that emitted the
//! object form for `function_call.arguments` round-tripped perfectly — and
//! produced a rollout in which the real `codex` CLI failed the record's decode,
//! dropped every tool call, reported each now-unpaired result as an `Orphan
//! function call output`, and dropped those too. Measured against
//! `codex-cli 0.145.0`, resuming one converted Claude Code session: the model
//! saw 11 prompt items instead of 23, with zero of the session's six tool calls
//! and zero of its six tool results.
//!
//! Nothing in the IR can catch that class, because by the time an event exists
//! the wire shape has already been parsed away. So these tests work on the
//! serialised line, and the contract they check
//! ([`ags::providers::codex_ir_write::wire_contract_violation`]) is derived
//! from the corpus rather than from belief: every field type below is the
//! observed type of that field across a 66,376-payload sample of real Codex
//! rollouts.
//!
//! The corpus tier is `#[ignore]`d and skips rather than fails when the corpus
//! is absent, matching `roundtrip_ir_test.rs`:
//!
//! ```bash
//! AGS_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --test codex_wire_shape_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read.

use std::path::{Path, PathBuf};

use ags::budget::ContextBudget;
use ags::providers::{claude_code_ir, codex_ir, codex_ir_write};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn real_world_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/real_world")
        .join(name)
}

/// Render a source IR to Codex lines and hand back the `response_item`
/// payloads, which is the only part of the file this contract governs.
fn codex_payloads(source_path: &Path) -> Vec<Value> {
    let ir = claude_code_ir::read(source_path).expect("fixture parses");
    let rendered = codex_ir_write::render(
        &ir,
        "wire-shape-session",
        chrono::Utc::now(),
        &ContextBudget::UNLIMITED,
    )
    .expect("the fixture has a replay to write");
    rendered
        .lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("type").and_then(Value::as_str) == Some("response_item"))
        .filter_map(|line| line.get("payload").cloned())
        .collect()
}

fn payloads_of_type<'a>(payloads: &'a [Value], kind: &str) -> Vec<&'a Value> {
    payloads
        .iter()
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some(kind))
        .collect()
}

// ---------------------------------------------------------------------------
// The regression this file exists for
// ---------------------------------------------------------------------------

/// Claude Code records `tool_use.input` as a JSON object and keeps no original
/// argument text, so every Claude Code tool call reaches the Codex writer as
/// `ToolInput::Json { original: None, .. }`. That branch used to write `value`
/// straight through, which is an object where Codex reads a string.
///
/// This asserts the wire type *and* that the string still holds the arguments —
/// a fix that stringified the wrong thing would satisfy the first half alone.
#[test]
fn function_call_arguments_are_a_json_string_on_the_wire() {
    let payloads = codex_payloads(&real_world_fixture("cc_real_world_sanitized.jsonl"));
    let calls = payloads_of_type(&payloads, "function_call");
    assert!(
        !calls.is_empty(),
        "the fixture must still contain function calls for this test to mean anything"
    );

    for call in &calls {
        let arguments = call
            .get("arguments")
            .expect("every function_call carries arguments");
        let text = arguments.as_str().unwrap_or_else(|| {
            panic!(
                "function_call.arguments must be a JSON *string* on the wire; Codex \
                 deserialises it as String and drops the whole record otherwise, taking the \
                 paired function_call_output with it as an orphan. Got: {arguments}"
            )
        });
        let parsed: Value = serde_json::from_str(text)
            .expect("the string must hold the arguments as JSON, not a debug rendering");
        assert!(
            parsed.is_object(),
            "the arguments JSON should still be the object Claude Code recorded, got {parsed}"
        );
    }
}

/// The same rollout, checked field by field against the corpus-derived
/// contract. This is the assertion that generalises: it fails for the next
/// wrong-typed field too, not only for `arguments`.
#[test]
fn every_written_payload_matches_the_codex_wire_contract() {
    let payloads = codex_payloads(&real_world_fixture("cc_real_world_sanitized.jsonl"));
    assert!(!payloads.is_empty(), "the fixture produced no payloads");
    for payload in &payloads {
        if let Some(violation) = codex_ir_write::wire_contract_violation(payload) {
            panic!("{violation}\n  offending payload: {payload}");
        }
    }
}

/// Whatever the writer emits must survive the reader too. Guards against
/// "fixed the wire type, broke the parse" — the stringified arguments have to
/// come back as the same arguments.
#[test]
fn the_stringified_arguments_still_read_back() {
    let source =
        claude_code_ir::read(&real_world_fixture("cc_real_world_sanitized.jsonl")).expect("parses");
    let rendered = codex_ir_write::render(
        &source,
        "wire-shape-session",
        chrono::Utc::now(),
        &ContextBudget::UNLIMITED,
    )
    .expect("renders");

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, format!("{}\n", rendered.lines.join("\n"))).expect("write");

    let back = codex_ir::read(&path).expect("the written rollout parses");
    let source_calls = tool_call_arguments(&source);
    let back_calls = tool_call_arguments(&back);
    assert_eq!(
        source_calls, back_calls,
        "stringifying the arguments must not change them"
    );
    assert!(
        !source_calls.is_empty(),
        "the fixture must still contain tool calls"
    );
}

fn tool_call_arguments(ir: &ags::ir::SessionIr) -> Vec<Value> {
    ir.model_visible()
        .iter()
        .filter_map(|event| match &event.body {
            ags::ir::Body::ToolCall { input, .. } => match input {
                ags::ir::ToolInput::Json { value, .. } => Some(value.clone()),
                ags::ir::ToolInput::Freeform { text } => Some(json!(text)),
            },
            ags::ir::Body::Message { .. }
            | ags::ir::Body::Reasoning { .. }
            | ags::ir::Body::ToolResult { .. }
            | ags::ir::Body::Compaction { .. }
            | ags::ir::Body::SealedContext { .. }
            | ags::ir::Body::TurnConfig { .. }
            | ags::ir::Body::EnvSnapshot { .. }
            | ags::ir::Body::Attachment { .. }
            | ags::ir::Body::Rollback { .. }
            | ags::ir::Body::Abort { .. }
            | ags::ir::Body::Control { .. }
            | ags::ir::Body::Unknown { .. } => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The contract itself
// ---------------------------------------------------------------------------

#[test]
fn wire_contract_rejects_the_object_arguments_codex_drops() {
    let bad = json!({
        "type": "function_call",
        "call_id": "toolu_01",
        "name": "Bash",
        "arguments": { "command": "ls" },
    });
    let violation =
        codex_ir_write::wire_contract_violation(&bad).expect("an object in arguments is a defect");
    assert!(
        violation.contains("function_call.arguments"),
        "the violation must name the field: {violation}"
    );
    assert!(
        violation.contains("string"),
        "the violation must say what Codex wanted: {violation}"
    );
}

#[test]
fn wire_contract_accepts_the_shapes_codex_itself_writes() {
    // Each of these is the shape observed in the local corpus.
    let good = [
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
        json!({"type": "function_call", "call_id": "c1", "name": "shell",
               "arguments": "{\"command\":\"ls\"}", "namespace": "direct"}),
        json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        json!({"type": "function_call_output", "call_id": "c1", "output": [{"type": "text"}]}),
        json!({"type": "custom_tool_call", "call_id": "c2", "name": "exec",
               "input": "ls -la", "status": "completed"}),
        json!({"type": "custom_tool_call_output", "call_id": "c2", "output": "done"}),
        json!({"type": "reasoning", "summary": [], "encrypted_content": "gAAA"}),
    ];
    for payload in good {
        assert_eq!(
            codex_ir_write::wire_contract_violation(&payload),
            None,
            "a shape Codex itself writes must pass: {payload}"
        );
    }
}

#[test]
fn wire_contract_reports_a_payload_type_it_does_not_know() {
    // A payload type added to the writer with no contract row is unverified,
    // which is the state that let the original defect through. It is reported
    // rather than waved past.
    let unknown = json!({"type": "local_shell_call", "call_id": "c9"});
    let violation = codex_ir_write::wire_contract_violation(&unknown)
        .expect("an unlisted payload type is unverified, not fine");
    assert!(violation.contains("local_shell_call"), "{violation}");
}

#[test]
fn wire_contract_rejects_a_non_object_turn_passthrough() {
    let bad = json!({
        "type": "message",
        "role": "user",
        "content": [],
        "internal_chat_message_metadata_passthrough": "t1",
    });
    let violation = codex_ir_write::wire_contract_violation(&bad)
        .expect("the passthrough is an object on all 319,184 corpus payloads that carry it");
    assert!(violation.contains("passthrough"), "{violation}");
}

// ---------------------------------------------------------------------------
// The history channel
// ---------------------------------------------------------------------------

/// Codex reconstructs its transcript from `event_msg`, not from
/// `response_item`. Measured against `codex-cli 0.145.0`: `thread/read` on a
/// real rollout returns 4 turns / 22 items, and on a rollout carrying only
/// `response_item` lines it returns 0 / 0 — a session that resumes with its
/// full model context and displays as blank.
#[test]
fn the_history_channel_carries_the_messages() {
    let payloads = event_msg_payloads(&codex_lines_from_claude_fixture());
    let kinds: Vec<&str> = payloads
        .iter()
        .filter_map(|payload| payload.get("type").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"user_message"),
        "the history view needs the user's turns: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent_message"),
        "the history view needs the agent's turns: {kinds:?}"
    );
}

/// The rule that governs this channel: restate what the IR knows, never
/// synthesise what it does not. These four are the records Codex writes that
/// casr cannot derive without asserting timings it never saw or a
/// sub-agent/MCP provenance no cross-agent source records.
#[test]
fn the_history_channel_invents_nothing() {
    let payloads = event_msg_payloads(&codex_lines_from_claude_fixture());
    for forbidden in [
        "token_count",
        "task_started",
        "task_complete",
        "turn_aborted",
        "sub_agent_activity",
        "mcp_tool_call_end",
    ] {
        assert!(
            !payloads
                .iter()
                .any(|payload| payload.get("type").and_then(Value::as_str) == Some(forbidden)),
            "'{forbidden}' cannot be derived from the IR; emitting one would put a history in \
             front of the user that did not happen"
        );
    }
    // `phase` is "commentary" or "final_answer" in real rollouts and is a
    // property of the original run. Omitted rather than guessed — and Codex
    // reads the record anyway, measured via `thread/read`.
    for payload in &payloads {
        if payload.get("type").and_then(Value::as_str) == Some("agent_message") {
            assert!(
                payload.get("phase").is_none(),
                "phase is not derivable and must not be invented: {payload}"
            );
        }
    }
}

/// A compaction boundary reaches the history channel exactly when the replay
/// still carries the sealed context — which is also when the writer emits its
/// "history is missing" marker. It cannot be keyed on `Body::Compaction`:
/// that is a history *directive*, consumed by the resolver to rewrite the
/// history rather than replayed as part of it, so it never reaches
/// `model_visible` and an arm on it would be unreachable.
///
/// The payload is `{"type": "context_compacted"}` and nothing else on all 388
/// corpus occurrences, so there is no field left to invent either.
#[test]
fn a_sealed_compaction_boundary_reaches_the_history_channel() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source_path = dir.path().join("source.jsonl");
    std::fs::write(
        &source_path,
        [
            r#"{"timestamp":"2026-07-25T10:00:00.000Z","type":"session_meta","payload":{"id":"s1","timestamp":"2026-07-25T10:00:00.000Z","cwd":"/tmp","originator":"codex-tui","cli_version":"0.145.0"}}"#,
            r#"{"timestamp":"2026-07-25T10:00:07.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#,
            // A sealed replacement entry: `type: "compaction"` with an
            // `encrypted_content` blob and no role, which is 4,325 of the
            // corpus's replacement entries and reads as `Body::SealedContext`.
            r#"{"timestamp":"2026-07-25T10:00:09.000Z","type":"compacted","payload":{"window_id":"w2","previous_window_id":"w1","replacement_history":[{"type":"compaction","encrypted_content":"gAAAAABsealed"}]}}"#,
            r#"{"timestamp":"2026-07-25T10:00:10.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}}"#,
            "",
        ]
        .join("\n"),
    )
    .expect("write source");

    let ir = codex_ir::read(&source_path).expect("the source rollout parses");
    let rendered = codex_ir_write::render(
        &ir,
        "history-session",
        chrono::Utc::now(),
        &ContextBudget::UNLIMITED,
    )
    .expect("renders");

    let payloads = event_msg_payloads(&rendered.lines);
    let found: Vec<&Value> = payloads
        .iter()
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("context_compacted"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "the compaction the source recorded must reach the history channel exactly once, got \
         {payloads:?}"
    );
    assert_eq!(
        found[0].as_object().map(|map| map.len()),
        Some(1),
        "context_compacted carries only its type on all 388 corpus occurrences; anything else \
         here would be invented: {}",
        found[0]
    );
}

/// Silence about the gap would be its own defect: the user would find out by
/// looking at a transcript with the work missing from it.
#[test]
fn the_conversion_reports_the_history_it_could_not_derive() {
    let source =
        claude_code_ir::read(&real_world_fixture("cc_real_world_sanitized.jsonl")).expect("parses");
    let rendered = codex_ir_write::render(
        &source,
        "history-session",
        chrono::Utc::now(),
        &ContextBudget::UNLIMITED,
    )
    .expect("renders");
    assert!(
        rendered
            .warnings
            .iter()
            .any(|note| note.contains("history view") && note.contains("tool call")),
        "a session with tool calls must say its Codex transcript will not show them: {:?}",
        rendered.warnings
    );
    // The model's context is complete, so this must not move the grade.
    assert!(
        !rendered
            .losses
            .iter()
            .any(|loss| loss.note.contains("history view")),
        "the history gap is a warning, not a loss: it must not downgrade fidelity"
    );
}

fn codex_lines_from_claude_fixture() -> Vec<String> {
    let ir =
        claude_code_ir::read(&real_world_fixture("cc_real_world_sanitized.jsonl")).expect("parses");
    codex_ir_write::render(
        &ir,
        "history-session",
        chrono::Utc::now(),
        &ContextBudget::UNLIMITED,
    )
    .expect("renders")
    .lines
}

fn event_msg_payloads(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("type").and_then(Value::as_str) == Some("event_msg"))
        .filter_map(|line| line.get("payload").cloned())
        .collect()
}

// ---------------------------------------------------------------------------
// Corpus tier
// ---------------------------------------------------------------------------

fn claude_corpus() -> Vec<PathBuf> {
    let Ok(root) = std::env::var("AGS_CLAUDE_CORPUS") else {
        eprintln!("AGS_CLAUDE_CORPUS unset; skipping");
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter(|path| is_claude_transcript(path))
        .collect();
    files.sort();
    files.truncate(120);
    files
}

fn is_claude_transcript(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if stem.starts_with("agent-") {
        return true;
    }
    let groups: Vec<&str> = stem.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && stem.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

/// The fixture is one session. This is the population: every real Claude Code
/// transcript on the machine, converted to Codex, every emitted payload checked.
#[test]
#[ignore = "requires the local Claude corpus"]
fn corpus_claude_to_codex_conversions_all_match_the_wire_contract() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }
    let mut sessions = 0usize;
    let mut payloads = 0usize;
    let mut calls = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for path in &files {
        let Ok(ir) = claude_code_ir::read(path) else {
            continue;
        };
        let Some(rendered) = codex_ir_write::render(
            &ir,
            "wire-shape-corpus",
            chrono::Utc::now(),
            &ContextBudget::UNLIMITED,
        ) else {
            continue;
        };
        sessions += 1;
        for line in &rendered.lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let Some(payload) = value.get("payload") else {
                continue;
            };
            payloads += 1;
            if payload.get("type").and_then(Value::as_str) == Some("function_call") {
                calls += 1;
            }
            if let Some(violation) = codex_ir_write::wire_contract_violation(payload)
                && violations.len() < 20
            {
                violations.push(format!("{}: {violation}", path.display()));
            }
        }
    }

    eprintln!(
        "wire contract: {sessions} sessions, {payloads} payloads, {calls} function calls, \
         {} violations",
        violations.len()
    );
    assert!(
        violations.is_empty(),
        "payloads the real Codex CLI would drop:\n{}",
        violations.join("\n")
    );
    assert!(calls > 0, "the corpus must exercise function calls at all");
}
