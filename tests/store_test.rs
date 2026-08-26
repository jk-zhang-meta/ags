//! Integration tests for the session store's public contract.
//!
//! These test the store the way `pipeline.rs` and `launch.rs` will use it —
//! through `ags::store` only — so that the wiring has something to lean on.
//! The three that matter most are the ones that turn a comment into a
//! guarantee: a stale cached IR is *deleted*, a deleted index is *rebuilt*, and
//! a store from the future is readable but not writable.
//!
//! The corpus under `~/.codex/sessions` and `~/.claude/projects` is read-only
//! here. Every store these tests create lives in a fresh `tempfile` root.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ags::ir::{
    Block, Body, Capsule, CapsuleBinding, CapsuleKind, Event, Fidelity, IR_VERSION, Loss, LossKind,
    Role as IrRole, SessionIr, SourceRef, Visibility,
};
use ags::providers::Provider;
use ags::store::{
    Availability, DerivedWrite, OriginPolicy, OriginSnapshot, OriginState, Role, SessionKey, Store,
    StoreError,
};

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fresh() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = Store::open_at(tmp.path().join("store")).expect("open store");
    (tmp, store)
}

fn write_session(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write session file");
    path
}

/// One registry for the whole test binary, borrowed exactly the way the
/// pipeline borrows its own: `best_source_for` takes it rather than building
/// one, so that the ranking counts capsules through the same providers the
/// caller will read the chosen candidate with.
fn registry() -> &'static ags::discovery::ProviderRegistry {
    static REGISTRY: std::sync::OnceLock<ags::discovery::ProviderRegistry> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(ags::discovery::ProviderRegistry::default_registry)
}

fn provider(slug: &str) -> &'static dyn Provider {
    registry()
        .find_by_slug(slug)
        .unwrap_or_else(|| panic!("no provider '{slug}'"))
}

/// A synthetic IR carrying `count` capsules of one vendor's sealed format.
fn ir_with_capsules(agent: &str, kind: CapsuleKind, count: usize) -> SessionIr {
    let mut ir = SessionIr::new(agent, "synthetic");
    for i in 0..count {
        ir.events.push(Event {
            id: format!("e{i}"),
            parent: None,
            branch: ags::ir::Branch::Main,
            turn: None,
            ts: None,
            visibility: Visibility::Model,
            body: Body::Message {
                role: IrRole::Assistant,
                blocks: vec![Block::Text {
                    text: "hello".to_string(),
                }],
            },
            capsules: vec![Capsule {
                kind,
                bound: CapsuleBinding {
                    provider: kind.vendor().to_string(),
                    model: None,
                },
                sealed: "SEALEDSEALED".to_string(),
            }],
            source: SourceRef {
                line: (i + 1) as u64,
                sha256: String::new(),
            },
        });
    }
    ir
}

/// Every `(provider, session_id) -> record` row in the index, read straight out
/// of SQLite so that "the index was rebuilt" is a claim about the index and not
/// about the store's fallbacks.
fn index_rows(root: &Path) -> Vec<(String, String, String)> {
    let conn = rusqlite::Connection::open(root.join("index.sqlite")).expect("open index");
    let mut stmt = conn
        .prepare("SELECT provider, provider_session_id, record_id FROM sessions ORDER BY 1, 2")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// The largest `.jsonl` under `root`, or `None` when the corpus is absent.
/// The largest corpus session that is not being written to right now.
///
/// The quiescence filter is not fussiness. On a machine that uses Codex the
/// largest rollout is routinely the *live* one — the session doing the work
/// that is running this test — and it grows between the scan and every
/// measurement afterwards. A caller that snapshots it then asks whether it
/// changed gets `rehashed: true`, which is the correct answer about a file
/// that did change and a useless one about the property being measured. One
/// minute is far longer than the gap between an agent's writes and far shorter
/// than the age of anything else in a corpus.
fn largest_under(root: &Path) -> Option<(PathBuf, u64)> {
    let quiescent_for = std::time::Duration::from_secs(60);
    let now = SystemTime::now();
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().map(|m| (e.path().to_path_buf(), m)))
        .filter(|(_, meta)| {
            meta.modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= quiescent_for)
        })
        .map(|(path, meta)| (path, meta.len()))
        .max_by_key(|(_, size)| *size)
}

/// The smallest corpus session over `min` bytes whose IR actually carries
/// capsules its own vendor can replay. Returns the path and the capsule count.
fn corpus_session_with_capsules(
    root: &Path,
    slug: &str,
    min: u64,
    max: u64,
) -> Option<(PathBuf, usize)> {
    let mut candidates: Vec<(u64, PathBuf)> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().map(|m| (m.len(), e.path().to_path_buf())))
        .filter(|(size, _)| *size >= min && *size <= max)
        .collect();
    candidates.sort();

    let vendor = ags::compare::vendor_of(slug)?;
    for (_, path) in candidates.into_iter().take(40) {
        let Ok(Some(ir)) = provider(slug).read_session_ir(&path) else {
            continue;
        };
        let fitting = ir
            .model_visible()
            .iter()
            .flat_map(|event| event.capsules.iter())
            .filter(|capsule| capsule.fits(vendor) == ags::ir::CapsuleFit::SameVendor)
            .count();
        if fitting > 0 {
            return Some((path, fitting));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Layout and root resolution
// ---------------------------------------------------------------------------

#[test]
fn the_store_creates_itself_under_ags_store() {
    let _lock = ENV.lock().expect("env lock");
    let home = tempfile::tempdir().expect("temp home");
    let root = home.path().join("chosen-root");
    let _guard = EnvGuard::set("AGS_STORE", &root);

    assert_eq!(
        ags::store::default_root().expect("root"),
        root,
        "$AGS_STORE wins over every default"
    );
    let store = Store::open().expect("open the default store");
    assert_eq!(store.root(), root.as_path());
    assert!(root.join("store.json").is_file(), "store.json is written");
    assert!(root.join("records").is_dir(), "records/ is created");
}

#[test]
fn a_record_holds_one_origin_and_a_derived_incarnation_per_conversion() {
    let (tmp, store) = fresh();
    let origin_path = write_session(tmp.path(), "rollout.jsonl", "{\"a\":1}\n");
    let written_path = write_session(tmp.path(), "cc.jsonl", "{\"b\":2}\n");
    let origin = SessionKey::new("codex", "01JORIGIN");
    let derived = SessionKey::new("claude-code", "a3fDERIVED");

    let record = store
        .ingest_origin(&origin, &origin_path, OriginPolicy::Reference)
        .expect("ingest");
    assert!(!record.id.is_empty(), "a fresh UUID names the conversation");

    let record = store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: derived.clone(),
                path: written_path,
                from: origin.clone(),
                fidelity: Fidelity::ContextNoReasoning,
                losses: vec![Loss {
                    kind: LossKind::Reasoning,
                    events: 41,
                    capsules: 30_082,
                    bytes: 1_234_567,
                    grade: Fidelity::ContextNoReasoning,
                    note: "openai capsules cannot be replayed by anthropic".to_string(),
                }],
            },
        )
        .expect("record conversion");

    assert_eq!(record.incarnations.len(), 2);
    assert_eq!(record.origin().expect("origin").key, origin);
    assert_eq!(
        record.for_provider("claude-code").expect("derived").key,
        derived
    );

    // Both keys resolve to the one conversation; that is the whole point.
    for key in [&origin, &derived] {
        assert_eq!(
            store
                .find_by_session(key)
                .expect("lookup")
                .expect("indexed")
                .id,
            record.id,
            "{key} should resolve to the conversation"
        );
    }

    // The losses survive a round trip through disk verbatim: they describe a
    // moment that cannot be recomputed.
    let reloaded = store.get(&record.id).expect("get").expect("record");
    let Role::Derived {
        from,
        fidelity,
        losses,
        snapshot,
    } = &reloaded
        .for_provider("claude-code")
        .expect("derived")
        .role
        .clone()
    else {
        panic!("expected a derived incarnation");
    };
    assert_eq!(from, &origin);
    assert_eq!(*fidelity, Fidelity::ContextNoReasoning);
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].capsules, 30_082);
    assert_eq!(losses[0].kind, LossKind::Reasoning);
    // And the file we wrote was snapshotted, which is what makes "the user has
    // since worked in this session" answerable by one `stat` later.
    assert!(
        snapshot.is_some(),
        "a derived incarnation carries its own snapshot, taken at record_conversion"
    );
}

