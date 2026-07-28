//! Tests for `ConversionPipeline`: mock-based error injection and real-provider
//! integration tests.
//!
//! Mock-based tests (first section) inject controlled failures that real
//! providers can't produce on demand. Real-provider tests (second section)
//! exercise the full pipeline with real CC/Codex/Gemini providers.
//!
//! Every pipeline in this file is built with `store: None`, which is what
//! `--no-store` gives the pipeline. That is not an omission: it means the whole
//! file is a regression suite for the behaviour the store must not change, since
//! with no store there is nothing to consult and `convert` reads the session it
//! was given. The store's own effect on the pipeline is tested in
//! `tests/store_pipeline_test.rs`.

mod test_env;

use std::{
    collections::{BTreeMap, HashMap},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use casr::{
    budget::ContextBudget,
    discovery::{DetectionResult, ProviderRegistry},
    error::CasrError,
    ir::{
        Body, Branch, Capsule, CapsuleBinding, CapsuleKind, Event, Fidelity, LossKind, SessionIr,
        SourceRef, Visibility,
    },
    model::{CanonicalMessage, CanonicalSession, MessageRole, ToolResult},
    pipeline::{ConversionPipeline, ConvertOptions, validate_session},
    providers::antigravity::Antigravity,
    providers::chatgpt::ChatGpt,
    providers::claude_code::ClaudeCode,
    providers::codex::Codex,
    providers::gemini::Gemini,
    providers::grok::Grok,
    providers::opencode::OpenCode,
    providers::{Displaced, Provider, StructuredWrite, WriteOptions, WrittenSession},
};

#[derive(Clone)]
enum ReadOutcome {
    Session(Box<CanonicalSession>),
    Error(String),
}

#[derive(Clone)]
enum WriteOutcome {
    Success(WrittenSession),
    Error(String),
}

#[derive(Clone, Default)]
struct MockState {
    installed: bool,
    owns_by_session_id: HashMap<String, PathBuf>,
    read_by_path: HashMap<PathBuf, ReadOutcome>,
    default_read: Option<ReadOutcome>,
    write_outcome: Option<WriteOutcome>,
    write_calls: usize,
    last_written: Option<CanonicalSession>,
    /// What `read_session_ir` returns. `None` models a provider with no
    /// structured reader, which is the default and the majority.
    ir_read: Option<SessionIr>,
    /// A provider that claims a structured reader whose reader then fails —
    /// a corrupt or truncated rollout, not an absent capability.
    ir_read_error: Option<String>,
    /// `read_session` hands back whatever `write_session` was last given, which
    /// is what a faithful writer/reader pair does. The alternative — a fixed
    /// session registered per path — cannot model a conversion that legitimately
    /// changed the session before writing it.
    read_echo: bool,
    /// Grade `write_session_ir` reports. `None` models a provider with no
    /// structured writer, so the pipeline must fall back to the flat path.
    structured_grade: Option<Fidelity>,
    structured_write_calls: usize,
}

#[derive(Clone)]
struct MockProvider {
    name: String,
    slug: String,
    alias: String,
    roots: Vec<PathBuf>,
    state: Arc<Mutex<MockState>>,
}

impl MockProvider {
    fn new(name: &str, slug: &str, alias: &str, roots: Vec<PathBuf>) -> Self {
        let state = MockState {
            installed: true,
            ..MockState::default()
        };
        Self {
            name: name.to_string(),
            slug: slug.to_string(),
            alias: alias.to_string(),
            roots,
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn set_owned_session(&self, session_id: &str, path: impl Into<PathBuf>) {
        self.state
            .lock()
            .expect("mock state lock")
            .owns_by_session_id
            .insert(session_id.to_string(), path.into());
    }

    fn set_installed(&self, installed: bool) {
        self.state.lock().expect("mock state lock").installed = installed;
    }

    fn set_read_session(&self, path: impl Into<PathBuf>, session: CanonicalSession) {
        self.state
            .lock()
            .expect("mock state lock")
            .read_by_path
            .insert(path.into(), ReadOutcome::Session(Box::new(session)));
    }

    fn set_read_error(&self, path: impl Into<PathBuf>, message: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .read_by_path
            .insert(path.into(), ReadOutcome::Error(message.to_string()));
    }

    fn set_write_success(&self, written: WrittenSession) {
        self.state.lock().expect("mock state lock").write_outcome =
            Some(WriteOutcome::Success(written));
    }

    fn set_write_error(&self, message: &str) {
        self.state.lock().expect("mock state lock").write_outcome =
            Some(WriteOutcome::Error(message.to_string()));
    }

    fn set_structured_read(&self, ir: SessionIr) {
        self.state.lock().expect("mock state lock").ir_read = Some(ir);
    }

    fn set_structured_read_error(&self, message: &str) {
        self.state.lock().expect("mock state lock").ir_read_error = Some(message.to_string());
    }

    fn set_read_echo(&self) {
        self.state.lock().expect("mock state lock").read_echo = true;
    }

    fn set_structured_write(&self, grade: Fidelity) {
        self.state.lock().expect("mock state lock").structured_grade = Some(grade);
    }

    fn write_calls(&self) -> usize {
        self.state.lock().expect("mock state lock").write_calls
    }

    fn structured_write_calls(&self) -> usize {
        self.state
            .lock()
            .expect("mock state lock")
            .structured_write_calls
    }

    fn last_written(&self) -> Option<CanonicalSession> {
        self.state
            .lock()
            .expect("mock state lock")
            .last_written
            .clone()
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn slug(&self) -> &str {
        &self.slug
    }

    fn cli_alias(&self) -> &str {
        &self.alias
    }

    fn detect(&self) -> DetectionResult {
        let installed = self.state.lock().expect("mock state lock").installed;
        DetectionResult {
            installed,
            version: None,
            evidence: vec![format!("installed={installed}")],
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        self.state
            .lock()
            .expect("mock state lock")
            .owns_by_session_id
            .get(session_id)
            .cloned()
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        let state = self.state.lock().expect("mock state lock");
        if state.read_echo {
            return state
                .last_written
                .clone()
                .ok_or_else(|| anyhow::anyhow!("mock echo reader: nothing has been written"));
        }
        if let Some(outcome) = state.read_by_path.get(path).cloned() {
            return match outcome {
                ReadOutcome::Session(session) => Ok(*session),
                ReadOutcome::Error(message) => Err(anyhow::anyhow!(message)),
            };
        }
        if let Some(outcome) = state.default_read.clone() {
            return match outcome {
                ReadOutcome::Session(session) => Ok(*session),
                ReadOutcome::Error(message) => Err(anyhow::anyhow!(message)),
            };
        }
        Err(anyhow::anyhow!(
            "mock provider '{}' has no read outcome for path {}",
            self.slug,
            path.display()
        ))
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        _opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let mut state = self.state.lock().expect("mock state lock");
        state.write_calls += 1;
        state.last_written = Some(session.clone());
        match state.write_outcome.clone() {
            Some(WriteOutcome::Success(written)) => Ok(written),
            Some(WriteOutcome::Error(message)) => Err(anyhow::anyhow!(message)),
            None => Ok(WrittenSession {
                paths: vec![PathBuf::from(format!(
                    "/tmp/{}/mock-output.json",
                    self.slug
                ))],
                session_id: format!("{}-target-session", self.alias),
                resume_command: self.resume_command(&format!("{}-target-session", self.alias)),
                backups: Vec::new(),
                warnings: Vec::new(),
            }),
        }
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("{} --resume {session_id}", self.alias)
    }

    fn read_session_ir(&self, _path: &Path) -> anyhow::Result<Option<SessionIr>> {
        let state = self.state.lock().expect("mock state lock");
        if let Some(message) = state.ir_read_error.clone() {
            return Err(anyhow::anyhow!(message));
        }
        Ok(state.ir_read.clone())
    }

    /// Mirrors what `read_session_ir` above actually does, for the same reason
    /// `supports_structured_write` does: the pipeline asks the probe *instead of*
    /// calling the reader, so a mock whose probe disagrees with its reader models
    /// a provider that cannot exist.
    fn supports_structured_read(&self) -> bool {
        let state = self.state.lock().expect("mock state lock");
        state.ir_read.is_some() || state.ir_read_error.is_some()
    }

    /// Mirrors what `write_session_ir` below will actually do, so the capability
    /// flag cannot drift out of step with the capability in a mock the way it
    /// could in a real provider.
    fn supports_structured_write(&self) -> bool {
        self.state
            .lock()
            .expect("mock state lock")
            .structured_grade
            .is_some()
    }

    fn write_session_ir(
        &self,
        _ir: &SessionIr,
        _opts: &WriteOptions,
        _budget: &ContextBudget,
    ) -> anyhow::Result<Option<StructuredWrite>> {
        let mut state = self.state.lock().expect("mock state lock");
        let Some(fidelity) = state.structured_grade else {
            return Ok(None);
        };
        state.structured_write_calls += 1;
        let session_id = format!("{}-structured-session", self.alias);
        Ok(Some(StructuredWrite {
            written: WrittenSession {
                paths: vec![PathBuf::from(format!("/tmp/{}/structured.json", self.slug))],
                resume_command: self.resume_command(&session_id),
                session_id,
                backups: Vec::new(),
                warnings: vec!["structured writer note".to_string()],
            },
            losses: Vec::new(),
            fidelity,
        }))
    }
}

/// A model-visible IR event, with whatever capsules the case needs.
fn ir_event(id: &str, body: Body, capsules: Vec<Capsule>) -> Event {
    Event {
        id: id.to_string(),
        parent: None,
        branch: Branch::Main,
        turn: None,
        ts: None,
        visibility: Visibility::Model,
        body,
        capsules,
        source: SourceRef {
            line: 1,
            sha256: String::new(),
        },
    }
}

/// An IR holding a single ordinary message — enough to be a structured read
/// without exercising any loss.
fn intact_ir() -> SessionIr {
    let mut ir = SessionIr::new("mock-source", "sid-ir");
    ir.events.push(ir_event(
        "e1",
        Body::Message {
            role: casr::ir::Role::User,
            blocks: vec![casr::ir::Block::Text {
                text: "question one".to_string(),
            }],
        },
        Vec::new(),
    ));
    ir
}

/// An IR whose live context is a sealed compaction: the conversation itself is
/// inside a blob only the issuing vendor can read.
fn sealed_compaction_ir() -> SessionIr {
    let mut ir = intact_ir();
    ir.events.push(ir_event(
        "e2",
        Body::SealedContext {
            native_id: Some("cmp_test_001".to_string()),
            meta: serde_json::Value::Null,
        },
        vec![Capsule {
            kind: CapsuleKind::OpenaiCompactedContext,
            bound: CapsuleBinding {
                provider: "openai".to_string(),
                model: None,
            },
            sealed: "c2VhbGVkLWNvbXBhY3RlZC1oaXN0b3J5".to_string(),
        }],
    ));
    ir
}

fn msg(idx: usize, role: MessageRole, content: &str, ts: Option<i64>) -> CanonicalMessage {
    CanonicalMessage {
        idx,
        role,
        content: content.to_string(),
        timestamp: ts,
        author: None,
        tool_calls: vec![],
        tool_results: vec![],
        extra: serde_json::Value::Null,
    }
}

fn valid_session_with_id(session_id: &str) -> CanonicalSession {
    CanonicalSession {
        session_id: session_id.to_string(),
        provider_slug: "mock-source".to_string(),
        workspace: Some(PathBuf::from("/tmp/mock-workspace")),
        title: Some("Mock session".to_string()),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_020_000),
        messages: vec![
            msg(
                0,
                MessageRole::User,
                "question one",
                Some(1_700_000_000_000),
            ),
            msg(
                1,
                MessageRole::Assistant,
                "answer one",
                Some(1_700_000_005_000),
            ),
            msg(
                2,
                MessageRole::User,
                "question two",
                Some(1_700_000_010_000),
            ),
            msg(
                3,
                MessageRole::Assistant,
                "answer two",
                Some(1_700_000_020_000),
            ),
        ],
        metadata: serde_json::Value::Null,
        source_path: PathBuf::from("/tmp/mock-source.json"),
        model_name: Some("mock-model".to_string()),
    }
}

fn options(dry_run: bool, source_hint: Option<String>) -> ConvertOptions {
    ConvertOptions {
        dry_run,
        force: false,
        verbose: false,
        enrich: false,
        source_hint,
        ..Default::default()
    }
}

#[test]
fn pipeline_convert_happy_path_writes_and_verifies() {
    let src = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let dst = MockProvider::new(
        "Mock Target",
        "mock-target",
        "tgt",
        vec![PathBuf::from("/tmp/tgt-root")],
    );

    let source_path = PathBuf::from("/tmp/src-root/session-a.json");
    let written_path = PathBuf::from("/tmp/tgt-root/session-out.json");
    let session = valid_session_with_id("sid-a");

    src.set_owned_session("sid-a", source_path.clone());
    src.set_read_session(source_path, session.clone());
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-sid-a".to_string(),
        resume_command: "tgt --resume target-sid-a".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });
    dst.set_read_session(written_path, session.clone());

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src.clone()), Box::new(dst.clone())]),
        store: None,
    };

    let result = pipeline
        .convert("tgt", "sid-a", options(false, None))
        .expect("happy path convert should succeed");

    assert_eq!(result.source_provider, "mock-source");
    assert_eq!(result.target_provider, "mock-target");
    assert!(result.written.is_some(), "write result should be present");
    assert!(result.warnings.is_empty(), "happy path should not warn");
    assert_eq!(dst.write_calls(), 1, "target write should run once");
    assert_eq!(
        dst.last_written()
            .expect("target should capture written session")
            .session_id,
        "sid-a"
    );
}

