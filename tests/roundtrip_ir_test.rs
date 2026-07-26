//! Structured round trips: native → IR → native → IR, compared structurally.
//!
//! A writer is correct exactly when it is the inverse of the reader it targets,
//! and nothing short of the real corpus establishes that. Fixtures prove the
//! writer runs; 592 Codex rollouts and 52 Claude transcripts prove it does not
//! quietly drop the third-most-common payload type.
//!
//! Same contract as `replay_test.rs`: the corpus tests are `#[ignore]`d because
//! the corpus is machine-local and private, and they skip rather than fail when
//! it is absent. Run them explicitly:
//!
//! ```bash
//! AGSX_CODEX_CORPUS="$HOME/.codex/sessions" \
//! AGSX_CLAUDE_CORPUS="$HOME/.claude/projects" \
//!   cargo test --release --test roundtrip_ir_test -- --ignored --nocapture
//! ```
//!
//! The corpus is only ever read. Every write in this file goes to a temp
//! directory; nothing here touches `~/.codex` or `~/.claude`.
//!
//! # What "structurally" means
//!
//! Model-visible content: body kind, role, content blocks, tool identity and
//! calling convention, outcome, structured companion, and capsules byte for
//! byte. Not ids, parents, timestamps or turn ids — those are re-minted by
//! construction, because the target session is a new session.
//!
//! Same-agent must be exactly equal. Cross-agent must lose the capsules
//! [`casr::ir::Capsule::fits`] predicted and account for everything else, which
//! is why the cross-agent tests report a tally rather than assert a boolean.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use casr::budget::ContextBudget;
use casr::ir::{Block, Body, Fidelity, Role, SessionIr, ToolInput, ToolOutcome};
use casr::providers::{
    Provider, WriteOptions, claude_code_ir, claude_code_ir_write, codex_ir, codex_ir_write,
};

// ---------------------------------------------------------------------------
// Corpus discovery (same discriminators as `replay_test.rs`)
// ---------------------------------------------------------------------------

fn corpus_files(env_var: &str, limit: usize) -> Vec<PathBuf> {
    let Ok(root) = std::env::var(env_var) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files.truncate(limit);
    files
}

/// The Claude projects tree also holds workflow journals that share the
/// extension; only uuid-named files and `agent-*` are transcripts.
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

fn codex_corpus() -> Vec<PathBuf> {
    let files = corpus_files("AGSX_CODEX_CORPUS", 600);
    if files.is_empty() {
        eprintln!("AGSX_CODEX_CORPUS unset or empty; skipping");
    }
    files
}

fn claude_corpus() -> Vec<PathBuf> {
    let files: Vec<PathBuf> = corpus_files("AGSX_CLAUDE_CORPUS", 800)
        .into_iter()
        .filter(|path| is_claude_transcript(path))
        .take(200)
        .collect();
    if files.is_empty() {
        eprintln!("AGSX_CLAUDE_CORPUS unset or empty; skipping");
    }
    files
}

// ---------------------------------------------------------------------------
// Write to a temp file, read back
// ---------------------------------------------------------------------------

/// Render `ir` as Codex and parse the result. `None` when the replay is empty.
fn through_codex(ir: &SessionIr) -> Option<(SessionIr, Fidelity)> {
    let rendered = codex_ir_write::render(ir, "roundtrip-session", chrono::Utc::now(), &ContextBudget::UNLIMITED)?;
    Some((reparse(&rendered.lines, codex_ir::read), rendered.fidelity))
}

/// Render `ir` as Claude Code and parse the result.
fn through_claude(ir: &SessionIr) -> Option<(SessionIr, Fidelity)> {
    let rendered = claude_code_ir_write::render(ir, "roundtrip-session", chrono::Utc::now(), &ContextBudget::UNLIMITED)?;
    Some((reparse(&rendered.lines, claude_code_ir::read), rendered.fidelity))
}

fn reparse(
    lines: &[String],
    read: fn(&Path) -> anyhow::Result<SessionIr>,
) -> SessionIr {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    for line in lines {
        writeln!(file, "{line}").expect("write");
    }
    file.flush().expect("flush");
    read(file.path()).unwrap_or_else(|error| {
        panic!("the writer produced a session its own reader rejects: {error}")
    })
}

// ---------------------------------------------------------------------------
// Structural shapes
// ---------------------------------------------------------------------------