// ---------------------------------------------------------------------------
// Cache 1: the IR, keyed by IR_VERSION
// ---------------------------------------------------------------------------

#[test]
fn an_ir_cache_stamped_with_an_older_version_is_deleted_not_migrated() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let record = store
        .ingest_origin(
            &SessionKey::new("codex", "01J"),
            &path,
            OriginPolicy::Reference,
        )
        .expect("ingest");

    let ir = SessionIr::new("codex", "01J");
    store.store_ir(&record.id, &ir).expect("cache the IR");
    let cache = store
        .root()
        .join("records")
        .join(&record.id)
        .join("ir.json");
    assert!(cache.is_file());
    assert_eq!(
        store.load_ir(&record.id).expect("load"),
        Some(ir),
        "a current cache is served"
    );

    // Plant the version that IR_VERSION superseded. `ags-ir/1` predates
    // `Body::Rollback`, `Body::Abort` and `SessionIr::live_head`, so a reader
    // that "migrated" it would be inventing history.
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache).expect("read")).expect("parse");
    assert_eq!(raw["ir_version"], serde_json::json!(IR_VERSION));
    raw["ir_version"] = serde_json::json!("ags-ir/1");
    std::fs::write(&cache, serde_json::to_vec(&raw).expect("encode")).expect("plant");

    assert_eq!(
        store.load_ir(&record.id).expect("load"),
        None,
        "a stale stamp means 'no cache', so the caller re-derives from origin bytes"
    );
    assert!(
        !cache.exists(),
        "and the stale file is gone: there is no migration path, ever"
    );
}

#[test]
fn an_unparseable_ir_cache_is_also_deleted() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let record = store
        .ingest_origin(
            &SessionKey::new("codex", "01J"),
            &path,
            OriginPolicy::Reference,
        )
        .expect("ingest");
    store
        .store_ir(&record.id, &SessionIr::new("codex", "01J"))
        .expect("cache");
    let cache = store
        .root()
        .join("records")
        .join(&record.id)
        .join("ir.json");
    std::fs::write(&cache, b"{ not json at all").expect("corrupt");

    assert_eq!(store.load_ir(&record.id).expect("load"), None);
    assert!(!cache.exists(), "a cache we cannot read is a cache we drop");
}

// ---------------------------------------------------------------------------
// Cache 2: the index
// ---------------------------------------------------------------------------