#[test]
fn pipeline_dry_run_skips_write() {
    let src = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let dst = MockProvider::new(
        "Mock Target",
        "mock-target",
        "tgt",
        vec![PathBuf::from("/tmp/tgt-root")],
    );
    let source_path = PathBuf::from("/tmp/src-root/session-b.json");
    src.set_owned_session("sid-b", source_path.clone());
    src.set_read_session(source_path, valid_session_with_id("sid-b"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst.clone())]),
        store: None,
    };

    let result = pipeline
        .convert("tgt", "sid-b", options(true, None))
        .expect("dry-run convert should succeed");

    assert!(result.written.is_none(), "dry-run should skip writes");
    assert_eq!(
        dst.write_calls(),
        0,
        "dry-run should not call write_session"
    );
}

fn assert_pipeline_refuses_target_in_every_mode(target: Box<dyn Provider>) {
    let target_alias = target.cli_alias().to_string();
    let target_slug = target.slug().to_string();
    let reason = target
        .write_refusal()
        .unwrap_or_else(|| panic!("{target_slug} must declare its target refusal"));
    let source = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let source_path = PathBuf::from("/tmp/src-root/refused-target.json");
    source.set_owned_session("sid-refused", source_path.clone());
    source.set_read_session(source_path, valid_session_with_id("sid-refused"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(source), target]),
        store: None,
    };

    for (label, force, dry_run) in [
        ("normal", false, false),
        ("force", true, false),
        ("dry-run", false, true),
    ] {
        let mut opts = options(dry_run, Some("src".to_string()));
        opts.force = force;
        let error = match pipeline.convert(&target_alias, "sid-refused", opts) {
            Ok(_) => panic!("{target_slug} {label} conversion unexpectedly reported success"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            reason,
            "{target_slug} {label} refusal drifted"
        );
    }
}

#[test]
fn every_read_only_target_refuses_normal_force_and_dry_run() {
    let targets: Vec<Box<dyn Provider>> = vec![
        Box::new(Antigravity),
        Box::new(ChatGpt),
        Box::new(Grok),
        Box::new(OpenCode),
    ];
    for target in targets {
        assert_pipeline_refuses_target_in_every_mode(target);
    }
}

#[test]
fn pipeline_same_provider_short_circuit_skips_write() {
    let provider = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let source_path = PathBuf::from("/tmp/src-root/session-same-provider.json");
    provider.set_owned_session("sid-same", source_path.clone());
    provider.set_read_session(source_path, valid_session_with_id("sid-same"));
    provider.set_write_error("write should not be called for same-provider short-circuit");

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(provider.clone())]),
        store: None,
    };

    let result = pipeline
        .convert("src", "sid-same", options(false, None))
        .expect("same-provider conversion should short-circuit");

    assert_eq!(
        provider.write_calls(),
        0,
        "same-provider conversion should not write"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("Source and target provider are the same")),
        "expected same-provider warning; got {:?}",
        result.warnings
    );
    let written = result
        .written
        .expect("same-provider should still return resume metadata");
    assert_eq!(written.paths.len(), 0);
    assert_eq!(written.session_id, "sid-same");
    assert_eq!(
        result.fidelity,
        Fidelity::ByteIdentical,
        "nothing was rewritten; the agent resumes its own bytes"
    );
}

// ---------------------------------------------------------------------------
// Track selection and fidelity grading
// ---------------------------------------------------------------------------

/// A source and target wired for the given session, minus any structured
/// support — the callers below opt into that individually.
fn flat_pair(session_id: &str) -> (MockProvider, MockProvider, ConversionPipeline) {
    pair_with_target_slug(session_id, "mock-target")
}

