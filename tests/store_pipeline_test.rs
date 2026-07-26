//! What the session store does to the pipeline, and what `--no-store` must not.
//!
//! Two halves, and the second is the reason the first is allowed to exist.
//!
//! - **The payoff.** A second conversion hop used to ask the session the user
//!   named rather than the best source for its target, so `codex → claude →
//!   codex` came back with none of the original reasoning capsules while the
//!   bytes that replay perfectly never left the Codex session directory. These
//!   tests run that chain through `ConversionPipeline` and count what arrives.
//! - **The escape hatch.** `--no-store` is `store: None`, and it has to mean
//!   *exactly* what the pipeline did before the store existed: read what I named,
//!   write where I said, record nothing. Pinned three ways below — no store
//!   directory appears, the result carries no selection, and the bytes written
//!   are identical to a store-backed run's.
//!
//! Every store here lives in a fresh `tempfile` root and every provider session
//! root is redirected into one. Nothing writes to a real session directory, and
//! the checked-in fixtures are only ever read.

mod test_env;

use std::path::{Path, PathBuf};

use casr::discovery::ProviderRegistry;
use casr::ir::Fidelity;
use casr::pipeline::{ConversionPipeline, ConversionResult, ConvertOptions};
use casr::store::{OriginPolicy, SessionKey, Store};

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

/// Both structured providers' session roots pointed into `tmp`.
///
/// Returned rather than dropped on the spot: the guards restore the environment
/// when they go out of scope, so they have to outlive the conversion.
fn redirect(tmp: &Path) -> Vec<EnvGuard> {
    vec![
        EnvGuard::set("CLAUDE_HOME", &tmp.join("claude")),
        EnvGuard::set("CODEX_HOME", &tmp.join("codex")),
    ]
}

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}

/// The Codex fixture that carries sealed material, which is the only kind of
/// session the second hop can lose anything interesting on.
const CODEX_WITH_CAPSULES: &str = "real_world/codex_real_world_sanitized.jsonl";

fn pipeline(store: Option<Store>) -> ConversionPipeline {
    ConversionPipeline {
        registry: ProviderRegistry::default_registry(),
        store,
    }
}

/// Convert `path` into `target_alias`, naming the file directly.
///
/// The source hint is the path so that no discovery is involved: the point of
/// these tests is what the *store* chooses, and a discovery miss would look like
/// a selection failure.
///
/// `ConvertOptions::default()` is an unlimited budget, for the reason
/// `conformance::second_hop` documents: this measures the conversion chain and
/// not the budget policy, and a budget that legitimately trims the oldest turns
/// would take capsules with it.
fn convert(
    pipeline: &ConversionPipeline,
    target_alias: &str,
    path: &Path,
) -> anyhow::Result<ConversionResult> {
    let named = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    pipeline.convert(
        target_alias,
        &named,
        ConvertOptions {
            source_hint: Some(path.display().to_string()),
            ..ConvertOptions::default()
        },
    )
}

/// Capsules on the events the model would be shown, read back off disk.
fn capsules_in(provider_slug: &str, path: &Path) -> usize {
    let registry = ProviderRegistry::default_registry();
    let provider = registry
        .find_by_slug(provider_slug)
        .unwrap_or_else(|| panic!("no provider '{provider_slug}'"));
    let ir = provider
        .read_session_ir(path)
        .unwrap_or_else(|error| {
            panic!(
                "{} could not read {}: {error}",
                provider_slug,
                path.display()
            )
        })
        .unwrap_or_else(|| panic!("{provider_slug} has no structured reader"));
    ir.model_visible()
        .iter()
        .map(|event| event.capsules.len())
        .sum()
}

/// The session a result actually delivers.
///
/// The file that was written — or, when the chosen source was already in the
/// target's format and so needed no conversion at all, the file the resume
/// command points back at. Both are "the session the user ends up in".
fn delivered(result: &ConversionResult) -> PathBuf {
    result
        .written
        .as_ref()
        .and_then(|written| written.paths.first())
        .cloned()
        .or_else(|| {
            result
                .source
                .as_ref()
                .and_then(|selection| selection.chosen())
                .map(|chosen| chosen.path.clone())
        })
        .expect("a successful conversion delivers a session")
}

// ---------------------------------------------------------------------------
// The payoff
// ---------------------------------------------------------------------------