#[test]
fn fsck_rebuilds_a_deleted_index_to_the_same_content() {
    let (tmp, store) = fresh();

    // Three conversations, one of them with two incarnations: four index rows.
    let mut expected_records = Vec::new();
    for i in 0..3 {
        let path = write_session(tmp.path(), &format!("rollout{i}.jsonl"), "{}\n");
        let record = store
            .ingest_origin(
                &SessionKey::new("codex", format!("origin-{i}")),
                &path,
                OriginPolicy::Reference,
            )
            .expect("ingest");
        expected_records.push(record);
    }
    let derived_path = write_session(tmp.path(), "cc.jsonl", "{}\n");
    store
        .record_conversion(
            &expected_records[0].id,
            DerivedWrite {
                key: SessionKey::new("claude-code", "derived-0"),
                path: derived_path,
                from: SessionKey::new("codex", "origin-0"),
                fidelity: Fidelity::ConversationOnly,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");

    let before = index_rows(store.root());
    assert_eq!(before.len(), 4, "3 origins + 1 derived; got {before:?}");
    println!("index rows before deletion: {}", before.len());

    // Lose the entire index. It is a cache, so nothing authoritative went with
    // it — the record directories still hold every fact.
    std::fs::remove_file(store.root().join("index.sqlite")).expect("delete index");
    assert!(!store.root().join("index.sqlite").exists());

    let report = store.fsck(true).expect("fsck");
    println!(
        "fsck: {} records, {} incarnations, {} rows indexed, {} problems",
        report.records,
        report.incarnations,
        report.indexed,
        report.problems.len()
    );
    assert_eq!(report.records, 3);
    assert_eq!(report.incarnations, 4);
    assert_eq!(report.indexed, 4, "fsck wrote one row per incarnation");
    assert!(report.problems.is_empty(), "{:?}", report.problems);

    let after = index_rows(store.root());
    assert_eq!(after, before, "rebuilt index is byte-for-byte the same map");

    // And it still answers the question it exists to answer.
    assert_eq!(
        store
            .find_by_session(&SessionKey::new("claude-code", "derived-0"))
            .expect("lookup")
            .expect("hit")
            .id,
        expected_records[0].id
    );
}

/// A rebuild deletes every row and rewrites the ones *its own scan* saw, so the
/// scan has to be inside the write lock rather than before it.
///
/// It was before it. A conversion that committed in between had its
/// `record.json` on disk and its index row erased by the rebuild that followed —
/// the session becomes unfindable, so the next conversion mints a second record
/// for a conversation that already had one. That is a silent loss of visibility
/// produced by the one operation the whole crash-safety argument leans on.
#[test]
fn a_rebuild_cannot_erase_a_conversion_that_committed_while_it_scanned() {
    let (tmp, store) = fresh();
    let a_path = write_session(tmp.path(), "a.jsonl", "{}\n");
    let a_key = SessionKey::new("codex", "01JA");
    store
        .ingest_origin(&a_key, &a_path, OriginPolicy::Reference)
        .expect("ingest a");

    let b_path = write_session(tmp.path(), "b.jsonl", "{}\n");
    let b_key = SessionKey::new("codex", "01JB");
    let b_id = "b0000000-0000-4000-8000-00000000000b";

    // Stand in for the other invocation by holding the store's own write lock.
    // That is what makes the interleaving exact instead of lucky: the rebuild
    // cannot reach the lock until this transaction commits, and it has to be
    // done scanning before it tries.
    let mut conn =
        rusqlite::Connection::open(store.root().join("index.sqlite")).expect("open index");
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .expect("busy timeout");
    let held = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("begin immediate");

    let report = std::thread::scope(|scope| {
        let rebuild = scope.spawn(|| store.fsck(true).expect("fsck"));
        // One record directory to walk; a fifth of a second is not a race.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Session B's ingest finishes, in the store's own order: record on disk
        // first, index row second, commit last — all of it strictly before the
        // rebuild can take the lock.
        let dir = store.root().join("records").join(b_id);
        std::fs::create_dir_all(&dir).expect("record dir");
        let record = ags::store::Record {
            id: b_id.to_string(),
            created_at: 1,
            updated_at: 1,
            incarnations: vec![ags::store::Incarnation {
                key: b_key.clone(),
                path: b_path.clone(),
                recorded_at: 1,
                role: Role::Origin {
                    snapshot: OriginSnapshot::of(&b_path).expect("snapshot"),
                },
            }],
        };
        std::fs::write(
            dir.join("record.json"),
            serde_json::to_vec(&record).expect("encode"),
        )
        .expect("publish record b");
        held.execute(
            "INSERT INTO sessions (provider, provider_session_id, record_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![b_key.provider, b_key.provider_session_id, b_id],
        )
        .expect("claim b");
        held.commit().expect("commit b");

        rebuild.join().expect("rebuild thread")
    });

    println!(
        "fsck: {} records, {} rows indexed, {} problems",
        report.records,
        report.indexed,
        report.problems.len()
    );
    assert!(
        store.find_by_session(&b_key).expect("lookup").is_some(),
        "the rebuild erased the index row of a record that was already on disk"
    );
    assert_eq!(
        report.records, 2,
        "a rebuild has to reindex what is on disk when it takes the lock, not before"
    );
    assert_eq!(report.indexed, 2);
    assert_eq!(index_rows(store.root()).len(), 2);
}

#[test]
fn an_index_speaking_an_unknown_schema_is_rebuilt_rather_than_migrated() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let key = SessionKey::new("codex", "01J");
    let record = store
        .ingest_origin(&key, &path, OriginPolicy::Reference)
        .expect("ingest");

    // A schema this build does not know, with a table it cannot read.
    let conn = rusqlite::Connection::open(store.root().join("index.sqlite")).expect("open");
    conn.execute_batch("DROP TABLE sessions; CREATE TABLE sessions (nonsense TEXT);")
        .expect("mangle");
    conn.pragma_update(None, "user_version", 9_999_i32)
        .expect("bump schema version");
    drop(conn);

    assert_eq!(
        store
            .find_by_session(&key)
            .expect("lookup")
            .expect("hit")
            .id,
        record.id,
        "the index rebuilt itself from the records and answered anyway"
    );
    assert_eq!(index_rows(store.root()).len(), 1);
}

// ---------------------------------------------------------------------------
// store.json: readable from the future, not writable
// ---------------------------------------------------------------------------

#[test]
fn a_store_from_the_future_stays_readable_and_refuses_writes() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let key = SessionKey::new("codex", "01J");
    let record = store
        .ingest_origin(&key, &path, OriginPolicy::Reference)
        .expect("ingest");
    drop(store);

    // Someone with a newer build bumped the layout.
    let manifest_path = tmp.path().join("store").join("store.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read"))
            .expect("parse");
    manifest["store_version"] = serde_json::json!(999);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode"),
    )
    .expect("write manifest");

    let store = Store::open_at(tmp.path().join("store")).expect("a newer store still opens");
    assert_eq!(store.store_version(), 999);

    // Direction 1: reading works. Listing a record does not require
    // understanding every field a newer build may have added.
    let listed = store.list().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, record.id);
    assert_eq!(
        store.get(&record.id).expect("get").expect("record").id,
        record.id
    );
    assert_eq!(
        store
            .find_by_session(&key)
            .expect("lookup")
            .expect("hit")
            .id,
        record.id,
        "answered by scanning the records, without touching their index"
    );

    // Direction 2: every write is refused, by name.
    for (what, err) in [
        (
            "ingest_origin",
            store
                .ingest_origin(
                    &SessionKey::new("codex", "other"),
                    &path,
                    OriginPolicy::Reference,
                )
                .expect_err("ingest must be refused"),
        ),
        (
            "record_conversion",
            store
                .record_conversion(
                    &record.id,
                    DerivedWrite {
                        key: SessionKey::new("claude-code", "a3f"),
                        path: path.clone(),
                        from: key.clone(),
                        fidelity: Fidelity::ConversationOnly,
                        losses: Vec::new(),
                    },
                )
                .expect_err("recording must be refused"),
        ),
        (
            "store_ir",
            store
                .store_ir(&record.id, &SessionIr::new("codex", "01J"))
                .expect_err("caching must be refused"),
        ),
        (
            "fsck --rebuild-index",
            store.fsck(true).expect_err("rebuilding must be refused"),
        ),
    ] {
        match err.downcast_ref::<StoreError>() {
            Some(StoreError::NewerStore {
                found, supported, ..
            }) => {
                assert_eq!(*found, 999);
                assert_eq!(*supported, ags::store::STORE_VERSION);
            }
            other => panic!("{what} should refuse with NewerStore, got {other:?}"),
        }
    }

    // A read-only fsck still reports.
    let report = store.fsck(false).expect("read-only fsck");
    assert_eq!(report.records, 1);
    assert!(!report.index_rebuilt);
}

// ---------------------------------------------------------------------------
// Origin references: the three-way resolution
// ---------------------------------------------------------------------------