/// The same pair, with the target's slug chosen by the caller.
///
/// The structural read-back verifier resolves the target's capsule vendor from
/// its slug, because that is the provider that was asked to write rather than
/// something inside the file it produced. A mock called `mock-target` therefore
/// has no known vendor and the comparison is skipped — correct behaviour, and
/// useless for testing the comparison, so the tests that need it borrow a slug
/// the comparator recognises.
fn pair_with_target_slug(
    session_id: &str,
    target_slug: &str,
) -> (MockProvider, MockProvider, ConversionPipeline) {
    let src = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let dst = MockProvider::new(
        "Mock Target",
        target_slug,
        "tgt",
        vec![PathBuf::from("/tmp/tgt-root")],
    );

    let source_path = PathBuf::from(format!("/tmp/src-root/{session_id}.json"));
    let written_path = PathBuf::from(format!("/tmp/tgt-root/{session_id}-out.json"));
    let session = valid_session_with_id(session_id);

    src.set_owned_session(session_id, source_path.clone());
    src.set_read_session(source_path, session.clone());
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: format!("target-{session_id}"),
        resume_command: format!("tgt --resume target-{session_id}"),
        backups: Vec::new(),
        warnings: Vec::new(),
    });
    dst.set_read_session(written_path, session);

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src.clone()), Box::new(dst.clone())]),
        store: None,
    };
    (src, dst, pipeline)
}

#[test]
fn pipeline_takes_the_structured_track_when_both_ends_support_it() {
    let (src, dst, pipeline) = flat_pair("sid-structured");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);

    let result = pipeline
        .convert("tgt", "sid-structured", options(false, None))
        .expect("structured convert should succeed");

    assert_eq!(dst.structured_write_calls(), 1);
    assert_eq!(
        dst.write_calls(),
        0,
        "the flat writer must not also run — that would write the session twice"
    );
    let written = result.written.expect("structured write returns files");
    assert_eq!(written.session_id, "tgt-structured-session");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w == "structured writer note"),
        "the structured writer's warnings must reach the user: {:?}",
        result.warnings
    );
    assert_eq!(
        result.fidelity,
        Fidelity::ContextComplete,
        "the grade is the writer's, carried unchanged"
    );
    assert!(result.losses.is_empty());
}

// ---------------------------------------------------------------------------
// The structured track's read-back verifier
// ---------------------------------------------------------------------------

/// An IR whose reasoning rides in an Anthropic capsule — foreign to a Codex
/// target, so `Capsule::fits` predicts it cannot cross.
fn anthropic_reasoning_ir() -> SessionIr {
    let mut ir = intact_ir();
    ir.events.push(ir_event(
        "e2",
        Body::Reasoning {
            text: None,
            summary: Vec::new(),
        },
        vec![Capsule {
            kind: CapsuleKind::AnthropicThinkingSignature,
            bound: CapsuleBinding {
                provider: "anthropic".to_string(),
                model: None,
            },
            sealed: "c2lnbmF0dXJl".to_string(),
        }],
    ));
    ir
}

#[test]
fn pipeline_verifies_the_structured_track_against_the_written_file() {
    // The gap this closes: the structured track used to return with nothing
    // checked at all, so the high-fidelity conversions were the unverified ones.
    let (src, dst, pipeline) = pair_with_target_slug("sid-verified", "codex");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);
    // What the target's structured reader finds on disk afterwards.
    dst.set_structured_read(intact_ir());

    let result = pipeline
        .convert("tgt", "sid-verified", options(false, None))
        .expect("a verified structured write should succeed");

    assert_eq!(dst.structured_write_calls(), 1);
    assert_eq!(
        result.fidelity,
        Fidelity::ContextComplete,
        "a clean verification leaves the writer's grade alone"
    );
    assert!(
        !result
            .warnings
            .iter()
            .any(|warning| warning.contains("could not be verified")),
        "the comparison ran, so nothing should say it was skipped: {:?}",
        result.warnings
    );
}

#[test]
fn pipeline_fails_a_structured_write_that_lost_the_conversation() {
    // A verification failure is not a write failure with a nicer message, and
    // it is not a reason to lower the grade and carry on: the file on disk is
    // not the session, so it goes back.
    let (src, dst, pipeline) = pair_with_target_slug("sid-damaged", "codex");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);
    // Read-back holds no events at all: the message is simply gone, and no
    // vendor boundary predicted that.
    dst.set_structured_read(SessionIr::new("codex", "written"));

    let error = pipeline
        .convert("tgt", "sid-damaged", options(false, None))
        .expect_err("a written file that is missing conversation must not pass");

    let Some(CasrError::VerifyFailed { detail, .. }) = error.downcast_ref::<CasrError>() else {
        panic!("expected VerifyFailed, got {error:?}");
    };
    assert!(
        detail.contains("nothing predicted the loss"),
        "the detail must distinguish this from a predicted vendor-boundary drop: {detail}"
    );
    assert!(
        detail.contains("rollback succeeded"),
        "an unverified write is rolled back: {detail}"
    );
}

#[test]
fn pipeline_says_when_a_structured_write_could_not_be_verified() {
    // A target with a structured writer and no structured reader. Nothing was
    // checked, and a check that did not run must not read as one that passed.
    let (src, dst, pipeline) = pair_with_target_slug("sid-unverifiable", "codex");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);

    let result = pipeline
        .convert("tgt", "sid-unverifiable", options(false, None))
        .expect("an unverifiable write is not a failed one");

    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("no structured reader")
                && warning.contains("could not be verified")),
        "the skipped check must be stated: {:?}",
        result.warnings
    );
    assert_eq!(result.fidelity, Fidelity::ContextComplete);
}

#[test]
fn pipeline_reports_a_writer_that_graded_better_than_its_output() {
    // The comparator derives a grade from the file independently. When the two
    // disagree the writer's grade is still the one reported — substituting the
    // comparator's would hide the disagreement as effectively as ignoring it —
    // and the disagreement itself is surfaced.
    let (src, dst, pipeline) = pair_with_target_slug("sid-overclaimed", "codex");
    src.set_structured_read(anthropic_reasoning_ir());
    dst.set_structured_write(Fidelity::ContextComplete);
    // The Anthropic capsule could not cross into a Codex rollout and did not,
    // which is correct — but a conversion that dropped reasoning is not
    // `ContextComplete`.
    dst.set_structured_read(intact_ir());

    let result = pipeline
        .convert("tgt", "sid-overclaimed", options(false, None))
        .expect("the bytes are fine, so nothing is rolled back");

    assert_eq!(
        result.fidelity,
        Fidelity::ContextComplete,
        "the reported grade is still the writer's"
    );
    assert!(
        result.warnings.iter().any(|warning| {
            warning.contains("graded this conversion ContextComplete")
                && warning.contains("only supports ContextNoReasoning")
        }),
        "the disagreement must be surfaced: {:?}",
        result.warnings
    );
}

#[test]
fn pipeline_falls_back_to_the_flat_path_when_one_end_cannot() {
    // Source reads structured, target cannot write it. Half a track is no
    // track: the conversion still has to happen, on the flat one.
    let (src, dst, pipeline) = flat_pair("sid-half");
    src.set_structured_read(intact_ir());

    let result = pipeline
        .convert("tgt", "sid-half", options(false, None))
        .expect("convert should fall back rather than fail");

    assert_eq!(dst.structured_write_calls(), 0);
    assert_eq!(dst.write_calls(), 1, "the flat writer must have run");
    assert_eq!(result.fidelity, Fidelity::ConversationOnly);

    // And the mirror image: a target that could write structured output, given
    // a source that cannot produce any.
    let (_src, dst, pipeline) = flat_pair("sid-other-half");
    dst.set_structured_write(Fidelity::ContextComplete);

    let result = pipeline
        .convert("tgt", "sid-other-half", options(false, None))
        .expect("convert should fall back rather than fail");

    assert_eq!(
        dst.structured_write_calls(),
        0,
        "with no IR to hand it, the structured writer is never asked"
    );
    assert_eq!(dst.write_calls(), 1);
    assert_eq!(result.fidelity, Fidelity::ConversationOnly);
}

#[test]
fn pipeline_grades_the_flat_projection_as_conversation_only() {
    // No structured reader at all — the common case. `CanonicalSession` keeps
    // roles and tool-call structure, so the honest grade is `ConversationOnly`
    // rather than `TranscriptOnly`, and there is nothing to add about it.
    let (_src, _dst, pipeline) = flat_pair("sid-flat");

    let result = pipeline
        .convert("tgt", "sid-flat", options(false, None))
        .expect("flat convert should succeed");

    assert_eq!(result.fidelity, Fidelity::ConversationOnly);
    assert!(result.losses.is_empty());
}