/// `codex → claude → codex`: the store reads the origin the first hop could not
/// carry, and says what the named session would have cost.
#[test]
fn the_second_hop_reads_the_origin_the_first_hop_could_not_carry() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());

    let origin = fixture(CODEX_WITH_CAPSULES);
    let origin_capsules = capsules_in("codex", &origin);
    assert!(
        origin_capsules > 0,
        "the fixture must carry sealed material or there is nothing for the second hop to lose"
    );

    let stored = pipeline(Some(
        Store::open_at(tmp.path().join("store")).expect("open store"),
    ));

    // Hop one, out of Codex. Everything sealed to openai is correctly refused.
    let first = convert(&stored, "cc", &origin).expect("codex -> claude");
    let intermediate = delivered(&first);
    assert_eq!(
        capsules_in("claude-code", &intermediate),
        0,
        "no openai capsule may cross into an anthropic session"
    );

    // Hop two, back into Codex, naming the Claude session — the file a user who
    // had been working in Claude Code would name.
    let second = convert(&stored, "cod", &intermediate).expect("claude -> codex");
    let selection = second
        .source
        .as_ref()
        .expect("a store-backed conversion reports its source");
    assert!(
        selection.overrode(),
        "the store had a strictly better source and read the named session anyway: {}",
        selection.line()
    );
    let chosen = selection.chosen().expect("a chosen source");
    assert_eq!(
        chosen.key,
        SessionKey::new("codex", &first.canonical_session.session_id)
    );
    assert_eq!(
        capsules_in("codex", &delivered(&second)),
        origin_capsules,
        "the second hop must arrive with every capsule the origin still holds"
    );

    // The counterfactual is part of the result, not a log line.
    let line = selection.line();
    assert!(
        line.starts_with("source: codex ") && line.contains("(origin;"),
        "got {line}"
    );
    assert!(
        line.contains("you named claude-code")
            && line.contains(&format!("cost {origin_capsules} capsules")),
        "got {line}"
    );
}

// ---------------------------------------------------------------------------
// The four rows of the growth table
// ---------------------------------------------------------------------------

/// A conversation as a fresh store sees it, with both files writable.
///
/// The Codex origin is a *copy* of the fixture in `tmp`, not the fixture itself,
/// so a row that needs the origin to have advanced can append to it. The repo's
/// fixtures are only ever read.
struct Chain {
    tmp: tempfile::TempDir,
    _redirect: Vec<EnvGuard>,
    stored: ConversionPipeline,
    origin: PathBuf,
    intermediate: PathBuf,
    origin_capsules: usize,
}

const MARKER: &str = "TWO-HOURS-OF-WORK-IN-CLAUDE";

fn chain() -> Chain {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let redirect = redirect(tmp.path());

    let origin = tmp.path().join("rollout.jsonl");
    std::fs::copy(fixture(CODEX_WITH_CAPSULES), &origin).expect("copy the fixture");
    let origin_capsules = capsules_in("codex", &origin);
    assert!(
        origin_capsules > 0,
        "the fixture must carry sealed material"
    );

    let stored = pipeline(Some(
        Store::open_at(tmp.path().join("store")).expect("open store"),
    ));
    let first = convert(&stored, "cc", &origin).expect("codex -> claude");
    let intermediate = delivered(&first);
    assert_eq!(
        capsules_in("claude-code", &intermediate),
        0,
        "no openai capsule may cross into an anthropic session"
    );

    Chain {
        tmp,
        _redirect: redirect,
        stored,
        origin,
        intermediate,
        origin_capsules,
    }
}

/// Two hours of work in the intermediate, as the append-only log records it.
fn work_in(path: &Path, provider: &str, turns: usize) -> u64 {
    casr::conformance::append_turns(path, provider, MARKER, turns)
        .unwrap_or_else(|error| panic!("append to {}: {error}", path.display()))
}

fn holds_the_work(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .contains(MARKER)
}