#[test]
fn origin_resolution_reports_match_growth_and_loss() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "first line\n");
    let key = SessionKey::new("codex", "01J");
    let record = store
        .ingest_origin(&key, &path, OriginPolicy::Reference)
        .expect("ingest");
    let Role::Origin { snapshot } = record.origin().expect("origin").role.clone() else {
        panic!("expected an origin");
    };

    // 1. Hash matches — usable at full fidelity.
    let state = snapshot.state(&path);
    assert!(state.usable());
    assert_eq!(state, OriginState::Unchanged { rehashed: false });
    println!("unchanged: {}", state.describe());

    // 2. The file grew and the stored prefix still matches: the normal case for
    //    a live, append-only session log.
    std::fs::write(&path, "first line\nsecond line\n").expect("append");
    let state = snapshot.state(&path);
    assert!(state.usable(), "an advanced session is still a source");
    assert_eq!(state, OriginState::Grew { added_bytes: 12 });
    println!("grew: {}", state.describe());

    // 3a. Diverged: something rewrote it.
    std::fs::write(&path, "a completely different session\n").expect("rewrite");
    let state = snapshot.state(&path);
    assert!(!state.usable());
    let OriginState::Unavailable { reason } = &state else {
        panic!("expected unavailable, got {state:?}");
    };
    assert!(reason.starts_with("diverged"), "got {reason}");
    println!("diverged: {}", state.describe());

    // 3b. Gone.
    std::fs::remove_file(&path).expect("remove");
    let state = snapshot.state(&path);
    assert!(!state.usable());
    let OriginState::Unavailable { reason } = &state else {
        panic!("expected unavailable, got {state:?}");
    };
    assert!(reason.starts_with("missing"), "got {reason}");
    println!("gone: {}", state.describe());

    // The choice reports the loss instead of papering over it.
    let choice = store.best_source_for(&record, provider("codex"), registry());
    assert!(choice.chosen().is_none(), "nothing left to read");
    let line = choice.explain(Some(&key));
    assert!(line.starts_with("no usable source for codex"), "got {line}");
    println!("{line}");
}

#[test]
fn a_truncated_origin_is_rejected_without_reading_a_byte() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "0123456789\n");
    let record = store
        .ingest_origin(
            &SessionKey::new("codex", "01J"),
            &path,
            OriginPolicy::Reference,
        )
        .expect("ingest");
    let Role::Origin { snapshot } = record.origin().expect("origin").role.clone() else {
        panic!("expected an origin");
    };

    std::fs::write(&path, "0123").expect("truncate");
    let state = snapshot.state(&path);
    let OriginState::Unavailable { reason } = &state else {
        panic!("expected unavailable, got {state:?}");
    };
    assert!(
        reason.contains("shrank from 11 to 4 bytes"),
        "the cheap length check answers this one; got {reason}"
    );
}

#[test]
fn archive_makes_a_record_survive_its_origin() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "irreplaceable\n");
    let record = store
        .ingest_origin(
            &SessionKey::new("codex", "01J"),
            &path,
            OriginPolicy::Archive,
        )
        .expect("ingest");
    std::fs::remove_file(&path).expect("delete the original");

    let choice = store.best_source_for(&record, provider("codex"), registry());
    let chosen = choice.chosen().expect("the archived copy is a source");
    assert_eq!(chosen.availability, Availability::Archived);
    assert_eq!(
        std::fs::read_to_string(&chosen.path).expect("read"),
        "irreplaceable\n"
    );
    assert!(
        choice.explain(None).contains("from the archived copy"),
        "the fallback is stated: {}",
        choice.explain(None)
    );

    // Without --archive the same deletion is unrecoverable, and says so.
    let (tmp2, store2) = fresh();
    let path2 = write_session(tmp2.path(), "rollout.jsonl", "irreplaceable\n");
    let record2 = store2
        .ingest_origin(
            &SessionKey::new("codex", "01J"),
            &path2,
            OriginPolicy::Reference,
        )
        .expect("ingest");
    std::fs::remove_file(&path2).expect("delete");
    assert!(
        store2
            .best_source_for(&record2, provider("codex"), registry())
            .chosen()
            .is_none(),
        "a reference buys availability, and availability is not backup"
    );
}

/// The point of `--archive`, in the one situation it exists for: the live origin
/// is gone and the conversation has to come back out of the byte copy.
///
/// The archive is *exactly* the bytes the store snapshotted, so it holds nothing
/// appended since — every derivative was made from precisely these bytes. Reading
/// the missing live file's `Unavailable` as the archive's own resolution made it
/// an unknown, the unknown made the record unmergeable, and an unmergeable record
/// defers to the session the user named: the Claude derivative, which is the one
/// incarnation that does *not* hold the sealed material `--archive` was paid for.
#[test]
fn an_archived_origin_is_still_selectable_after_its_live_file_is_gone() {
    let (tmp, store) = fresh();
    let codex_path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let cc_path = write_session(tmp.path(), "cc.jsonl", "{}\n");
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    let record = store
        .ingest_origin(&codex_key, &codex_path, OriginPolicy::Archive)
        .expect("ingest");
    let record = store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: cc_key.clone(),
                path: cc_path,
                from: codex_key.clone(),
                fidelity: Fidelity::ContextNoReasoning,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");
    store
        .store_ir(
            &record.id,
            &ir_with_capsules("codex", CapsuleKind::OpenaiReasoningEncryptedContent, 9),
        )
        .expect("cache the origin IR");

    // The user cleaned out `~/.codex/sessions`. Nothing touched the Claude side.
    std::fs::remove_file(&codex_path).expect("delete the live origin");

    let choice = store.best_source_for(&record, provider("codex"), registry());
    let archived = choice.find(&codex_key).expect("the origin is a candidate");
    assert_eq!(archived.availability, Availability::Archived);
    assert!(
        archived.advance.unmoved(),
        "the archive is the bytes the store recorded and cannot have advanced, got {:?}",
        archived.advance
    );
    assert!(
        !choice.unmergeable(),
        "nothing here diverged: {}",
        choice.explain(Some(&cc_key))
    );
    let chosen = choice.resolve(Some(&cc_key)).expect("a source");
    assert_eq!(
        chosen.key,
        codex_key,
        "converting back to codex has to read the archive, not the derivative: {}",
        choice.explain(Some(&cc_key))
    );
    assert_eq!(
        chosen.capsules.fitting(),
        9,
        "and the sealed material --archive was kept for comes back with it"
    );
}

// ---------------------------------------------------------------------------
// best_source_for
// ---------------------------------------------------------------------------