#[test]
fn pipeline_grades_a_dropped_sealed_compaction_as_history_incomplete() {
    let (src, _dst, pipeline) = flat_pair("sid-sealed");
    src.set_structured_read(sealed_compaction_ir());

    let result = pipeline
        .convert("tgt", "sid-sealed", options(false, None))
        .expect("the conversion still succeeds; it is the launch that refuses");

    assert_eq!(
        result.fidelity,
        Fidelity::HistoryIncomplete,
        "the flat projection carries no capsules, so this is a hole in the \
         conversation rather than a degraded rendering of it"
    );
    let loss = result
        .losses
        .iter()
        .find(|loss| loss.kind == LossKind::SealedContext)
        .expect("a hole in the conversation has to say how big and whose it is");
    assert_eq!(loss.events, 1, "one sealed capsule was dropped");
    assert_eq!(loss.grade, Fidelity::HistoryIncomplete);
    let detail = &loss.note;
    assert!(
        detail.contains("1 compacted-history capsule(s)")
            && detail.contains("32 bytes")
            && detail.contains("sealed to openai")
            && detail.contains("mock-target"),
        "the detail must name the count, the bytes, the vendor and the target: {detail}"
    );
}

#[test]
fn pipeline_grades_a_dry_run_as_the_conversion_it_describes() {
    // A dry run that reported a clean grade for a session the real conversion
    // would gut is worse than no grade at all.
    let (src, dst, pipeline) = flat_pair("sid-dry-sealed");
    src.set_structured_read(sealed_compaction_ir());

    let result = pipeline
        .convert("tgt", "sid-dry-sealed", options(true, None))
        .expect("dry run should succeed");

    assert!(result.written.is_none());
    assert_eq!(dst.write_calls(), 0);
    assert_eq!(result.fidelity, Fidelity::HistoryIncomplete);
    assert!(!result.losses.is_empty());
}

#[test]
fn pipeline_warns_when_target_cli_missing_but_write_succeeds() {
    let src = MockProvider::new("Source", "src", "src", vec![PathBuf::from("/tmp/src-root")]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![PathBuf::from("/tmp/dst-root")]);
    dst.set_installed(false);

    let source_path = PathBuf::from("/tmp/src-root/session-missing-target-cli.json");
    let written_path = PathBuf::from("/tmp/dst-root/out-target-cli-missing.json");
    src.set_owned_session("sid-target-cli-missing", source_path.clone());
    src.set_read_session(source_path, valid_session_with_id("sid-target-cli-missing"));
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "sid-target-cli-missing-out".to_string(),
        resume_command: "tgt --resume sid-target-cli-missing-out".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });
    dst.set_read_session(
        written_path,
        valid_session_with_id("sid-target-cli-missing"),
    );

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst)]),
        store: None,
    };

    let result = pipeline
        .convert("tgt", "sid-target-cli-missing", options(false, None))
        .expect("write should still succeed when target detect reports not installed");

    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("not detected as installed")),
        "expected missing-target warning; got {:?}",
        result.warnings
    );
}

#[test]
fn pipeline_unknown_target_alias_errors() {
    let src = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src)]),
        store: None,
    };

    let err = pipeline
        .convert("missing", "sid-z", options(false, None))
        .expect_err("unknown target alias should error");

    assert!(matches!(
        err.downcast_ref::<CasrError>(),
        Some(CasrError::UnknownProviderAlias { .. })
    ));
}

#[test]
fn pipeline_session_not_found_errors() {
    let src = MockProvider::new(
        "Mock Source",
        "mock-source",
        "src",
        vec![PathBuf::from("/tmp/src-root")],
    );
    let dst = MockProvider::new(
        "Mock Target",
        "mock-target",
        "tgt",
        vec![PathBuf::from("/tmp/tgt-root")],
    );
    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst)]),
        store: None,
    };

    let err = pipeline
        .convert("tgt", "missing-session", options(false, None))
        .expect_err("missing session should error");

    assert!(matches!(
        err.downcast_ref::<CasrError>(),
        Some(CasrError::SessionNotFound { .. })
    ));
}

#[test]
fn pipeline_ambiguous_session_errors() {
    let src_a = MockProvider::new("Source A", "src-a", "s1", vec![PathBuf::from("/tmp/src-a")]);
    let src_b = MockProvider::new("Source B", "src-b", "s2", vec![PathBuf::from("/tmp/src-b")]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![PathBuf::from("/tmp/dst")]);

    src_a.set_owned_session("same-id", "/tmp/src-a/a.json");
    src_b.set_owned_session("same-id", "/tmp/src-b/b.json");

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src_a), Box::new(src_b), Box::new(dst)]),
        store: None,
    };

    let err = pipeline
        .convert("tgt", "same-id", options(false, None))
        .expect_err("ambiguous session id should error");

    assert!(matches!(
        err.downcast_ref::<CasrError>(),
        Some(CasrError::AmbiguousSessionId { .. })
    ));
}

#[test]
fn pipeline_source_hint_alias_narrows_resolution() {
    let src_a = MockProvider::new("Source A", "src-a", "s1", vec![PathBuf::from("/tmp/src-a")]);
    let src_b = MockProvider::new("Source B", "src-b", "s2", vec![PathBuf::from("/tmp/src-b")]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![PathBuf::from("/tmp/dst")]);

    let path_a = PathBuf::from("/tmp/src-a/session.json");
    let path_b = PathBuf::from("/tmp/src-b/session.json");
    src_a.set_owned_session("same-id", path_a.clone());
    src_b.set_owned_session("same-id", path_b.clone());
    src_a.set_read_session(path_a, valid_session_with_id("from-a"));
    src_b.set_read_session(path_b, valid_session_with_id("from-b"));

    let written_path = PathBuf::from("/tmp/dst/out.json");
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-id".to_string(),
        resume_command: "tgt --resume target-id".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });
    dst.set_read_session(written_path, valid_session_with_id("from-a"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![
            Box::new(src_a),
            Box::new(src_b),
            Box::new(dst.clone()),
        ]),
        store: None,
    };

    let result = pipeline
        .convert("tgt", "same-id", options(false, Some("s1".to_string())))
        .expect("source alias hint should disambiguate");
    assert!(result.written.is_some());
    assert_eq!(
        dst.last_written()
            .expect("target should capture written session")
            .session_id,
        "from-a"
    );
}

#[test]
fn pipeline_source_hint_path_bypasses_discovery() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src_root = tmp.path().join("src-root");
    let dst_root = tmp.path().join("dst-root");
    std::fs::create_dir_all(&src_root).expect("create src root");
    std::fs::create_dir_all(&dst_root).expect("create dst root");
    let direct_path = src_root.join("direct.json");
    std::fs::write(&direct_path, "{}").expect("create direct source file");

    let src = MockProvider::new("Source", "src", "src", vec![src_root.clone()]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![dst_root.clone()]);
    src.set_read_session(direct_path.clone(), valid_session_with_id("direct-session"));

    let written_path = dst_root.join("out.json");
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-direct".to_string(),
        resume_command: "tgt --resume target-direct".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });
    dst.set_read_session(written_path, valid_session_with_id("direct-session"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst.clone())]),
        store: None,
    };

    let result = pipeline
        .convert(
            "tgt",
            "ignored-by-path-hint",
            options(false, Some(direct_path.display().to_string())),
        )
        .expect("path source hint should resolve direct path");

    assert!(result.written.is_some());
    assert_eq!(
        dst.last_written()
            .expect("target should capture written session")
            .session_id,
        "direct-session"
    );
}

#[test]
fn pipeline_write_failure_propagates() {
    let src = MockProvider::new("Source", "src", "src", vec![PathBuf::from("/tmp/src-root")]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![PathBuf::from("/tmp/dst-root")]);
    let source_path = PathBuf::from("/tmp/src-root/session-write-fail.json");
    src.set_owned_session("sid-write-fail", source_path.clone());
    src.set_read_session(source_path, valid_session_with_id("sid-write-fail"));
    dst.set_write_error("write failed in mock target");

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst.clone())]),
        store: None,
    };

    let err = pipeline
        .convert("tgt", "sid-write-fail", options(false, None))
        .expect_err("write failure should propagate");
    assert!(err.to_string().contains("write failed in mock target"));
    assert_eq!(
        dst.write_calls(),
        1,
        "write should have been attempted once"
    );
}