/// One model-visible event, reduced to the content a conversion must preserve.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    kind: &'static str,
    /// Role for a message, tool name for a call, `""` otherwise.
    label: String,
    /// Calling convention for a tool call, outcome for a result.
    protocol: String,
    /// Content blocks, canonicalised.
    blocks: Vec<String>,
    /// Tool arguments or the structured companion, canonicalised.
    payload: Option<String>,
    /// `(kind, sealed)` for each capsule, byte for byte.
    capsules: Vec<(String, String)>,
}

fn items(ir: &SessionIr) -> Vec<Item> {
    ir.model_visible()
        .iter()
        .map(|event| {
            let capsules = event
                .capsules
                .iter()
                .map(|capsule| (format!("{:?}", capsule.kind), capsule.sealed.clone()))
                .collect();
            let mut item = match &event.body {
                Body::Message { role, blocks } => Item {
                    kind: "message",
                    label: format!("{role:?}"),
                    protocol: String::new(),
                    blocks: blocks.iter().map(canonical_block).collect(),
                    payload: None,
                    capsules,
                },
                Body::Reasoning { text, summary } => Item {
                    kind: "reasoning",
                    label: text.clone().unwrap_or_default(),
                    protocol: String::new(),
                    blocks: summary.clone(),
                    payload: None,
                    capsules,
                },
                Body::ToolCall {
                    call_id,
                    name,
                    namespace,
                    input,
                } => Item {
                    kind: "tool_call",
                    label: format!("{name}@{call_id}"),
                    protocol: format!("{:?}/{namespace:?}", input.protocol()),
                    blocks: Vec::new(),
                    payload: Some(canonical_input(input)),
                    capsules,
                },
                Body::ToolResult {
                    call_id,
                    outcome,
                    output,
                    structured,
                } => Item {
                    kind: "tool_result",
                    label: call_id.clone(),
                    protocol: format!("{outcome:?}"),
                    blocks: output.iter().map(canonical_block).collect(),
                    payload: structured.as_ref().map(ToString::to_string),
                    capsules,
                },
                Body::SealedContext { native_id, .. } => Item {
                    kind: "sealed_context",
                    label: native_id.clone().unwrap_or_default(),
                    protocol: String::new(),
                    blocks: Vec::new(),
                    payload: None,
                    capsules,
                },
                other => Item {
                    kind: other.kind(),
                    label: String::new(),
                    protocol: String::new(),
                    blocks: Vec::new(),
                    payload: None,
                    capsules,
                },
            };
            item.label.truncate(200);
            item
        })
        .collect()
}

fn canonical_block(block: &Block) -> String {
    match block {
        Block::Text { text } => format!("text:{text}"),
        Block::Image { url, media_type } => format!("image:{url}:{media_type:?}"),
        Block::Document { data } => format!("document:{data}"),
        Block::Redacted { reason } => format!("redacted:{reason:?}"),
        Block::Unknown { raw, .. } => format!("unknown:{raw}"),
    }
}

fn canonical_input(input: &ToolInput) -> String {
    match input {
        ToolInput::Json { value, original } => format!("json:{value}|{original:?}"),
        ToolInput::Freeform { text } => format!("freeform:{text}"),
    }
}