#[test]
fn best_source_depends_on_the_target_vendor() {
    let (tmp, store) = fresh();
    let codex_path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let cc_path = write_session(tmp.path(), "cc.jsonl", "{}\n");
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    let record = store
        .ingest_origin(&codex_key, &codex_path, OriginPolicy::Reference)
        .expect("ingest");
    let record = store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: cc_key.clone(),
                path: cc_path,
                from: codex_key.clone(),
                fidelity: Fidelity::ContextNoReasoning,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");

    // The origin holds sealed OpenAI material. The Claude derivative could not
    // carry it — that is exactly the loss the record above describes.
    store
        .store_ir(
            &record.id,
            &ir_with_capsules("codex", CapsuleKind::OpenaiReasoningEncryptedContent, 9),
        )
        .expect("cache the origin IR");

    // Same vendor as the sealed material: the origin is strictly better.
    let to_codex = store.best_source_for(&record, provider("codex"), registry());
    assert_eq!(to_codex.target_vendor, Some("openai"));
    assert_eq!(to_codex.chosen().expect("source").key, codex_key);
    assert_eq!(to_codex.chosen().unwrap().capsules.fitting(), 9);
    let line = to_codex.explain(Some(&cc_key));
    println!("{line}");
    assert!(
        line.contains("you named claude-code a3fCC, which would have cost 9 capsules"),
        "the counterfactual must be in the value, not in a log line: {line}"
    );

    // Foreign vendor: those nine capsules are worth nothing to Anthropic, so
    // the session that needs no conversion at all wins instead.
    let to_cc = store.best_source_for(&record, provider("claude-code"), registry());
    assert_eq!(to_cc.target_vendor, Some("anthropic"));
    assert_eq!(
        to_cc.chosen().expect("source").key,
        cc_key,
        "a Codex origin does NOT beat a Claude derivative when the target is Claude"
    );
    assert_eq!(to_cc.chosen().unwrap().capsules.fitting(), 0);
    let line = to_cc.explain(Some(&codex_key));
    println!("{line}");
    assert!(
        line.contains("would have needed another conversion"),
        "{line}"
    );

    // A target whose vendor this build does not know: neither vendor's capsules
    // can cross, the two are worth the same, and the store says the vendor is
    // unknown rather than guessing.
    let to_gemini = store.best_source_for(&record, provider("gemini"), registry());
    assert_eq!(to_gemini.target_vendor, None);
    for candidate in &to_gemini.candidates {
        assert_eq!(
            candidate.capsules.fitting(),
            0,
            "{} should carry nothing a gemini target can replay",
            candidate.key
        );
    }
    assert_eq!(
        to_gemini.chosen().expect("source").key,
        codex_key,
        "tied on capsules, the more complete copy of the conversation wins"
    );
}

// ---------------------------------------------------------------------------
// The rung above capsules
// ---------------------------------------------------------------------------

/// A codex origin holding `capsules` sealed capsules, plus one claude derivative.
///
/// Returned with both paths so a caller can append to either and re-rank.
fn diverging_pair(
    store: &Store,
    tmp: &Path,
    capsules: usize,
) -> (ags::store::Record, PathBuf, PathBuf) {
    let codex_path = write_session(tmp, "rollout.jsonl", "{}\n");
    let cc_path = write_session(tmp, "cc.jsonl", "{}\n");
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    let record = store
        .ingest_origin(&codex_key, &codex_path, OriginPolicy::Reference)
        .expect("ingest");
    let record = store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: cc_key,
                path: cc_path.clone(),
                from: codex_key,
                fidelity: Fidelity::ContextNoReasoning,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");
    store
        .store_ir(
            &record.id,
            &ir_with_capsules(
                "codex",
                CapsuleKind::OpenaiReasoningEncryptedContent,
                capsules,
            ),
        )
        .expect("cache the origin IR");
    (record, codex_path, cc_path)
}

/// Growth outranks capsules, and the explanation states the cost of that.
///
/// The defect this rung exists for: with capsules on top, the origin's nine
/// sealed capsules beat everything the user did in Claude afterwards, the second
/// hop wrote nothing, and the appended turns were gone with no way back.
#[test]
fn an_advanced_derivative_outranks_an_older_origin_with_more_capsules() {
    let (tmp, store) = fresh();
    let (record, _codex_path, cc_path) = diverging_pair(&store, tmp.path(), 9);
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    // Nothing has advanced: the derivative is a lossy projection of the origin,
    // holds nothing it lacks, and the capsule rung decides.
    let before = store.best_source_for(&record, provider("codex"), registry());
    assert_eq!(before.chosen().expect("source").key, codex_key);
    assert!(!before.unmergeable());

    // Two hours of work in Claude.
    std::fs::write(&cc_path, "{}\n{\"work\":\"two hours of it\"}\n").expect("append");

    let after = store.best_source_for(&record, provider("codex"), registry());
    assert_eq!(
        after.chosen().expect("source").key,
        cc_key,
        "content nothing else holds outranks capsules the origin still holds"
    );
    assert!(
        !after.unmergeable(),
        "one side advanced, so the ranking settled it"
    );
    let line = after.explain(Some(&cc_key));
    println!("{line}");
    assert!(
        line.contains("gives up 9 capsules") && line.contains("codex 01JCODEX still holds"),
        "the cost of keeping the newer work has to be stated: {line}"
    );
}

/// Both advanced: the ranking cannot settle it, so the named session is read.
#[test]
fn a_diverged_record_reads_the_named_session_and_names_both_costs() {
    let (tmp, store) = fresh();
    let (record, codex_path, cc_path) = diverging_pair(&store, tmp.path(), 9);
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    std::fs::write(&codex_path, "{}\n{\"more\":\"codex work\"}\n").expect("append to the origin");
    std::fs::write(&cc_path, "{}\n{\"more\":\"claude work\"}\n").expect("append to the derivative");

    let choice = store.best_source_for(&record, provider("codex"), registry());
    assert!(choice.unmergeable());
    assert_eq!(choice.unmerged().len(), 2);

    // Whichever the user names is what they get — which is exactly what
    // `--no-store` would have delivered, so the store cannot make it worse.
    for named in [&cc_key, &codex_key] {
        assert_eq!(
            choice.resolve(Some(named)).expect("a source").key,
            *named,
            "a record this design cannot merge defers to the session the user named"
        );
    }
    let line = choice.explain(Some(&cc_key));
    println!("{line}");
    assert!(line.contains("cannot merge two incarnations"), "{line}");
    assert!(
        line.contains("is missing whatever the others hold"),
        "{line}"
    );
    assert!(line.contains("gives up 9 capsules"), "{line}");
}

/// An unknown never hands the choice to the older-but-richer incarnation, and
/// never hands it to the other one either. It stops choosing.
#[test]
fn an_unknown_makes_the_record_unmergeable_rather_than_unmoved() {
    let (tmp, store) = fresh();
    let (record, _codex_path, cc_path) = diverging_pair(&store, tmp.path(), 9);
    let codex_key = SessionKey::new("codex", "01JCODEX");
    let cc_key = SessionKey::new("claude-code", "a3fCC");

    // A record from before derived incarnations were snapshotted. Never migrated.
    let mut without = record.clone();
    for incarnation in &mut without.incarnations {
        if let Role::Derived { snapshot, .. } = &mut incarnation.role {
            *snapshot = None;
        }
    }

    let choice = store.best_source_for(&without, provider("codex"), registry());
    assert!(
        choice.unmergeable(),
        "an unmeasurable derivative is an unknown, not a proof that it did not advance"
    );
    assert_eq!(choice.resolve(Some(&cc_key)).expect("source").key, cc_key);
    assert_eq!(
        choice.resolve(Some(&codex_key)).expect("source").key,
        codex_key,
        "and it may not be read as 'advanced' either, or naming the origin would cost 9 capsules \
         on a guess"
    );

    // A derivative the agent rewrote rather than appended to is the same answer.
    std::fs::write(&cc_path, "not the bytes we wrote\n").expect("rewrite");
    let diverged = store.best_source_for(&record, provider("codex"), registry());
    assert!(diverged.unmergeable());
    assert_eq!(
        diverged.resolve(Some(&cc_key)).expect("source").key,
        cc_key,
        "a rewritten derivative is still the user's own session and still readable"
    );
}