#[test]
fn pipeline_readback_mismatch_fails_and_removes_unverified_output() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src_root = tmp.path().join("src");
    let dst_root = tmp.path().join("dst");
    fs::create_dir_all(&src_root).expect("create src root");
    fs::create_dir_all(&dst_root).expect("create dst root");

    let src = MockProvider::new("Source", "src", "src", vec![src_root.clone()]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![dst_root.clone()]);

    let source_path = src_root.join("session-readback-mismatch.json");
    let written_path = dst_root.join("out-mismatch.json");
    src.set_owned_session("sid-readback-mismatch", source_path.clone());
    src.set_read_session(source_path, valid_session_with_id("sid-readback-mismatch"));
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-mismatch".to_string(),
        resume_command: "tgt --resume target-mismatch".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });

    fs::write(&written_path, "unverified-output").expect("seed unverified output");

    let mut short_session = valid_session_with_id("sid-readback-mismatch");
    short_session.messages.truncate(2);
    dst.set_read_session(written_path.clone(), short_session);

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst)]),
        store: None,
    };
    let err = pipeline
        .convert("tgt", "sid-readback-mismatch", options(false, None))
        .expect_err("readback mismatch should fail conversion");

    match err.downcast_ref::<CasrError>() {
        Some(CasrError::VerifyFailed { detail, .. }) => {
            assert!(
                detail.contains("message count mismatch"),
                "unexpected verify detail: {detail}"
            );
        }
        other => panic!("expected VerifyFailed, got {other:?}"),
    }
    assert!(
        !written_path.exists(),
        "unverified output should be removed on verify failure"
    );
}

#[test]
fn pipeline_readback_content_mismatch_fails_and_removes_unverified_output() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src_root = tmp.path().join("src");
    let dst_root = tmp.path().join("dst");
    fs::create_dir_all(&src_root).expect("create src root");
    fs::create_dir_all(&dst_root).expect("create dst root");

    let src = MockProvider::new("Source", "src", "src", vec![src_root.clone()]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![dst_root.clone()]);

    let source_path = src_root.join("session-readback-content-mismatch.json");
    let written_path = dst_root.join("out-content-mismatch.json");
    src.set_owned_session("sid-readback-content-mismatch", source_path.clone());
    src.set_read_session(
        source_path,
        valid_session_with_id("sid-readback-content-mismatch"),
    );
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-content-mismatch".to_string(),
        resume_command: "tgt --resume target-content-mismatch".to_string(),
        backups: Vec::new(),
        warnings: Vec::new(),
    });

    fs::write(&written_path, "unverified-output").expect("seed unverified output");

    let mut readback = valid_session_with_id("sid-readback-content-mismatch");
    readback.messages[1].content = "corrupted".to_string();
    dst.set_read_session(written_path.clone(), readback);

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst)]),
        store: None,
    };
    let err = pipeline
        .convert("tgt", "sid-readback-content-mismatch", options(false, None))
        .expect_err("readback content mismatch should fail conversion");

    match err.downcast_ref::<CasrError>() {
        Some(CasrError::VerifyFailed { detail, .. }) => {
            assert!(
                detail.contains("content mismatch"),
                "unexpected verify detail: {detail}"
            );
        }
        other => panic!("expected VerifyFailed, got {other:?}"),
    }
    assert!(
        !written_path.exists(),
        "unverified output should be removed on verify failure"
    );
}

#[test]
fn pipeline_readback_error_restores_backup_and_returns_verify_failed() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src_root = tmp.path().join("src");
    let dst_root = tmp.path().join("dst");
    fs::create_dir_all(&src_root).expect("create src root");
    fs::create_dir_all(&dst_root).expect("create dst root");

    let src = MockProvider::new("Source", "src", "src", vec![src_root.clone()]);
    let dst = MockProvider::new("Target", "dst", "tgt", vec![dst_root.clone()]);

    let source_path = src_root.join("session-readback-error.json");
    let written_path = dst_root.join("out-readback-error.json");
    let backup_path = dst_root.join("out-readback-error.json.bak");
    src.set_owned_session("sid-readback-error", source_path.clone());
    src.set_read_session(source_path, valid_session_with_id("sid-readback-error"));
    dst.set_write_success(WrittenSession {
        paths: vec![written_path.clone()],
        session_id: "target-readback-error".to_string(),
        resume_command: "tgt --resume target-readback-error".to_string(),
        backups: vec![Displaced {
            target: written_path.clone(),
            backup: backup_path.clone(),
        }],
        warnings: Vec::new(),
    });
    dst.set_read_error(written_path.clone(), "cannot parse written file");

    fs::write(&written_path, "broken-target-content").expect("seed broken target");
    fs::write(&backup_path, "restorable-original-content").expect("seed backup");

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(src), Box::new(dst)]),
        store: None,
    };
    let err = pipeline
        .convert("tgt", "sid-readback-error", options(false, None))
        .expect_err("readback error should fail conversion");

    match err.downcast_ref::<CasrError>() {
        Some(CasrError::VerifyFailed { detail, .. }) => {
            assert!(
                detail.contains("rollback succeeded"),
                "expected rollback detail, got: {detail}"
            );
        }
        other => panic!("expected VerifyFailed, got {other:?}"),
    }

    let restored = fs::read_to_string(&written_path).expect("restored target should exist");
    assert_eq!(restored, "restorable-original-content");
    assert!(
        !backup_path.exists(),
        "backup should be consumed during restore"
    );
}

#[test]
fn validate_session_errors_for_empty_and_single_sided() {
    let mut empty = valid_session_with_id("empty");
    empty.messages.clear();
    assert!(
        validate_session(&empty).has_errors(),
        "empty session should fail validation"
    );

    let mut user_only = valid_session_with_id("user-only");
    user_only
        .messages
        .retain(|m| matches!(m.role, MessageRole::User));
    assert!(
        validate_session(&user_only).has_errors(),
        "user-only session should fail validation"
    );

    let mut assistant_only = valid_session_with_id("assistant-only");
    assistant_only
        .messages
        .retain(|m| matches!(m.role, MessageRole::Assistant));
    assert!(
        validate_session(&assistant_only).has_errors(),
        "assistant-only session should fail validation"
    );
}

#[test]
fn validate_session_warnings_and_info_for_quality_issues() {
    let mut session = valid_session_with_id("quality");
    session.workspace = None;
    for msg in &mut session.messages {
        msg.timestamp = None;
    }
    session.messages = vec![
        msg(0, MessageRole::User, "u1", None),
        msg(1, MessageRole::User, "u2", None),
        msg(2, MessageRole::Assistant, "a1", None),
    ];
    session.messages[2].tool_results = vec![ToolResult {
        call_id: Some("missing-call-id".to_string()),
        content: "result".to_string(),
        is_error: false,
    }];

    let validation = validate_session(&session);

    let warnings = validation.warnings.join("\n");
    assert!(warnings.contains("no workspace"), "warnings: {warnings}");
    assert!(warnings.contains("no timestamps"), "warnings: {warnings}");
    let info_joined = validation.info.join("\n");
    assert!(
        info_joined.contains("unknown tool call id"),
        "info: {info_joined}"
    );
}

#[test]
fn validate_session_reports_tool_call_info_when_present() {
    let mut session = valid_session_with_id("tool-calls");
    session.messages[1].tool_calls.push(casr::model::ToolCall {
        id: Some("call-1".to_string()),
        name: "Read".to_string(),
        arguments: serde_json::json!({"file":"src/lib.rs"}),
    });
    let validation = validate_session(&session);
    assert!(
        validation
            .info
            .iter()
            .any(|line| line.contains("Session contains tool calls")),
        "expected tool-call info line; got {:?}",
        validation.info
    );
}

// ===========================================================================
// Real-provider pipeline tests (no mocks)
//
// These exercise the full ConversionPipeline with real providers operating on
// real fixture files in temp directories. Error-injection tests above still
// use MockProvider because real providers don't fail predictably.
// ===========================================================================

static CC_ENV: test_env::EnvLock = test_env::EnvLock;
static CODEX_ENV: test_env::EnvLock = test_env::EnvLock;
static GEMINI_ENV: test_env::EnvLock = test_env::EnvLock;

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let original = std::env::var(key).ok();
        // SAFETY: Tests must hold an `_ENV` lock (see `test_env`) while mutating
        // the process environment and while invoking code that reads it.
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(val) => unsafe { std::env::set_var(self.key, val) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn seed_cc_fixture(claude_home: &Path) -> String {
    let src = fixtures_dir().join("claude_code/cc_simple.jsonl");
    let first_line: serde_json::Value = {
        let content = std::fs::read_to_string(&src).expect("read cc_simple fixture");
        serde_json::from_str(content.lines().next().unwrap()).expect("parse first line")
    };
    let session_id = first_line["sessionId"].as_str().unwrap_or("cc-simple-001");
    let cwd = first_line["cwd"].as_str().unwrap_or("/tmp");
    let project_key = cwd.replace(|c: char| !c.is_alphanumeric(), "-");
    let target_dir = claude_home.join(format!("projects/{project_key}"));
    fs::create_dir_all(&target_dir).expect("create CC project dir");
    fs::copy(&src, target_dir.join(format!("{session_id}.jsonl"))).expect("copy CC fixture");
    session_id.to_string()
}

#[test]
fn pipeline_real_cc_to_codex_happy_path() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _codex_lock = CODEX_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));

    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]),
        store: None,
    };

    let result = pipeline
        .convert(
            "cod",
            &cc_sid,
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect("real CC→Codex pipeline should succeed");

    assert_eq!(result.source_provider, "claude-code");
    assert_eq!(result.target_provider, "codex");
    assert!(result.written.is_some(), "should have written output");
    let written = result.written.unwrap();
    assert!(
        !written.session_id.is_empty(),
        "target session_id should be set"
    );
    assert!(written.paths[0].exists(), "written Codex file should exist");
}