/// **Row 3 of the table.** The user worked in the derivative for two hours, so
/// the derivative must win — and the capsule cost of that has to be stated.
///
/// This is the defect the corrected design exists for. Ranking capsules above
/// growth returns the Codex origin here, writes nothing, and hands back the file
/// the user already had: the fifty appended turns are simply gone, and the output
/// line reads like a win.
#[test]
fn the_work_the_user_did_in_the_derivative_outranks_the_origin_s_capsules() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let chain = chain();

    let added = work_in(&chain.intermediate, "claude-code", 25);
    assert!(added > 0, "the append added no bytes");
    assert!(holds_the_work(&chain.intermediate));

    let second = convert(&chain.stored, "cod", &chain.intermediate).expect("claude -> codex");
    let delivered = delivered(&second);
    let selection = second.source.as_ref().expect("a store-backed selection");

    assert!(
        holds_the_work(&delivered),
        "the store delivered {} for a Codex target and the two hours of work appended to \
         {} are not in it. It chose {}, which is older-but-richer: {}",
        delivered.display(),
        chain.intermediate.display(),
        selection
            .chosen()
            .map(|chosen| chosen.key.to_string())
            .unwrap_or_default(),
        selection.line()
    );
    assert_eq!(
        selection.chosen().expect("a chosen source").key,
        SessionKey::new("claude-code", chain.intermediate_id()),
        "the incarnation that holds content nothing else has must be the source"
    );
    // What that costs is stated, in the direction it is actually paid.
    let line = selection.line();
    assert!(
        line.contains("gives up") && line.contains(&format!("{} capsules", chain.origin_capsules)),
        "the capsule cost of keeping the newer work must be reported: {line}"
    );
}

/// **Row 1 of the table.** Nothing appended anywhere: the origin still wins,
/// because at the moment of derivation the derivative is a lossy projection of it
/// and so holds nothing it lacks.
#[test]
fn with_nothing_appended_the_origin_still_wins() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let chain = chain();

    let second = convert(&chain.stored, "cod", &chain.intermediate).expect("claude -> codex");
    let selection = second.source.as_ref().expect("a store-backed selection");
    assert!(selection.overrode(), "{}", selection.line());
    assert_eq!(
        capsules_in("codex", &delivered(&second)),
        chain.origin_capsules,
        "with nothing appended the origin loses nothing and carries every capsule"
    );
}

/// **Row 2 of the table.** Only the origin advanced, so it holds both the newer
/// turns and the capsules.
#[test]
fn when_only_the_origin_advanced_it_holds_everything() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let chain = chain();

    work_in(&chain.origin, "codex", 5);

    let second = convert(&chain.stored, "cod", &chain.intermediate).expect("claude -> codex");
    let selection = second.source.as_ref().expect("a store-backed selection");
    assert_eq!(
        selection.chosen().expect("a chosen source").key,
        SessionKey::new("codex", chain.origin_id()),
        "{}",
        selection.line()
    );
    assert!(
        holds_the_work(&delivered(&second)),
        "the origin's own newer turns must arrive: {}",
        selection.line()
    );
}

/// **Row 4 of the table.** Both advanced. Nothing here can merge two
/// incarnations, so the session the user named is read — which is what they would
/// have got with `--no-store` — and both costs are stated.
#[test]
fn genuine_divergence_reads_the_named_session_and_says_what_each_side_costs() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let chain = chain();

    work_in(&chain.origin, "codex", 5);
    work_in(&chain.intermediate, "claude-code", 25);

    let second = convert(&chain.stored, "cod", &chain.intermediate).expect("claude -> codex");
    let selection = second.source.as_ref().expect("a store-backed selection");

    assert!(
        !selection.overrode(),
        "a record this design cannot merge must fall back to the session the user named, which \
         is what `--no-store` would have delivered: {}",
        selection.line()
    );
    assert!(
        holds_the_work(&delivered(&second)),
        "the named session's own work must arrive: {}",
        selection.line()
    );
    let line = selection.line();
    for expected in ["cannot merge", "gives up", "is missing"] {
        assert!(
            line.contains(expected),
            "a divergence has to state the cost of both sides; no {expected:?} in: {line}"
        );
    }
    assert!(
        second
            .warnings
            .iter()
            .any(|warning| warning.contains("cannot merge")),
        "and it has to be loud, not only in the source line: {:?}",
        second.warnings
    );
}