#[test]
fn re_ingesting_a_derived_session_does_not_overwrite_its_lineage() {
    let (tmp, store) = fresh();
    let origin_path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let derived_path = write_session(tmp.path(), "cc.jsonl", "{}\n");
    let origin = SessionKey::new("codex", "01J");
    let derived = SessionKey::new("claude-code", "a3f");

    let record = store
        .ingest_origin(&origin, &origin_path, OriginPolicy::Reference)
        .expect("ingest");
    store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: derived.clone(),
                path: derived_path.clone(),
                from: origin.clone(),
                fidelity: Fidelity::HistoryIncomplete,
                losses: vec![Loss {
                    kind: LossKind::SealedContext,
                    events: 4,
                    capsules: 4,
                    bytes: 87_600_000,
                    grade: Fidelity::HistoryIncomplete,
                    note: "a sealed compaction could not cross".to_string(),
                }],
            },
        )
        .expect("record conversion");

    // Naming the session we wrote must not turn it into an origin: that would
    // overwrite a measurement nothing can take again.
    let after = store
        .ingest_origin(&derived, &derived_path, OriginPolicy::Reference)
        .expect("re-ingest");
    assert_eq!(after.incarnations.len(), 2);
    assert_eq!(after.origin().expect("origin").key, origin);
    let Role::Derived {
        losses, fidelity, ..
    } = &after.for_provider("claude-code").unwrap().role
    else {
        panic!("the derived incarnation must stay derived");
    };
    assert_eq!(*fidelity, Fidelity::HistoryIncomplete);
    assert_eq!(
        losses[0].bytes, 87_600_000,
        "losses are records, not caches"
    );
}

#[test]
fn a_single_incarnation_costs_no_parse_and_still_reports_its_origin() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let key = SessionKey::new("codex", "01J");
    let record = store
        .ingest_origin(&key, &path, OriginPolicy::Reference)
        .expect("ingest");

    let choice = store.best_source_for(&record, provider("codex"), registry());
    assert_eq!(choice.candidates.len(), 1);
    assert_eq!(choice.chosen().expect("source").key, key);
    assert!(
        matches!(
            choice.chosen().unwrap().capsules,
            ags::store::Inventory::Unknown { .. }
        ),
        "with nothing to choose between, counting capsules would be work for nothing"
    );
    assert!(choice.chosen().unwrap().origin_state.is_some());
    assert_eq!(choice.explain(Some(&key)), "source: codex 01J (origin)");
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn two_invocations_ingesting_one_session_converge_on_one_record() {
    let (tmp, store) = fresh();
    let path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let key = SessionKey::new("codex", "01J");

    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| store.ingest_origin(&key, &path, OriginPolicy::Reference));
        let second = scope.spawn(|| store.ingest_origin(&key, &path, OriginPolicy::Reference));
        (
            first.join().expect("thread a").expect("ingest a"),
            second.join().expect("thread b").expect("ingest b"),
        )
    });

    assert_eq!(a.id, b.id, "one conversation, one record id");
    assert_eq!(
        store.list().expect("list").len(),
        1,
        "no orphan record dirs"
    );
    assert_eq!(index_rows(store.root()).len(), 1);
    let report = store.fsck(false).expect("fsck");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

/// The other half of the same race, and the one that says the fix is a lock and
/// not a merge: two invocations that share nothing but the store must not
/// interfere at all.
///
/// It failed for a reason that had nothing to do with either session — the index
/// is created lazily on first use, and two invocations both deciding to create
/// it would each drop the table the other had just filled.
#[test]
fn two_invocations_ingesting_different_sessions_keep_both() {
    let (tmp, store) = fresh();
    let codex_path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let cc_path = write_session(tmp.path(), "cc.jsonl", "{\"type\":\"user\"}\n");
    let codex_key = SessionKey::new("codex", "01J");
    let cc_key = SessionKey::new("claude-code", "a3f");

    let (a, b) = std::thread::scope(|scope| {
        let first =
            scope.spawn(|| store.ingest_origin(&codex_key, &codex_path, OriginPolicy::Reference));
        let second =
            scope.spawn(|| store.ingest_origin(&cc_key, &cc_path, OriginPolicy::Reference));
        (
            first.join().expect("thread a").expect("ingest a"),
            second.join().expect("thread b").expect("ingest b"),
        )
    });

    assert_ne!(a.id, b.id, "two conversations, two records");
    assert_eq!(store.list().expect("list").len(), 2);
    assert_eq!(
        index_rows(store.root()).len(),
        2,
        "neither invocation's index row was dropped by the other"
    );
    assert_eq!(
        store
            .find_by_session(&codex_key)
            .expect("lookup")
            .expect("hit")
            .id,
        a.id
    );
    assert_eq!(
        store
            .find_by_session(&cc_key)
            .expect("lookup")
            .expect("hit")
            .id,
        b.id
    );
    let report = store.fsck(false).expect("fsck");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

/// Two conversions of one conversation at once — `--to cc` in one terminal and
/// `--to gemini` in another — which is `ingest_origin`'s bug in the writer that
/// has more to lose.
///
/// An unlocked read-modify-write of `record.json` drops one of the two
/// incarnations, and with it that conversion's `losses`, which are records and
/// not caches: nothing recomputes them. The index row for the dropped
/// incarnation survives, so the cache is left naming a session its own record
/// does not hold.
#[test]
fn two_conversions_of_one_conversation_keep_both_lineages() {
    let (tmp, store) = fresh();
    let origin_path = write_session(tmp.path(), "rollout.jsonl", "{}\n");
    let origin = SessionKey::new("codex", "01J");
    let record = store
        .ingest_origin(&origin, &origin_path, OriginPolicy::Reference)
        .expect("ingest");

    let cc_path = write_session(tmp.path(), "cc.jsonl", "{\"type\":\"user\"}\n");
    let gemini_path = write_session(tmp.path(), "gemini.json", "{}\n");
    let cc_key = SessionKey::new("claude-code", "a3f");
    let gemini_key = SessionKey::new("gemini-cli", "g1");
    let loss = |capsules| Loss {
        kind: LossKind::Reasoning,
        events: 12,
        capsules,
        bytes: 4_096,
        grade: Fidelity::ContextNoReasoning,
        note: "openai capsules cannot cross vendors".to_string(),
    };

    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            store.record_conversion(
                &record.id,
                DerivedWrite {
                    key: cc_key.clone(),
                    path: cc_path.clone(),
                    from: origin.clone(),
                    fidelity: Fidelity::ContextNoReasoning,
                    losses: vec![loss(30_082)],
                },
            )
        });
        let second = scope.spawn(|| {
            store.record_conversion(
                &record.id,
                DerivedWrite {
                    key: gemini_key.clone(),
                    path: gemini_path.clone(),
                    from: origin.clone(),
                    fidelity: Fidelity::ContextNoReasoning,
                    losses: vec![loss(17)],
                },
            )
        });
        first.join().expect("thread a").expect("record a");
        second.join().expect("thread b").expect("record b");
    });

    let reloaded = store.get(&record.id).expect("get").expect("record");
    assert_eq!(
        reloaded.incarnations.len(),
        3,
        "the origin and both conversions: {:?}",
        reloaded
            .incarnations
            .iter()
            .map(|inc| inc.key.to_string())
            .collect::<Vec<_>>()
    );
    for (key, capsules) in [(&cc_key, 30_082), (&gemini_key, 17)] {
        let inc = reloaded.find(key).expect("incarnation");
        let Role::Derived { losses, .. } = &inc.role else {
            panic!("expected a derived incarnation for {key}");
        };
        assert_eq!(
            losses,
            &[loss(capsules)],
            "losses are records; a lost update destroys them"
        );
    }

    let rows = index_rows(store.root());
    assert_eq!(rows.len(), 3, "one row per incarnation: {rows:?}");
    for (provider, session_id, record_id) in rows {
        assert_eq!(record_id, record.id);
        assert!(
            reloaded
                .find(&SessionKey::new(provider.clone(), session_id.clone()))
                .is_some(),
            "the index names {provider} {session_id}, which the record does not hold"
        );
    }
    let report = store.fsck(false).expect("fsck");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