#[test]
fn pipeline_real_cc_to_gemini_happy_path() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _gemini_lock = GEMINI_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _gemini_env = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Gemini)]),
        store: None,
    };

    let result = pipeline
        .convert(
            "gmi",
            &cc_sid,
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect("real CC→Gemini pipeline should succeed");

    assert_eq!(result.source_provider, "claude-code");
    assert_eq!(result.target_provider, "gemini");
    assert!(result.written.is_some());
}

#[test]
fn pipeline_real_dry_run_skips_write() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _codex_lock = CODEX_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));

    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]),
        store: None,
    };

    let result = pipeline
        .convert(
            "cod",
            &cc_sid,
            ConvertOptions {
                dry_run: true,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect("real dry-run should succeed");

    assert!(result.written.is_none(), "dry-run should not write");
    // No Codex session files should exist.
    let codex_sessions = tmp.path().join("codex/sessions");
    assert!(
        !codex_sessions.exists(),
        "dry-run should not create codex session dir"
    );
}

#[test]
fn pipeline_real_same_provider_short_circuit() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));

    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode)]),
        store: None,
    };

    let result = pipeline
        .convert(
            "cc",
            &cc_sid,
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect("real same-provider should short-circuit");

    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("Source and target provider are the same")),
        "expected same-provider warning; got {:?}",
        result.warnings
    );
}

#[test]
fn pipeline_real_source_hint_narrows_resolution() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _codex_lock = CODEX_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _gemini_lock = GEMINI_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));
    let _gemini_env = EnvGuard::set("GEMINI_HOME", &tmp.path().join("gemini"));

    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![
            Box::new(ClaudeCode),
            Box::new(Codex),
            Box::new(Gemini),
        ]),
        store: None,
    };

    let result = pipeline
        .convert(
            "gmi",
            &cc_sid,
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: Some("cc".to_string()),
                ..Default::default()
            },
        )
        .expect("source hint 'cc' should resolve to ClaudeCode");

    assert_eq!(result.source_provider, "claude-code");
    assert_eq!(result.target_provider, "gemini");
    assert!(result.written.is_some());
}

#[test]
fn pipeline_real_session_not_found() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _codex_lock = CODEX_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]),
        store: None,
    };

    let err = pipeline
        .convert(
            "cod",
            "nonexistent-session-id",
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect_err("real not-found should error");

    assert!(matches!(
        err.downcast_ref::<CasrError>(),
        Some(CasrError::SessionNotFound { .. })
    ));
}

#[test]
fn pipeline_real_unknown_target_alias() {
    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode)]),
        store: None,
    };

    let err = pipeline
        .convert(
            "nonexistent-alias",
            "any-session",
            ConvertOptions {
                dry_run: false,
                force: false,
                verbose: false,
                enrich: false,
                source_hint: None,
                ..Default::default()
            },
        )
        .expect_err("unknown alias should error");

    assert!(matches!(
        err.downcast_ref::<CasrError>(),
        Some(CasrError::UnknownProviderAlias { .. })
    ));
}

// ---------------------------------------------------------------------------
// Tracing / observability tests
// ---------------------------------------------------------------------------

use tracing_subscriber::prelude::*;

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: tracing::Level,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct LogCollector {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl LogCollector {
    fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("log collector lock").clone()
    }
}

/// The capture, installed once as the binary's global subscriber.
///
/// `tracing` caches each callsite's interest globally, and the first thread to
/// reach a callsite decides it — a thread with no subscriber caches "never", and
/// every later event at that callsite is dropped however many subscribers have
/// joined since. A thread-local `set_default` is therefore in a race it cannot
/// reliably win, and warming the callsites first only helps when the scheduler
/// cooperates. Measured over fifteen runs of this binary: one failure before
/// four more converting tests were added beside this one, eleven after. The same
/// `info!` in the structured track is on every one of their paths.
///
/// A global subscriber settles the interest for every callsite once, before any
/// assertion depends on it. It also sees every test's events, which is why
/// recording is gated on [`RECORDING`]: only the thread that asked for a capture
/// gets one, and without that the negative assertions — *this event must not
/// appear* — would be answered by some other test's conversion.
static CAPTURE: std::sync::OnceLock<LogCollector> = std::sync::OnceLock::new();

thread_local! {
    static RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record this thread's tracing events until the guard is dropped.
fn capture_tracing() -> TracingCapture {
    let collector = CAPTURE.get_or_init(|| {
        let collector = LogCollector::default();
        let subscriber = tracing_subscriber::registry().with(
            collector
                .clone()
                .with_filter(tracing_subscriber::filter::LevelFilter::TRACE),
        );
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other test in this binary installs a subscriber");
        collector
    });
    collector.events.lock().expect("log collector lock").clear();
    RECORDING.with(|recording| recording.set(true));
    TracingCapture(collector)
}

struct TracingCapture(&'static LogCollector);

impl TracingCapture {
    fn snapshot(&self) -> Vec<CapturedEvent> {
        self.0.snapshot()
    }
}

impl Drop for TracingCapture {
    fn drop(&mut self) {
        RECORDING.with(|recording| recording.set(false));
    }
}

impl<S> tracing_subscriber::Layer<S> for LogCollector
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !RECORDING.with(|recording| recording.get()) {
            return;
        }
        let meta = event.metadata();
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor {
            fields: &mut fields,
        });
        self.events
            .lock()
            .expect("log collector lock")
            .push(CapturedEvent {
                level: *meta.level(),
                fields,
            });
    }
}

struct FieldVisitor<'a> {
    fields: &'a mut BTreeMap<String, String>,
}

impl<'a> tracing::field::Visit for FieldVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

fn event_has_message(event: &CapturedEvent, needle: &str) -> bool {
    event
        .fields
        .get("message")
        .is_some_and(|msg| msg.contains(needle))
}

#[test]
fn pipeline_emits_trace_events_for_detection_read_write_verify() {
    let _cc_lock = CC_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _codex_lock = CODEX_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _cc_env = EnvGuard::set("CLAUDE_HOME", &tmp.path().join("claude"));
    let _codex_env = EnvGuard::set("CODEX_HOME", &tmp.path().join("codex"));
    let cc_sid = seed_cc_fixture(&tmp.path().join("claude"));

    let pipeline = ConversionPipeline {
        registry: ProviderRegistry::new(vec![Box::new(ClaudeCode), Box::new(Codex)]),
        store: None,
    };
    let convert = || {
        pipeline
            .convert(
                "cod",
                &cc_sid,
                ConvertOptions {
                    dry_run: false,
                    force: false,
                    verbose: false,
                    enrich: false,
                    source_hint: None,
                    ..Default::default()
                },
            )
            .expect("conversion should succeed")
    };

    // See `CAPTURE`: the subscriber is global and recording is per-thread, so
    // there is no callsite-registration race left to warm around.
    let capture = capture_tracing();

    convert();

    let events = capture.snapshot();

    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::INFO && event_has_message(e, "starting conversion")),
        "missing starting conversion INFO event; got {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::TRACE && event_has_message(e, "detection")),
        "missing provider detection TRACE event; got {events:#?}"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && event_has_message(e, "found Claude Code session")),
        "missing session discovery DEBUG event; got {events:#?}"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && event_has_message(e, "Claude Code session parsed")),
        "missing source read DEBUG event; got {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::INFO
                && event_has_message(e, "atomic write complete")),
        "missing atomic write INFO event; got {events:#?}"
    );
    // Claude Code -> Codex takes the structured track, which returns before the
    // flat verifier. That verifier compares the target's `read_session` against
    // `canonical`, and a structured write legitimately preserves more than the
    // flat projection, so running it would fail the better conversion. So the
    // two pins below are a pair: the flat oracle must *not* run here, and the
    // structural one must. An earlier revision of this test pinned only the
    // absence, because there was no second oracle to pin — which made the
    // missing step visible instead of letting it read as a passing one. There
    // is one now.
    assert!(
        events.iter().any(|e| e.level == tracing::Level::INFO
            && event_has_message(e, "session written on the structured track")),
        "missing structured-track INFO event; got {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.level == tracing::Level::DEBUG
                && event_has_message(e, "Codex session parsed")),
        "the structured track must not run the flat read-back verifier: it \
         compares against `canonical`, which is not what was written"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && event_has_message(e, "structured read-back verified")),
        "the structured track must run the structural comparator: without it \
         the high-fidelity conversions are the unverified ones; got {events:#?}"
    );
}