/// A record written before derived snapshots existed cannot be judged, and an
/// unknown must not be read as "did not advance" — that is the defect again.
///
/// It resolves the same way divergence does: read what the user named, which is
/// never worse than `--no-store`, and say why.
#[test]
fn a_derived_incarnation_with_no_snapshot_falls_back_to_the_named_session() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let chain = chain();

    // Strip the snapshot the way a record written by the previous build has it:
    // absent. Records are never migrated, so this shape is permanent.
    let store_root = chain.tmp.path().join("store");
    let record = std::fs::read_dir(store_root.join("records"))
        .expect("records")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("record.json"))
        .find(|path| path.is_file())
        .expect("one record");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).expect("read")).expect("parse");
    let mut stripped = 0;
    for incarnation in json["incarnations"]
        .as_array_mut()
        .expect("incarnations")
        .iter_mut()
    {
        let role = incarnation["role"].as_object_mut().expect("a role object");
        if role.get("role").and_then(|tag| tag.as_str()) == Some("derived")
            && role.remove("snapshot").is_some()
        {
            stripped += 1;
        }
    }
    assert_eq!(stripped, 1, "one derived snapshot to strip");
    std::fs::write(&record, serde_json::to_vec_pretty(&json).expect("encode")).expect("write");

    let second = convert(&chain.stored, "cod", &chain.intermediate).expect("claude -> codex");
    let selection = second.source.as_ref().expect("a store-backed selection");
    assert!(
        !selection.overrode(),
        "an unknown must not hand the choice to the older-but-richer incarnation: {}",
        selection.line()
    );
}

impl Chain {
    /// The Claude session id the first hop minted, which is how the store knows
    /// the intermediate.
    fn intermediate_id(&self) -> String {
        self.intermediate
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// The Codex session id, read out of the copied rollout's own first line.
    fn origin_id(&self) -> String {
        let text = std::fs::read_to_string(&self.origin).expect("read the origin");
        let first = text.lines().next().expect("a first line");
        serde_json::from_str::<serde_json::Value>(first)
            .ok()
            .and_then(|line| {
                line.pointer("/payload/id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string)
            })
            .expect("the rollout names its own session id")
    }
}

/// The same chain with no store loses all of it — which is the defect, measured.
#[test]
fn the_same_second_hop_without_a_store_arrives_empty() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());

    let origin = fixture(CODEX_WITH_CAPSULES);
    assert!(capsules_in("codex", &origin) > 0);

    let bare = pipeline(None);
    let first = convert(&bare, "cc", &origin).expect("codex -> claude");
    let second = convert(&bare, "cod", &delivered(&first)).expect("claude -> codex");

    assert!(second.source.is_none(), "no store, so no selection");
    assert_eq!(
        capsules_in("codex", &delivered(&second)),
        0,
        "without the store the second hop can only carry what the first one left"
    );
}

// ---------------------------------------------------------------------------
// `--no-store` is exactly what the pipeline did before the store existed
// ---------------------------------------------------------------------------

/// No store means nothing was consulted and nothing was created.
///
/// The filesystem half matters as much as the result half: `Store::open` creates
/// its root on first use, so "the store was not consulted" and "no store appeared
/// on disk" are two different claims and both are the flag's promise.
#[test]
fn no_store_consults_nothing_and_creates_nothing() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());
    let would_be = tmp.path().join("store");
    let _store_env = EnvGuard::set("AGSX_STORE", &would_be);

    let result = convert(&pipeline(None), "cc", &fixture(CODEX_WITH_CAPSULES))
        .expect("codex -> claude with no store");

    assert!(result.source.is_none());
    assert!(
        !would_be.exists(),
        "a `--no-store` run created {}",
        would_be.display()
    );
    assert!(
        result
            .warnings
            .iter()
            .all(|warning| !warning.contains("session store")),
        "a run with no store must not mention one: {:?}",
        result.warnings
    );
}

/// `codex → codex` stays byte-identical through `--no-store`.
///
/// Byte-identical in the strongest available sense, which is why this is the
/// regression test the design names: the pipeline writes nothing at all, points
/// the resume command back at the session's own id, and the file it names is the
/// same bytes afterwards as before.
#[test]
fn codex_into_itself_stays_byte_identical_through_no_store() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());

    let origin = fixture(CODEX_WITH_CAPSULES);
    let before = std::fs::read(&origin).expect("read fixture");

    let result = convert(&pipeline(None), "cod", &origin).expect("codex -> codex");
    let written = result.written.as_ref().expect("resume metadata");

    assert_eq!(result.fidelity, Fidelity::ByteIdentical);
    assert!(result.losses.is_empty());
    assert!(
        written.paths.is_empty(),
        "a same-provider conversion writes nothing: {:?}",
        written.paths
    );
    assert_eq!(written.session_id, result.canonical_session.session_id);
    assert_eq!(
        std::fs::read(&origin).expect("re-read fixture"),
        before,
        "the source bytes were modified by a conversion that claims to be byte-identical"
    );
}