/// A lookup racing an ingest: the read half of the same contention.
///
/// Two things are asserted, and the second is the one the write ordering buys.
/// A reader never fails — a writer holding the store's lock is something to wait
/// for, not an error to report — and a session the index answers for always
/// resolves to a record that is already on disk and whole. That is only true
/// because `record.json` is renamed into place *before* the index row that names
/// it is committed.
/// Sets its flag however the writer thread ends, including by panic.
struct Release<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn a_lookup_racing_an_ingest_never_sees_half_a_record() {
    let (tmp, store) = fresh();
    let sessions: Vec<(SessionKey, PathBuf)> = (0..40)
        .map(|i| {
            (
                SessionKey::new("codex", format!("01J-{i}")),
                write_session(tmp.path(), &format!("rollout-{i}.jsonl"), "{}\n"),
            )
        })
        .collect();
    let done = std::sync::atomic::AtomicBool::new(false);

    let (hits, polls) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            // On drop, not at the end of the loop: a writer that fails must
            // still release the reader, or a regression turns this test into a
            // hang instead of a failure.
            let _release = Release(&done);
            for (key, path) in &sessions {
                store
                    .ingest_origin(key, path, OriginPolicy::Reference)
                    .expect("ingest");
            }
        });
        let reader = scope.spawn(|| {
            let (mut hits, mut polls) = (0usize, 0usize);
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                polls += 1;
                for (key, _) in &sessions {
                    let Some(record) = store.find_by_session(key).expect("lookup") else {
                        continue;
                    };
                    hits += 1;
                    // Whole, not half: the index answered for this session, so
                    // the record it named is readable and holds it as an origin.
                    let reloaded = store
                        .get(&record.id)
                        .expect("get")
                        .expect("the index named a record that is not on disk");
                    assert_eq!(reloaded.id, record.id);
                    assert!(
                        reloaded.find(key).is_some_and(|inc| inc.is_origin()),
                        "record {} does not hold {key} as an origin",
                        record.id
                    );
                }
                for record in store.list().expect("list") {
                    assert!(!record.incarnations.is_empty());
                }
            }
            (hits, polls)
        });
        writer.join().expect("writer");
        reader.join().expect("reader")
    });

    assert!(polls > 0, "the reader never ran alongside the writer");
    println!("reader: {polls} passes, {hits} hits while the writer ingested 40 sessions");
    assert_eq!(store.list().expect("list").len(), 40);
    assert_eq!(index_rows(store.root()).len(), 40);
    for (key, _) in &sessions {
        assert!(store.find_by_session(key).expect("lookup").is_some());
    }
}

// ---------------------------------------------------------------------------
// Measured against the real corpus
// ---------------------------------------------------------------------------

#[test]
fn origin_lookup_on_the_largest_corpus_rollout_is_a_stat_not_a_hash() {
    let Some(home) = dirs::home_dir() else {
        eprintln!("skipped: no home directory");
        return;
    };
    let sessions = home.join(".codex").join("sessions");
    let Some((path, size)) = largest_under(&sessions) else {
        eprintln!("skipped: no Codex corpus under {}", sessions.display());
        return;
    };

    let started = Instant::now();
    let snapshot = OriginSnapshot::of(&path).expect("snapshot");
    let ingest = started.elapsed();
    // `>=`, not `==`. The largest rollout on a machine that uses Codex is
    // routinely the *live* one, and it grows between `largest_under`'s stat and
    // this snapshot's — 1,024,951,233 bytes then 1,024,954,809 on the run that
    // found this, three and a half kilobytes of one turn apart. An equality here
    // fails on exactly the corpus the test was written to measure, and says
    // nothing about the thing being measured. Rollouts are append-only, so the
    // invariant that holds is that it never shrank.
    assert!(
        snapshot.size >= size,
        "the largest rollout shrank between the scan and the snapshot: {size} -> {}",
        snapshot.size
    );

    // The lookup a conversion actually performs: unchanged file, one stat.
    let mut fast = std::time::Duration::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let state = snapshot.state(&path);
        fast = fast.min(started.elapsed());
        assert_eq!(state, OriginState::Unchanged { rehashed: false });
    }

    // The same lookup with the cheap check defeated — the cost of the growth
    // case, which hashes the stored prefix rather than the whole file. The
    // corpus is read-only, so the snapshot's mtime is nudged instead of the
    // file's.
    let stale_stat = OriginSnapshot {
        mtime_ms: snapshot.mtime_ms - 1,
        ..snapshot.clone()
    };
    let started = Instant::now();
    let state = stale_stat.state(&path);
    let hashed = started.elapsed();
    assert_eq!(state, OriginState::Unchanged { rehashed: true });

    println!(
        "largest corpus rollout: {} ({:.1} MiB)",
        path.file_name().unwrap_or_default().to_string_lossy(),
        size as f64 / (1024.0 * 1024.0)
    );
    println!("  ingest snapshot (full hash): {ingest:?}");
    println!("  lookup, size+mtime match:    {fast:?}");
    println!("  lookup, prefix re-hashed:    {hashed:?}");

    assert!(
        fast < std::time::Duration::from_millis(5),
        "an unchanged origin must not cost a hash; took {fast:?}"
    );
    assert!(
        hashed > fast * 10,
        "sanity: hashing {size} bytes should dominate a stat ({hashed:?} vs {fast:?})"
    );
}