// ---------------------------------------------------------------------------
// Structured-read capability
// ---------------------------------------------------------------------------

/// The pipeline asks [`Provider::supports_structured_read`] *instead of* calling
/// `read_session_ir`, so the two have to agree: a provider whose probe says
/// `false` must be a provider whose reader would have returned `Ok(None)`. That
/// equivalence is the whole justification for the skip, and it is mechanical
/// enough to check rather than assert in a comment.
#[test]
fn a_provider_that_denies_a_structured_reader_has_none() {
    let registry = ProviderRegistry::default_registry();

    let structured: Vec<&str> = registry
        .all_providers()
        .iter()
        .filter(|provider| provider.supports_structured_read())
        .map(|provider| provider.slug())
        .collect();
    assert_eq!(
        structured,
        ["claude-code", "codex"],
        "the structured track is these two; a provider joining it overrides the \
         probe as well as the reader"
    );

    for provider in registry.all_providers() {
        if provider.supports_structured_read() {
            continue;
        }
        // The path is deliberately one that does not exist. A provider with no
        // structured reader cannot look at it — that is what makes skipping the
        // call free of consequences — so `Ok(None)` is the only answer available.
        let answer = provider.read_session_ir(Path::new("/nonexistent/session.jsonl"));
        assert!(
            matches!(answer, Ok(None)),
            "{} answers `false` to the probe, so the pipeline never calls its \
             reader; if that reader can return anything but Ok(None) the skip \
             changes the conversion",
            provider.slug()
        );
    }
}

// ---------------------------------------------------------------------------
// The grade a decision acts on
// ---------------------------------------------------------------------------

#[test]
fn a_grade_the_comparator_disproved_is_not_what_a_refusal_acts_on() {
    // The writer says the conversion is fine. The comparator, reading the file
    // back independently, finds a sealed compaction that could not cross into an
    // Anthropic target and is therefore gone. Reporting the writer's number is
    // deliberate — substituting would hide the disagreement — but the launch
    // refusal reads `effective_fidelity`, which is the worse of the two.
    let (src, dst, pipeline) = pair_with_target_slug("sid-underclaimed", "claude-code");
    src.set_structured_read(sealed_compaction_ir());
    dst.set_structured_write(Fidelity::ConversationOnly);
    dst.set_structured_read(intact_ir());

    let result = pipeline
        .convert("tgt", "sid-underclaimed", options(false, None))
        .expect("the bytes are as predicted, so nothing is rolled back");

    assert_eq!(
        result.fidelity,
        Fidelity::ConversationOnly,
        "the reported grade is still the writer's, so the disagreement stays visible"
    );
    assert_eq!(
        result.verified_fidelity,
        Some(Fidelity::HistoryIncomplete),
        "the comparator derived its own grade and it must survive to the caller"
    );
    assert_eq!(
        result.effective_fidelity(),
        Fidelity::HistoryIncomplete,
        "a launch keys on this, and a writer that under-reports must not be able to \
         talk it into starting on a session with a hole in it"
    );
    assert!(
        result.fidelity_disagreement().is_some_and(
            |note| note.contains("ConversationOnly") && note.contains("HistoryIncomplete")
        ),
        "the disagreement has to be sayable in one sentence: {:?}",
        result.fidelity_disagreement()
    );
}

#[test]
fn agreement_and_no_check_at_all_are_different_answers() {
    // `verified_fidelity` is `Some` when a check ran and agreed, and `None` when
    // none ran. Collapsing those would make "verified" unreadable.
    let (src, dst, pipeline) = pair_with_target_slug("sid-agreed", "codex");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);
    dst.set_structured_read(intact_ir());
    let verified = pipeline
        .convert("tgt", "sid-agreed", options(false, None))
        .expect("convert");
    assert_eq!(verified.verified_fidelity, Some(Fidelity::ContextComplete));
    assert_eq!(verified.effective_fidelity(), Fidelity::ContextComplete);
    assert!(verified.fidelity_disagreement().is_none());

    let (_src, _dst, pipeline) = flat_pair("sid-unchecked");
    let flat = pipeline
        .convert("tgt", "sid-unchecked", options(false, None))
        .expect("convert");
    assert_eq!(
        flat.verified_fidelity, None,
        "the flat read-back checks text and roles, not fidelity; it has no grade to offer"
    );
    assert_eq!(flat.effective_fidelity(), flat.fidelity);
}

// ---------------------------------------------------------------------------
// The flat context budget
// ---------------------------------------------------------------------------

/// A session long enough that a small `--max-context-tokens` has to delete
/// turns out of the middle of it.
fn long_session(session_id: &str) -> CanonicalSession {
    let mut session = valid_session_with_id(session_id);
    session.messages = (0..12)
        .map(|i| {
            msg(
                i,
                if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                &"x".repeat(400),
                Some(1_700_000_000_000 + i as i64),
            )
        })
        .collect();
    session
}

#[test]
fn the_flat_budget_grades_the_turns_it_deleted() {
    // The flat twin of the structured track's budget accounting. Dropping the
    // middle of a conversation used to produce a warning and nothing else, so
    // the conversion still reported `ConversationOnly` — "every piece of the
    // conversation is still present" — and `--launch` believed it.
    let (src, dst, pipeline) = flat_pair("sid-budget");
    let source_path = PathBuf::from("/tmp/src-root/sid-budget.json");
    src.set_read_session(source_path, long_session("sid-budget"));
    dst.set_read_echo();

    let result = pipeline
        .convert(
            "tgt",
            "sid-budget",
            ConvertOptions {
                budget: ContextBudget {
                    max_context_tokens: 300,
                    ..ContextBudget::UNLIMITED
                },
                ..ConvertOptions::default()
            },
        )
        .expect("convert");

    assert!(
        result.canonical_session.messages.len() < 12,
        "the budget has to have actually dropped something for this to test anything"
    );
    let loss = result
        .losses
        .iter()
        .find(|loss| loss.kind == LossKind::Conversation)
        .expect("deleted turns are a loss of conversation");
    assert!(loss.events > 0, "the count has to be a count: {loss:?}");
    assert_eq!(loss.grade, Fidelity::HistoryIncomplete);
    assert_eq!(
        result.fidelity,
        Fidelity::HistoryIncomplete,
        "a hole the budget made is still a hole"
    );
}

#[test]
fn an_unbudgeted_conversion_is_graded_exactly_as_before() {
    // The other half of the same guarantee: folding budget losses into the grade
    // must not move a conversion that had no budget applied to it.
    let (_src, _dst, pipeline) = flat_pair("sid-unbudgeted");
    let result = pipeline
        .convert("tgt", "sid-unbudgeted", options(false, None))
        .expect("convert");
    assert_eq!(result.fidelity, Fidelity::ConversationOnly);
    assert!(result.losses.is_empty());
}

#[test]
fn pairing_repair_leaves_a_call_that_arrived_unanswered() {
    // A session that ended while a tool call was outstanding is a normal
    // session. Repairing by pairing alone deleted that call, then deleted the
    // message it lived in when nothing was left, on every conversion — including
    // ones where the budget removed nothing at all.
    let (src, dst, pipeline) = flat_pair("sid-orphan");
    let mut trailing = msg(1, MessageRole::Assistant, "", Some(1_700_000_005_000));
    trailing.tool_calls.push(casr::model::ToolCall {
        id: Some("call-1".to_string()),
        name: "bash".to_string(),
        arguments: serde_json::json!({"cmd": "ls"}),
    });
    let mut session = valid_session_with_id("sid-orphan");
    session.messages = vec![
        msg(0, MessageRole::User, "run ls", Some(1_700_000_000_000)),
        trailing,
    ];
    src.set_read_session(PathBuf::from("/tmp/src-root/sid-orphan.json"), session);
    dst.set_read_echo();

    let result = pipeline
        .convert("tgt", "sid-orphan", options(false, None))
        .expect("convert");

    let written = dst.last_written().expect("the flat writer ran");
    assert_eq!(
        written.messages.len(),
        2,
        "the trailing turn must survive: {:?}",
        written.messages
    );
    assert_eq!(
        written.messages[1].tool_calls.len(),
        1,
        "an unanswered call is the source's own shape, not damage to repair"
    );
    // Severing is the `HistoryIncomplete` half of `LossKind::ToolProtocol`:
    // something the model was shown is gone. The `ConversationOnly` half of the
    // same kind is the target writing a structured call as `[Tool: <name>]`
    // text, which this target does and which is not damage to the pairing — so
    // the assertion names the grade rather than the whole channel.
    assert!(
        result.losses.iter().all(|loss| {
            loss.kind != LossKind::ToolProtocol || loss.grade != Fidelity::HistoryIncomplete
        }),
        "nothing was severed, so nothing should be reported as severed: {:?}",
        result.losses
    );
}