/// `--no-store` writes the same bytes as a store that has nothing better.
///
/// Everything a writer mints fresh on every call — the target session id, the
/// write timestamp — is normalised away by [`normalise`], and the test proves its
/// own mask before it uses it: two `--no-store` runs must normalise equal, which
/// they cannot do if the mask is letting a genuinely variable field through.
/// Only then is the store-backed run compared against them.
#[test]
fn no_store_writes_the_same_bytes_as_a_store_with_nothing_better() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());
    let origin = fixture(CODEX_WITH_CAPSULES);

    let bare = pipeline(None);
    let first = convert(&bare, "cc", &origin).expect("run one");
    let second = convert(&bare, "cc", &origin).expect("run two");

    // A store that has never seen this conversation ingests it as the origin,
    // ranks a single incarnation, and chooses exactly what was named.
    let stored = pipeline(Some(
        Store::open_at(tmp.path().join("store")).expect("open store"),
    ));
    let third = convert(&stored, "cc", &origin).expect("run three");
    assert!(
        third
            .source
            .as_ref()
            .is_some_and(|selection| !selection.overrode()),
        "a store with one incarnation has nothing better to offer"
    );

    let baseline = normalise(&first);
    assert_eq!(
        baseline,
        normalise(&second),
        "two identical `--no-store` runs did not normalise equal, so the mask below cannot be \
         trusted to prove anything about the store"
    );
    assert_eq!(
        baseline,
        normalise(&third),
        "a store-backed conversion wrote different bytes than `--no-store` did"
    );
}

/// A written session's bytes with the minted id and the write time masked out.
fn normalise(result: &ConversionResult) -> Vec<String> {
    let path = delivered(result);
    let text = std::fs::read_to_string(&path).expect("read written session");
    text.lines()
        .map(
            |line| match serde_json::from_str::<serde_json::Value>(line) {
                Ok(mut value) => {
                    mask(&mut value);
                    value.to_string()
                }
                // Not JSON, so nothing in it was minted by a serializer either.
                Err(_) => line.to_string(),
            },
        )
        .collect()
}

/// Blank every string that is a UUID or an RFC-3339 timestamp, recursively.
///
/// By *shape* rather than by key name, because a mask that has to be told where
/// to look grows a hole every time a writer adds a field. Neither shape can
/// carry conversation content: a uuid is an identifier this run minted and a
/// timestamp is when it ran.
fn mask(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if is_uuid(text) {
                *text = "<uuid>".to_string();
            } else if is_timestamp(text) {
                *text = "<timestamp>".to_string();
            } else {
                *text = mask_simple_uuids(text);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(mask),
        serde_json::Value::Object(fields) => {
            fields.iter_mut().for_each(|(_, field)| mask(field));
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && [8, 13, 18, 23]
            .iter()
            .all(|at| text.as_bytes()[*at] == b'-')
        && text
            .bytes()
            .enumerate()
            .all(|(at, byte)| byte.is_ascii_hexdigit() || [8, 13, 18, 23].contains(&at))
}

/// Replace every embedded hyphen-free uuid, such as Claude Code's
/// `msg_casr_<32 hex>` message ids.
///
/// A run of *exactly* 32 hex digits, so that a 40-character git object name and a
/// 64-character digest — both of which are content — are left alone.
fn mask_simple_uuids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < bytes.len() {
        let run = bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        if run == 32 {
            out.push_str("<simple-uuid>");
        } else {
            out.push_str(&text[at..at + run.max(1)]);
        }
        at += run.max(1);
    }
    out
}

fn is_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 19
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
}

// ---------------------------------------------------------------------------
// A broken store is a warning, never a failed conversion
// ---------------------------------------------------------------------------

/// A store this build may not write to does not take the conversion with it.
///
/// The conservative half of turning the store on by default: `store.json` from a
/// newer build stays readable and refuses every write, and a conversion that
/// worked before the store existed has to keep working. The failure is reported —
/// a silently absent store looks exactly like one with nothing to say.
#[test]
fn a_store_the_pipeline_may_not_write_degrades_to_a_warning() {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _redirect = redirect(tmp.path());

    let root = tmp.path().join("store");
    std::fs::create_dir_all(&root).expect("create store root");
    std::fs::write(
        root.join("store.json"),
        serde_json::json!({ "store_version": u32::MAX }).to_string(),
    )
    .expect("write a store from the future");

    let stored = pipeline(Some(Store::open_at(&root).expect("open a future store")));
    let result = convert(&stored, "cc", &fixture(CODEX_WITH_CAPSULES))
        .expect("a store from the future must not fail a conversion");

    assert!(result.written.is_some(), "the session was still written");
    assert!(
        result.source.is_none(),
        "nothing could be ingested, so there is no selection to report"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("session store")),
        "the store's refusal must be reported, not swallowed: {:?}",
        result.warnings
    );
}