/// What a realistic candidate list costs, split by which half of it is cached.
///
/// `ir.json` caches the **origin's** IR and nothing else, which `docs/STORE.md`
/// flags as a consequence worth knowing before it surprises someone: ranking a
/// *derived* candidate re-parses that candidate's file every time. This measures
/// the surprise on real sessions rather than arguing about it — a record with an
/// origin and two derived incarnations, which is what a conversation that has been
/// converted twice looks like.
///
/// It asserts a budget rather than a ratio. The ratio is the wrong shape: the
/// cached half is a `serde_json` load of an IR and the uncached half is a provider
/// parse, and on a small session those are the same order of magnitude, so a
/// ratio assertion would fail on session size rather than on a regression. What
/// matters to a caller is whether selection is cheap next to the conversion it
/// precedes, and a conversion of these sessions is tens of milliseconds.
#[test]
fn ranking_a_realistic_candidate_list_is_cheap_next_to_the_conversion() {
    let Some(home) = dirs::home_dir() else {
        eprintln!("skipped: no home directory");
        return;
    };
    let (Some((codex_path, _)), Some((cc_path, _))) = (
        corpus_session_with_capsules(
            &home.join(".codex").join("sessions"),
            "codex",
            200_000,
            3_000_000,
        ),
        corpus_session_with_capsules(
            &home.join(".claude").join("projects"),
            "claude-code",
            200_000,
            3_000_000,
        ),
    ) else {
        eprintln!("skipped: corpus has no session pair carrying sealed material");
        return;
    };

    let (_tmp, store) = fresh();
    let origin = SessionKey::new("codex", "origin");
    let record = store
        .ingest_origin(&origin, &codex_path, OriginPolicy::Reference)
        .expect("ingest");
    // Two derived incarnations of the same file: the point is the *count* of
    // uncached candidates, and one real Claude transcript parsed twice costs
    // exactly what two of them would.
    let mut record = record;
    for name in ["derived-1", "derived-2"] {
        record = store
            .record_conversion(
                &record.id,
                DerivedWrite {
                    key: SessionKey::new("claude-code", name),
                    path: cc_path.clone(),
                    from: origin.clone(),
                    fidelity: Fidelity::HistoryIncomplete,
                    losses: Vec::new(),
                },
            )
            .expect("record conversion");
    }
    assert_eq!(record.incarnations.len(), 3);

    let target = provider("codex");

    // First call: nothing is cached, so all three candidates are parsed and the
    // origin's IR is written to `ir.json` on the way past.
    let started = Instant::now();
    let cold = store.best_source_for(&record, target, registry());
    let cold_elapsed = started.elapsed();
    assert_eq!(cold.chosen().expect("a source").key, origin);

    // Every later call: the origin comes off `ir.json` and the two derived
    // candidates are re-parsed, every time. This is the cost the design chose.
    let mut warm_elapsed = std::time::Duration::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let warm = store.best_source_for(&record, target, registry());
        warm_elapsed = warm_elapsed.min(started.elapsed());
        assert_eq!(warm.chosen().expect("a source").key, origin);
    }

    // The conversion this selection precedes, for scale: one parse of the origin
    // through the provider that owns it.
    let started = Instant::now();
    let parsed = target
        .read_session_ir(&codex_path)
        .expect("read")
        .expect("an IR");
    let one_parse = started.elapsed();
    assert!(!parsed.events.is_empty());

    println!(
        "candidate list: 1 codex origin ({} KiB) + 2 derived claude sessions ({} KiB each)",
        std::fs::metadata(&codex_path)
            .map(|m| m.len() / 1024)
            .unwrap_or(0),
        std::fs::metadata(&cc_path)
            .map(|m| m.len() / 1024)
            .unwrap_or(0)
    );
    println!("  first call, nothing cached:      {cold_elapsed:?}");
    println!("  later calls, origin from ir.json: {warm_elapsed:?}");
    println!("  one provider parse of the origin: {one_parse:?}");

    assert!(
        warm_elapsed < std::time::Duration::from_millis(500),
        "ranking three real candidates took {warm_elapsed:?}; at that price the selection costs \
         more than the conversion it exists to improve"
    );
}

#[test]
fn a_real_codex_origin_beats_a_real_claude_session_only_for_a_codex_target() {
    let Some(home) = dirs::home_dir() else {
        eprintln!("skipped: no home directory");
        return;
    };
    let (Some((codex_path, codex_capsules)), Some((cc_path, cc_capsules))) = (
        corpus_session_with_capsules(
            &home.join(".codex").join("sessions"),
            "codex",
            200_000,
            3_000_000,
        ),
        corpus_session_with_capsules(
            &home.join(".claude").join("projects"),
            "claude-code",
            200_000,
            3_000_000,
        ),
    ) else {
        eprintln!("skipped: corpus has no session pair carrying sealed material");
        return;
    };

    println!(
        "codex origin  {} — {codex_capsules} openai capsules",
        codex_path.file_name().unwrap_or_default().to_string_lossy()
    );
    println!(
        "claude source {} — {cc_capsules} anthropic capsules",
        cc_path.file_name().unwrap_or_default().to_string_lossy()
    );

    let (_tmp, store) = fresh();
    let codex_key = SessionKey::new("codex", "corpus-codex");
    let cc_key = SessionKey::new("claude-code", "corpus-cc");
    let record = store
        .ingest_origin(&codex_key, &codex_path, OriginPolicy::Reference)
        .expect("ingest");
    let record = store
        .record_conversion(
            &record.id,
            DerivedWrite {
                key: cc_key.clone(),
                path: cc_path,
                from: codex_key.clone(),
                fidelity: Fidelity::ContextNoReasoning,
                losses: Vec::new(),
            },
        )
        .expect("record conversion");

    let to_codex = store.best_source_for(&record, provider("codex"), registry());
    println!("→ codex: {}", to_codex.explain(Some(&cc_key)));
    assert_eq!(to_codex.chosen().expect("source").key, codex_key);
    assert_eq!(
        to_codex.chosen().unwrap().capsules.fitting(),
        codex_capsules,
        "counted through Capsule::fits, on real bytes"
    );

    let to_cc = store.best_source_for(&record, provider("claude-code"), registry());
    println!("→ claude-code: {}", to_cc.explain(Some(&codex_key)));
    assert_eq!(
        to_cc.chosen().expect("source").key,
        cc_key,
        "the same origin is the wrong source once the target changes vendor"
    );
    assert_eq!(to_cc.chosen().unwrap().capsules.fitting(), cc_capsules);
}