#[test]
fn pairing_repair_reports_the_pairs_the_budget_broke() {
    // The converse: when the budget genuinely severs a call from its result, the
    // removal is real and has to be graded, not just warned about.
    let (src, dst, pipeline) = flat_pair("sid-severed");
    let mut session = valid_session_with_id("sid-severed");
    // The call is the expensive turn, so the budget drops it and keeps the cheap
    // result that answered it — the one shape where the repair is removing
    // something that was whole a moment ago.
    let mut caller = msg(1, MessageRole::Assistant, &"a".repeat(4000), Some(1));
    caller.tool_calls.push(casr::model::ToolCall {
        id: Some("call-1".to_string()),
        name: "bash".to_string(),
        arguments: serde_json::json!({}),
    });
    let mut answerer = msg(2, MessageRole::Tool, "ok", Some(2));
    answerer.tool_results.push(casr::model::ToolResult {
        call_id: Some("call-1".to_string()),
        content: "ok".to_string(),
        is_error: false,
    });
    session.messages = vec![
        msg(0, MessageRole::User, "pinned task", Some(0)),
        caller,
        answerer,
        msg(3, MessageRole::User, "and now this", Some(3)),
    ];
    src.set_read_session(PathBuf::from("/tmp/src-root/sid-severed.json"), session);
    dst.set_read_echo();

    let result = pipeline
        .convert(
            "tgt",
            "sid-severed",
            ConvertOptions {
                budget: ContextBudget {
                    max_context_tokens: 300,
                    ..ContextBudget::UNLIMITED
                },
                ..ConvertOptions::default()
            },
        )
        .expect("convert");

    assert!(
        result
            .losses
            .iter()
            .any(|loss| loss.kind == LossKind::ToolProtocol
                && loss.grade == Fidelity::HistoryIncomplete),
        "a severed pair removes something the model was shown: {:?}",
        result.losses
    );
}

// ---------------------------------------------------------------------------
// A source this build cannot parse
// ---------------------------------------------------------------------------

#[test]
fn a_failed_structured_read_is_not_the_same_as_no_structured_reader() {
    // Both used to arrive at `flat_fidelity` as `None`, and `None` means "this
    // provider has a plain format and the projection carries all of it". Applied
    // to a rollout the reader choked on, that is a claim nobody checked.
    let (src, _dst, pipeline) = flat_pair("sid-unparseable");
    src.set_structured_read_error("truncated rollout: unexpected end of file at line 4102");

    let result = pipeline
        .convert("tgt", "sid-unparseable", options(false, None))
        .expect("an unparseable structure is not a failed conversion");

    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("could not parse")),
        "a reader that ran and failed must be reported: {:?}",
        result.warnings
    );
    assert_eq!(
        result.fidelity,
        Fidelity::HistoryIncomplete,
        "graded at the worst it could be, because nothing here can rule it out"
    );
    let loss = result
        .losses
        .iter()
        .find(|loss| loss.grade == Fidelity::HistoryIncomplete)
        .expect("a grade this bad has to be explained");
    assert_eq!(
        (loss.events, loss.capsules, loss.bytes),
        (0, 0, 0),
        "nothing was measured, so the counts must not pretend otherwise"
    );
    assert!(
        loss.note.contains("could not be determined"),
        "the note has to separate a floor from a finding: {}",
        loss.note
    );
}

// ---------------------------------------------------------------------------
// --enrich
// ---------------------------------------------------------------------------

#[test]
fn enrichment_reaches_the_file_the_output_claims_it_reached() {
    // The structured writer is handed the source IR, which enrichment never
    // touches, so an enriched structured conversion reported "Added 2 synthetic
    // context message(s)" over a file containing none of them — and the read-back
    // verifier, comparing that same untouched IR, agreed the file was perfect.
    let (src, dst, pipeline) = pair_with_target_slug("sid-enriched", "claude-code");
    src.set_structured_read(intact_ir());
    dst.set_structured_write(Fidelity::ContextComplete);
    dst.set_structured_read(intact_ir());
    dst.set_read_echo();

    let result = pipeline
        .convert(
            "tgt",
            "sid-enriched",
            ConvertOptions {
                enrich: true,
                ..ConvertOptions::default()
            },
        )
        .expect("convert");

    assert_eq!(
        dst.structured_write_calls(),
        0,
        "the track that cannot carry enrichment must not be the one that runs"
    );
    let written = dst.last_written().expect("the flat writer ran");
    assert!(
        written
            .messages
            .iter()
            .any(|m| m.content.contains("[casr synthetic context]")),
        "the file must hold what the run said was added to it"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("--enrich adds messages")),
        "the track change is a fidelity consequence and has to be stated: {:?}",
        result.warnings
    );
}

// ---------------------------------------------------------------------------
// Rolling a write back
// ---------------------------------------------------------------------------

#[test]
fn rollback_restores_each_backup_onto_the_file_it_came_from() {
    // Cline's shape, which is the one the old rollback got wrong: three session
    // files, plus a backup of a shared index that is not one of them. Pairing the
    // backup with `paths[0]` moved the task index on top of the API history.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let api = tmp.path().join("api_conversation_history.json");
    let ui = tmp.path().join("ui_messages.json");
    let index = tmp.path().join("taskHistory.json");
    let index_backup = tmp.path().join("taskHistory.json.bak");
    fs::write(&api, "NEW API HISTORY").expect("seed api");
    fs::write(&ui, "NEW UI MESSAGES").expect("seed ui");
    fs::write(&index, "MODIFIED INDEX").expect("seed index");
    fs::write(&index_backup, "ORIGINAL INDEX").expect("seed index backup");

    let (src, dst, pipeline) = flat_pair("sid-rollback");
    let _ = src;
    dst.set_write_success(WrittenSession {
        paths: vec![api.clone(), ui.clone()],
        session_id: "task-1".to_string(),
        resume_command: "code .".to_string(),
        backups: vec![Displaced {
            target: index.clone(),
            backup: index_backup.clone(),
        }],
        warnings: Vec::new(),
    });
    dst.set_read_error(api.clone(), "cannot parse written file");

    pipeline
        .convert("tgt", "sid-rollback", options(false, None))
        .expect_err("an unreadable write must fail verification");

    assert_eq!(
        fs::read_to_string(&index).expect("the index must still exist"),
        "ORIGINAL INDEX",
        "the index's own backup goes back to the index"
    );
    assert!(!api.exists(), "an unverified output is removed");
    assert!(!ui.exists(), "every unverified output is removed");
    assert!(
        !index_backup.exists(),
        "the backup is consumed by the restore"
    );
}

// ---------------------------------------------------------------------------
// Atomic writes
// ---------------------------------------------------------------------------

#[test]
fn a_forced_write_never_leaves_the_session_path_empty() {
    // The session file is what the agent reads. A forced conversion that renames
    // the target away before writing its replacement has an interval in which the
    // session does not exist, and a crash in that interval loses it outright.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let target = tmp.path().join("session.jsonl");
    fs::write(&target, "ORIGINAL").expect("seed target");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let vanished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (target, stop, vanished) = (
            target.clone(),
            std::sync::Arc::clone(&stop),
            std::sync::Arc::clone(&vanished),
        );
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if !target.exists() {
                    vanished.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        })
    };

    // Large enough that the write takes long enough to be observed.
    let content = vec![b'x'; 32 * 1024 * 1024];
    casr::pipeline::atomic_write(&target, &content, true, "test").expect("forced write");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher.join().expect("watcher");

    assert!(
        !vanished.load(std::sync::atomic::Ordering::Relaxed),
        "the session path was empty part way through a forced write"
    );
    assert_eq!(
        content.len(),
        fs::metadata(&target).expect("target").len() as usize
    );
}

#[test]
fn concurrent_forced_writes_cannot_overwrite_each_others_backups() {
    // Backup names used to be chosen by an existence check and used a moment
    // later, so two forced conversions aimed at one target could both pick
    // `session.jsonl.bak` and the second would rename the first's *output* over
    // the original. The reservation is now the same operation as the check.
    for round in 0..25 {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let target = tmp.path().join("session.jsonl");
        fs::write(&target, "ORIGINAL").expect("seed target");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let target = target.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let payload = vec![b'0' + i as u8; 256 * 1024];
                    let _ = casr::pipeline::atomic_write(&target, &payload, true, "test");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread");
        }

        let survived = fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .any(|entry| fs::read_to_string(entry.path()).is_ok_and(|c| c == "ORIGINAL"));
        assert!(
            survived,
            "round {round}: the original session exists nowhere any more"
        );
    }
}