// ---------------------------------------------------------------------------
// A record id is an identifier a provider can be pointed at
// ---------------------------------------------------------------------------

/// `resume <record-id>` resolves to a session the target can actually resume.
///
/// The target's own incarnation first, because it needs no conversion; the origin
/// when the target has never seen this conversation.
#[test]
fn a_record_id_resolves_to_the_session_a_provider_can_resume() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = Store::open_at(tmp.path().join("store")).expect("open store");
    let session = tmp.path().join("origin.jsonl");
    std::fs::write(&session, "{}\n").expect("write a session file");

    let codex = SessionKey::new("codex", "codex-1");
    let record = store
        .ingest_origin(&codex, &session, OriginPolicy::Reference)
        .expect("ingest");

    // Only the origin so far: every target resolves to it.
    for target in ["codex", "claude-code", "gemini"] {
        assert_eq!(
            casr::launch::session_named_by_record(&record, target),
            Some(&codex),
            "a record with one incarnation names it for every target"
        );
    }

    let claude = SessionKey::new("claude-code", "cc-1");
    let record = store
        .record_conversion(
            &record.id,
            casr::store::DerivedWrite {
                key: claude.clone(),
                path: tmp.path().join("derived.jsonl"),
                from: codex.clone(),
                fidelity: Fidelity::HistoryIncomplete,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");

    assert_eq!(
        casr::launch::session_named_by_record(&record, "claude-code"),
        Some(&claude),
        "the target's own incarnation needs no conversion, so it comes first"
    );
    assert_eq!(
        casr::launch::session_named_by_record(&record, "codex"),
        Some(&codex)
    );
    assert_eq!(
        casr::launch::session_named_by_record(&record, "gemini"),
        Some(&codex),
        "a target with no incarnation falls back to the conversation's origin"
    );
}

// ---------------------------------------------------------------------------
// No test may write to the developer's own store
// ---------------------------------------------------------------------------

/// Every test file that runs `casr resume` in a child process must redirect the
/// session store.
///
/// The store is on by default, so `casr resume …` writes to
/// `dirs::data_dir()/agsx` — a real store belonging to whoever is running the
/// suite — unless the test says otherwise. That is not hypothetical: turning the
/// store on made `grok_test::cli_convert_into_grok_is_refused` file a fixture as
/// the origin of a new conversation in the author's own store, pointing at a path
/// inside a `tempfile` directory that was deleted a moment later. The four CLI
/// harnesses already redirect `XDG_DATA_HOME`; that one test builds its command
/// by hand and so bypassed them.
///
/// Checked against the source rather than at run time, because the leak is
/// invisible from inside the test that causes it: the child process succeeds or
/// fails on its own terms and says nothing about which store it used. File-level
/// granularity on purpose — per-invocation would mean parsing Rust, and one
/// mention of a redirect per file is enough to make the omission deliberate
/// rather than forgotten.
#[test]
fn every_cli_test_that_resumes_redirects_the_store() {
    let tests = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    let mut checked = 0usize;

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&tests)
        .expect("read the tests directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    entries.sort();

    for path in entries {
        let source = std::fs::read_to_string(&path).expect("read a test file");
        let spawns = source.contains("CARGO_BIN_EXE_casr") || source.contains("cargo_bin");
        if !spawns || !source.contains("\"resume\"") {
            continue;
        }
        checked += 1;
        let redirects = source.contains("AGSX_STORE")
            || source.contains("XDG_DATA_HOME")
            || source.contains("--no-store");
        if !redirects {
            offenders.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    assert!(
        checked > 0,
        "this guard found no CLI test that runs `resume`, so it is guarding nothing"
    );
    assert!(
        offenders.is_empty(),
        "{offenders:?} run `casr resume` in a child process without redirecting the session \
         store, so they write into the real one at `dirs::data_dir()/agsx`. Set \
         `AGSX_STORE` or `XDG_DATA_HOME` to a temp directory on the command, or pass \
         `--no-store`."
    );
}