/// Every text the model could read, as a multiset.
///
/// Reasoning and sealed context are excluded: both are provider-bound by
/// construction, and both are allowed to disappear across a vendor boundary.
fn readable_text(ir: &SessionIr) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in ir.model_visible() {
        let blocks = match &event.body {
            Body::Message { blocks, .. } => blocks,
            Body::ToolResult { output, .. } => output,
            _ => continue,
        };
        for text in blocks.iter().filter_map(Block::as_text) {
            if text.trim().is_empty() {
                continue;
            }
            *counts.entry(text.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// Tallies
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    sessions: usize,
    before: usize,
    after: usize,
    capsules_before: usize,
    capsules_after: usize,
    kinds_before: BTreeMap<&'static str, usize>,
    kinds_after: BTreeMap<&'static str, usize>,
    protocols_before: BTreeMap<String, usize>,
    protocols_after: BTreeMap<String, usize>,
    roles_before: BTreeMap<String, usize>,
    roles_after: BTreeMap<String, usize>,
    structured_before: usize,
    structured_after: usize,
    text_lost: usize,
    grades: BTreeMap<String, usize>,
}

impl Tally {
    fn add(&mut self, source: &SessionIr, target: &SessionIr, grade: Fidelity) {
        self.sessions += 1;
        *self.grades.entry(format!("{grade:?}")).or_insert(0) += 1;

        for (items, kinds, protocols, roles, total, capsules, structured) in [
            (
                items(source),
                &mut self.kinds_before,
                &mut self.protocols_before,
                &mut self.roles_before,
                &mut self.before,
                &mut self.capsules_before,
                &mut self.structured_before,
            ),
            (
                items(target),
                &mut self.kinds_after,
                &mut self.protocols_after,
                &mut self.roles_after,
                &mut self.after,
                &mut self.capsules_after,
                &mut self.structured_after,
            ),
        ] {
            *total += items.len();
            for item in &items {
                *kinds.entry(item.kind).or_insert(0) += 1;
                *capsules += item.capsules.len();
                if item.kind == "tool_call" {
                    *protocols.entry(item.protocol.clone()).or_insert(0) += 1;
                }
                if item.kind == "message" {
                    *roles.entry(item.label.clone()).or_insert(0) += 1;
                }
                if item.kind == "tool_result" && item.payload.is_some() {
                    *structured += 1;
                }
            }
        }

        let before = readable_text(source);
        let after = readable_text(target);
        for (text, count) in &before {
            let kept = after.get(text).copied().unwrap_or(0);
            self.text_lost += count.saturating_sub(kept);
        }
    }

    fn report(&self, label: &str) {
        println!("\n=== {label}: {} sessions", self.sessions);
        println!(
            "  model events   {} -> {}   capsules {} -> {}",
            self.before, self.after, self.capsules_before, self.capsules_after
        );
        println!("  readable text blocks lost: {}", self.text_lost);
        println!("  structured tool results: {} -> {}", self.structured_before, self.structured_after);
        for (name, before, after) in [
            ("kind", &self.kinds_before, &self.kinds_after),
        ] {
            let mut keys: Vec<&&str> = before.keys().chain(after.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                println!(
                    "  {name:<9} {key:<16} {:>7} -> {:>7}",
                    before.get(*key).copied().unwrap_or(0),
                    after.get(*key).copied().unwrap_or(0)
                );
            }
        }
        for (name, before, after) in [
            ("protocol", &self.protocols_before, &self.protocols_after),
            ("role", &self.roles_before, &self.roles_after),
        ] {
            let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                println!(
                    "  {name:<9} {key:<34} {:>7} -> {:>7}",
                    before.get(key).copied().unwrap_or(0),
                    after.get(key).copied().unwrap_or(0)
                );
            }
        }
        println!("  grades: {:?}", self.grades);
    }
}

/// Same-agent: the two item lists must be identical, event for event.
fn assert_identical(path: &Path, source: &SessionIr, target: &SessionIr) {
    let before = items(source);
    let after = items(target);
    assert_eq!(
        before.len(),
        after.len(),
        "{}: {} model-visible events went in, {} came out",
        path.display(),
        before.len(),
        after.len()
    );
    for (index, (want, got)) in before.iter().zip(&after).enumerate() {
        assert_eq!(
            want,
            got,
            "{}: event {index} did not survive its own round trip",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Same-agent round trips
// ---------------------------------------------------------------------------

/// Codex → Codex must be lossless on model-visible content.
///
/// The writer is only ever as good as its inverse property, and the shapes that
/// break it are the rare ones: sealed compaction inside `compacted`, freeform
/// tool calls paired to `custom_tool_call_output`, `tool_search_output`'s
/// catalogue payload, and `encrypted_content` blocks buried in `agent_message`
/// content. All four are in the corpus and none is in a fixture.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_round_trips_into_itself_without_loss() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut tally = Tally::default();
    for path in &files {
        let Ok(source) = codex_ir::read(path) else {
            continue;
        };
        let Some((target, grade)) = through_codex(&source) else {
            continue;
        };
        assert_identical(path, &source, &target);
        tally.add(&source, &target, grade);
    }

    tally.report("codex -> codex");
    assert!(tally.sessions > 0, "no Codex rollouts round-tripped");
    assert_eq!(tally.before, tally.after);
    assert_eq!(
        tally.capsules_before, tally.capsules_after,
        "a same-vendor capsule must survive verbatim"
    );
    assert_eq!(tally.text_lost, 0);
    assert_eq!(
        tally.grades.keys().collect::<Vec<_>>(),
        vec!["ContextComplete"],
        "nothing is lost same-agent, so nothing may be graded as lost"
    );
}

/// Claude Code → Claude Code must be lossless on model-visible content.
///
/// The shape that breaks it here is the record split: one native record becomes
/// up to three events (`thinking`, `tool_use`, coalesced text), so the writer
/// has to put them back into one record without merging two records into one.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_round_trips_into_itself_without_loss() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut tally = Tally::default();
    for path in &files {
        let Ok(source) = claude_code_ir::read(path) else {
            continue;
        };
        let Some((target, grade)) = through_claude(&source) else {
            continue;
        };
        assert_identical(path, &source, &target);
        tally.add(&source, &target, grade);
    }

    tally.report("claude -> claude");
    assert!(tally.sessions > 0, "no Claude transcripts round-tripped");
    assert_eq!(tally.before, tally.after);
    assert_eq!(tally.capsules_before, tally.capsules_after);
    assert_eq!(tally.text_lost, 0);
    assert_eq!(
        tally.grades.keys().collect::<Vec<_>>(),
        vec!["ContextComplete"]
    );
}

// ---------------------------------------------------------------------------
// Cross-agent round trips
// ---------------------------------------------------------------------------

/// Codex → Claude Code loses the capsules `fits()` predicted, and says so.
///
/// Two very different losses ride the same wire here, and conflating them is
/// what [`Fidelity::HistoryIncomplete`] exists to prevent: a dropped reasoning
/// blob costs a train of thought Anthropic would have stripped anyway, while a
/// dropped `SealedContext` blob costs the conversation itself.
#[test]
#[ignore = "requires a local Codex corpus; set AGSX_CODEX_CORPUS"]
fn codex_to_claude_loses_only_what_cannot_cross() {
    let files = codex_corpus();
    if files.is_empty() {
        return;
    }

    let mut tally = Tally::default();
    let mut with_sealed = 0usize;
    for path in &files {
        let Ok(source) = codex_ir::read(path) else {
            continue;
        };
        let sealed = source
            .model_visible()
            .iter()
            .filter(|event| matches!(event.body, Body::SealedContext { .. }))
            .count();
        let Some((target, grade)) = through_claude(&source) else {
            continue;
        };
        if sealed > 0 {
            with_sealed += 1;
            assert_eq!(
                grade,
                Fidelity::HistoryIncomplete,
                "{}: {sealed} sealed compaction(s) were dropped and the grade did not say so",
                path.display()
            );
            let markers = target
                .model_visible()
                .iter()
                .filter(|event| match &event.body {
                    Body::Message { blocks, .. } => blocks
                        .iter()
                        .filter_map(Block::as_text)
                        .any(|text| text.starts_with("[converted by casr]")),
                    _ => false,
                })
                .count();
            assert_eq!(
                markers,
                sealed,
                "{}: a hole in the conversation must be visible, not silent",
                path.display()
            );
        }
        tally.add(&source, &target, grade);
    }

    tally.report("codex -> claude");
    assert!(tally.sessions > 0);
    assert_eq!(
        tally.capsules_after, 0,
        "no OpenAI blob is replayable in a Claude transcript"
    );
    assert_eq!(
        tally.text_lost, 0,
        "reasoning and sealed context may cross-agent be lost; ordinary text may not"
    );
    assert_eq!(
        tally.kinds_after.get("sealed_context").copied().unwrap_or(0),
        0,
        "Claude has no sealed-context record"
    );
    assert!(
        with_sealed > 0,
        "no compacted rollout in the sample; the HistoryIncomplete path went unverified"
    );
    println!("  {with_sealed} rollouts carried sealed context across");
}

/// Claude Code → Codex loses the thinking signatures and keeps everything else.
///
/// The asymmetry with the other direction is real and worth stating: Claude has
/// one calling convention, so nothing is downgraded on the way out, and Claude
/// never seals its history, so no conversation goes missing.
#[test]
#[ignore = "requires a local Claude corpus; set AGSX_CLAUDE_CORPUS"]
fn claude_to_codex_loses_only_the_thinking_signatures() {
    let files = claude_corpus();
    if files.is_empty() {
        return;
    }

    let mut tally = Tally::default();
    for path in &files {
        let Ok(source) = claude_code_ir::read(path) else {
            continue;
        };
        let Some((target, grade)) = through_codex(&source) else {
            continue;
        };
        assert!(
            grade <= Fidelity::ConversationOnly,
            "{}: Claude seals no history, so nothing here should grade worse \
             than a structural downgrade, but the writer reported {grade:?}",
            path.display()
        );
        tally.add(&source, &target, grade);
    }

    tally.report("claude -> codex");
    assert!(tally.sessions > 0);
    assert_eq!(
        tally.capsules_after, 0,
        "no Anthropic signature is replayable in a Codex rollout"
    );
    assert_eq!(tally.text_lost, 0);
    assert_eq!(
        tally.kinds_after.get("reasoning").copied().unwrap_or(0),
        0,
        "a thinking block with no signature is an empty husk, not a reasoning step"
    );
    // Both a call and its output must survive, or the target sees an
    // unanswered tool call at the end of its context.
    assert_eq!(
        tally.kinds_before.get("tool_call"),
        tally.kinds_after.get("tool_call")
    );
    assert_eq!(
        tally.kinds_before.get("tool_result"),
        tally.kinds_after.get("tool_result")
    );
}

// ---------------------------------------------------------------------------
// Fixtures — these run in the normal suite
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/real_world")
        .join(name)
}

/// A rollout covering every `response_item` payload the corpus contains.
///
/// The Codex fixture under `tests/fixtures/real_world` cannot stand in for
/// this: its `response_item` payloads carry no `type`, so the structured reader
/// classifies all 33 of them as `Unclassified` and the replay is empty. That is
/// a fixture that predates the payload envelope, not a writer to test against.
const CODEX_ROLLOUT: &[&str] = &[
    r#"{"type":"session_meta","timestamp":"2026-07-26T10:00:00.000Z","payload":{"id":"019f-test","session_id":"019f-test","cli_version":"0.145.0","model_provider":"sub2api","cwd":"/work","timestamp":"2026-07-26T10:00:00.000Z","git":{"branch":"main","commit_hash":"abc123"}}}"#,
    r#"{"type":"turn_context","timestamp":"2026-07-26T10:00:01.000Z","payload":{"turn_id":"t1","model":"gpt-5-codex","effort":"high","workspace_roots":["/work","/extra"]}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:02.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"please look"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:03.000Z","payload":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"RRRR","internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:04.000Z","payload":{"type":"custom_tool_call","id":"ctc_1","status":"completed","call_id":"c1","name":"shell","input":"ls -la","internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:05.000Z","payload":{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"a.txt"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:06.000Z","payload":{"type":"function_call","id":"fc_1","call_id":"c2","name":"read","namespace":"collaboration","arguments":"{\"path\":\"a.txt\"}","internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:07.000Z","payload":{"type":"function_call_output","call_id":"c2","output":"{\"timed_out\":true}","internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:08.000Z","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"operator instruction"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:09.000Z","payload":{"type":"agent_message","author":"sub","recipient":"main","content":[{"type":"encrypted_content","encrypted_content":"EEEE"},{"type":"output_text","text":"agent says"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:10.000Z","payload":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"query":"rust"},"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:11.000Z","payload":{"type":"tool_search_output","call_id":"ts_1","status":"completed","execution":"local","tools":[{"name":"grep","schema":{"a":1}}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
    r#"{"type":"event_msg","timestamp":"2026-07-26T10:00:12.000Z","payload":{"type":"token_count","info":{}}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:13.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}"#,
];

/// The compaction shape, which is the one that costs a conversation when it is
/// written back wrong.
const CODEX_COMPACTED: &[&str] = &[
    r#"{"type":"session_meta","timestamp":"2026-07-26T10:00:00.000Z","payload":{"id":"019f-cmp","cli_version":"0.145.0","model_provider":"openai","cwd":"/work"}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:01.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"superseded"}]}}"#,
    r#"{"type":"compacted","timestamp":"2026-07-26T10:00:02.000Z","payload":{"window_id":"w2","previous_window_id":"w1","message":"here is the summary","replacement_history":[{"type":"compaction","id":"cmp_1","encrypted_content":"CCCC","internal_chat_message_metadata_passthrough":{"turn_id":"t9"}},{"type":"message","role":"user","content":[{"type":"input_text","text":"preserved"}]}]}}"#,
    r#"{"type":"response_item","timestamp":"2026-07-26T10:00:03.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}}"#,
];

fn synthetic(lines: &[&str]) -> SessionIr {
    let owned: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    reparse(&owned, codex_ir::read)
}

#[test]
fn codex_round_trips_every_payload_type_it_records() {
    let source = synthetic(CODEX_ROLLOUT);
    assert_eq!(
        source.model_visible().len(),
        11,
        "the sample must exercise every model-visible payload shape"
    );
    let (target, grade) = through_codex(&source).expect("non-empty replay");
    assert_identical(Path::new("<synthetic>"), &source, &target);
    assert_eq!(grade, Fidelity::ContextComplete);
    // Header fields the reader sources from `session_meta` and `turn_context`.
    assert_eq!(target.origin.provider.as_deref(), Some("sub2api"));
    assert_eq!(target.origin.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(target.origin.agent_version.as_deref(), Some("0.145.0"));
    assert_eq!(target.workspace.git_branch.as_deref(), Some("main"));
    assert_eq!(target.workspace.git_commit.as_deref(), Some("abc123"));
    assert_eq!(target.workspace.roots.len(), 2);
}

#[test]
fn codex_sealed_compaction_survives_a_same_agent_round_trip() {
    let source = synthetic(CODEX_COMPACTED);
    let (target, grade) = through_codex(&source).expect("non-empty replay");
    assert_identical(Path::new("<synthetic>"), &source, &target);
    assert_eq!(grade, Fidelity::ContextComplete);
    let sealed: Vec<&str> = target
        .model_visible()
        .iter()
        .flat_map(|event| event.capsules.iter())
        .map(|capsule| capsule.sealed.as_str())
        .collect();
    assert_eq!(
        sealed,
        ["CCCC"],
        "87.6 MB of the corpus is this blob; re-encoding or dropping it costs history"
    );
}

#[test]
fn codex_crosses_to_claude_with_its_text_intact() {
    let source = synthetic(CODEX_ROLLOUT);
    let (target, grade) = through_claude(&source).expect("non-empty replay");
    let before = readable_text(&source);
    let after = readable_text(&target);
    for (text, count) in &before {
        assert!(
            after.get(text).copied().unwrap_or(0) >= *count,
            "crossing agents dropped readable text: {text:?}"
        );
    }
    assert_eq!(
        grade,
        Fidelity::ConversationOnly,
        "a freeform call and a developer message both degrade, neither loses history"
    );
    assert!(
        !target
            .model_visible()
            .iter()
            .any(|event| matches!(event.body, Body::Reasoning { .. })),
        "an OpenAI reasoning blob has no Claude counterpart and leaves nothing behind"
    );
    assert!(
        !target.model_visible().iter().any(|event| matches!(
            &event.body,
            Body::Message {
                role: Role::Developer,
                ..
            }
        )),
        "Claude Code has only `user` and `assistant`"
    );
}

#[test]
fn a_lost_sealed_compaction_is_visible_in_the_claude_transcript() {
    let source = synthetic(CODEX_COMPACTED);
    let (target, grade) = through_claude(&source).expect("non-empty replay");
    assert_eq!(grade, Fidelity::HistoryIncomplete);
    let markers = target
        .model_visible()
        .iter()
        .filter(|event| match &event.body {
            Body::Message { blocks, .. } => blocks
                .iter()
                .filter_map(Block::as_text)
                .any(|text| text.starts_with("[converted by casr]")),
            _ => false,
        })
        .count();
    assert_eq!(markers, 1, "omitting the hole silently is the worst option");
}

#[test]
fn claude_fixture_round_trips_into_itself() {
    let path = fixture("cc_real_world_sanitized.jsonl");
    let source = claude_code_ir::read(&path).expect("fixture parses");
    let (target, grade) = through_claude(&source).expect("fixture has a replay");
    assert_identical(&path, &source, &target);
    assert_eq!(grade, Fidelity::ContextComplete);
}

#[test]
fn claude_fixture_crosses_to_codex_with_its_tool_pairs_intact() {
    let source = claude_code_ir::read(&fixture("cc_real_world_sanitized.jsonl")).expect("parses");
    let (target, _) = through_codex(&source).expect("has a replay");
    let calls = |ir: &SessionIr| -> (usize, usize) {
        ir.model_visible().iter().fold((0, 0), |(c, r), event| {
            match &event.body {
                Body::ToolCall { .. } => (c + 1, r),
                Body::ToolResult { .. } => (c, r + 1),
                _ => (c, r),
            }
        })
    };
    assert_eq!(
        calls(&source),
        calls(&target),
        "an unanswered tool call at the end of the context is a broken resume"
    );
}

/// An unrecorded outcome must not become a success.
///
/// Codex writes no success marker on any of the 85,674 tool outputs sampled, so
/// a writer that emits `success: true` by default turns every unknown result in
/// every converted history into a confirmed one.
#[test]
fn an_unknown_tool_outcome_stays_unknown() {
    let source = synthetic(CODEX_ROLLOUT);
    let (target, _) = through_codex(&source).expect("has a replay");
    let outcomes = |ir: &SessionIr| -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for event in ir.model_visible() {
            if let Body::ToolResult { outcome, .. } = &event.body {
                *counts.entry(format!("{outcome:?}")).or_insert(0) += 1;
            }
        }
        counts
    };
    let before = outcomes(&source);
    assert!(
        before.contains_key(&format!("{:?}", ToolOutcome::Unknown)),
        "the fixture must exercise the unknown case"
    );
    assert_eq!(before, outcomes(&target));
}

// ---------------------------------------------------------------------------
// The provider methods themselves
//
// Everything above tests the renderers. These two test the seam: file
// placement, the atomic write, the resume command, and — for Codex — the
// thread-index registration that `codex resume <id>` actually looks the session
// up in. Both point the provider's home at a temp directory; `~/.codex` and
// `~/.claude` are never written to.
// ---------------------------------------------------------------------------

mod test_env;

static ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: guarded by `test_env::EnvLock` for the lifetime of the test.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[test]
fn codex_write_session_ir_lands_where_codex_looks_for_it() {
    let _lock = ENV.lock().expect("env lock");
    let home = tempfile::tempdir().expect("temp home");
    let _guard = EnvGuard::set("CODEX_HOME", home.path());

    let source = synthetic(CODEX_ROLLOUT);
    let written = casr::providers::codex::Codex
        .write_session_ir(&source, &WriteOptions { force: false }, &ContextBudget::UNLIMITED)
        .expect("write")
        .expect("Codex is on the structured track");

    let path = &written.written.paths[0];
    assert!(path.starts_with(home.path().join("sessions")));
    assert!(
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl")),
        "{} does not follow the rollout naming convention",
        path.display()
    );
    assert_eq!(written.fidelity, Fidelity::ContextComplete);
    assert!(
        written
            .written
            .resume_command
            .contains(&written.written.session_id)
    );
    // No `state_*.sqlite` in a temp home, so the session is on disk but
    // unregistered — and the writer has to say so rather than imply a resume
    // that will report "No saved session found".
    assert!(
        written
            .written
            .warnings
            .iter()
            .any(|warning| warning.contains("thread index")),
        "an unregistered session must be reported: {:?}",
        written.written.warnings
    );

    let reread = codex_ir::read(path).expect("the written rollout parses");
    assert_identical(path, &source, &reread);
}

#[test]
fn claude_write_session_ir_lands_in_the_project_directory() {
    let _lock = ENV.lock().expect("env lock");
    let home = tempfile::tempdir().expect("temp home");
    let _guard = EnvGuard::set("CLAUDE_HOME", home.path());

    let source =
        claude_code_ir::read(&fixture("cc_real_world_sanitized.jsonl")).expect("fixture parses");
    let written = casr::providers::claude_code::ClaudeCode
        .write_session_ir(&source, &WriteOptions { force: false }, &ContextBudget::UNLIMITED)
        .expect("write")
        .expect("Claude Code is on the structured track");

    let path = &written.written.paths[0];
    let expected_dir = home.path().join("projects").join(
        casr::providers::claude_code::project_dir_key(
            source.workspace.cwd.as_deref().unwrap_or(Path::new("/tmp")),
        ),
    );
    assert_eq!(path.parent(), Some(expected_dir.as_path()));
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(format!("{}.jsonl", written.written.session_id).as_str())
    );
    assert_eq!(written.fidelity, Fidelity::ContextComplete);
    assert!(written.written.warnings.is_empty());
    assert!(
        std::fs::read_to_string(path).expect("read back").ends_with('\n'),
        "Claude Code appends to this file on resume; a missing final newline \
         corrupts its first appended record"
    );

    let reread = claude_code_ir::read(path).expect("the written transcript parses");
    assert_identical(path, &source, &reread);
}
