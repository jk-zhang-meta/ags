//! What a listing already learned about a file, kept so it is not learned again.
//!
//! # Why this exists
//!
//! A listing row carries counts only a full read can produce — messages,
//! unique user messages, average agent response length, tool uses. On one real
//! machine the thirty most recent Codex rollouts are 1.45 GiB, so rendering
//! *two* rows of `ags convert list` parsed 1.45 GiB, and did it again on the next
//! invocation. Rollouts are append-only and most of them are finished, so
//! nearly every one of those parses reproduced the previous run's answer.
//!
//! # What makes reusing a row safe
//!
//! The key is the file's identity *and* its bytes: path, size, and mtime.
//! Anything that changes what a file says changes at least one of the two
//! stamps — appending changes both, a rewrite changes mtime, and a checkpoint
//! restore puts the *original* mtime back (`artifact_metadata=mode-mtime-v1`
//! in the AGS runtime), which is a different stamp from the live file it
//! replaced and so is read again rather than answered from the row it evicted.
//!
//! Nothing here decides *whether* a file is listed. That rule stays with the
//! provider, is applied to every candidate on every run, and a cache hit is
//! only ever a shortcut past re-deriving what an already-listed file said.
//!
//! # It is a cache in the sense [`crate::store`] uses the word
//!
//! Every row is reconstructible from the native bytes it names. So:
//!
//! - A schema or row version this build does not recognise is **dropped and
//!   rebuilt**, never migrated.
//! - A cache that will not open is not an error. The caller reads the files,
//!   which is what it did before this module existed.
//! - Rows are pruned by age, so a store that outlives the sessions it lists
//!   does not grow without bound.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use tracing::debug;

/// Schema of `listing.sqlite`, kept in `PRAGMA user_version`.
///
/// Bump when the table shape changes. A mismatch drops every row.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rows (
    path         TEXT PRIMARY KEY,
    size         INTEGER NOT NULL,
    mtime_millis INTEGER NOT NULL,
    row_version  INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    row          TEXT NOT NULL
) STRICT;
";

/// How long an untouched row survives.
///
/// Long enough that a store used weekly keeps its rows, short enough that a
/// machine whose sessions have been deleted does not carry their rows forever.
/// Nothing depends on the exact number: an evicted row costs one re-read.
const PRUNE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The file identity a cached row is bound to.
///
/// Both halves matter. Size alone misses an in-place rewrite of the same
/// length; mtime alone misses a filesystem whose timestamps have one-second
/// granularity, where a file appended to twice in the same second would keep
/// answering from the first read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub mtime_millis: i64,
}

impl Stamp {
    /// The stamp of `path`, or `None` when it cannot be measured.
    ///
    /// `None` is not a miss to be papered over: without a stamp there is no key,
    /// so the caller reads the file and stores nothing.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = path.metadata().ok()?;
        let mtime_millis = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis();
        Some(Self {
            size: meta.len(),
            mtime_millis: i64::try_from(mtime_millis).ok()?,
        })
    }
}

/// Whether a row derived from `cached` may still describe a file now stamped
/// `current`, given only that the file grew.
///
/// Appending to a session file cannot change what the listing shows to identify
/// it: the title is the first thing the user typed, the workspace is recorded in
/// the header, and the start time is the start time. What appending *does*
/// change is the activity time and the per-message counts — and the activity
/// time is exactly the stamp's own mtime, so it costs nothing to refresh.
///
/// That leaves the counts, which go stale. They are bounded rather than exact:
/// once the file has grown by more than a fifth, the row is re-derived from the
/// bytes. The bound is proportional on purpose — a fifth of a 1.16 GiB rollout
/// is a rare event, a fifth of a 200 KiB one is cheap to honour — so the work
/// scales with the file instead of with how often anyone runs `list`.
#[must_use]
pub fn reusable_after_growth(cached: Stamp, current: Stamp) -> bool {
    // Shrunk or rewritten: nothing about the old row can be trusted.
    if current.size < cached.size {
        return false;
    }
    // A row derived from nothing has no counts worth keeping.
    if cached.size == 0 {
        return false;
    }
    current.size - cached.size <= cached.size / 5
}

/// Rows read from the cache at the start of a listing, keyed by path.
///
/// Loaded in one query and consulted from many threads, because the parses it
/// replaces run in parallel and a `Connection` does not.
#[derive(Debug, Default)]
pub struct LoadedRows {
    rows: HashMap<PathBuf, (Stamp, String)>,
}

impl LoadedRows {
    /// The row cached for `path`, if one was stored against this exact stamp.
    pub fn get(&self, path: &Path, stamp: Stamp) -> Option<&str> {
        let (cached, row) = self.rows.get(path)?;
        (*cached == stamp).then_some(row.as_str())
    }

    /// The row cached for `path` and the stamp it was derived from, whatever
    /// that stamp is.
    ///
    /// For a file that is still being appended to — an agent session that is
    /// open right now — a stamp keyed on `(size, mtime)` can never match, so
    /// [`get`](Self::get) returns `None` on every run and the file is parsed
    /// again every time. Measured on one machine: 301 of 315 rows hit, and the
    /// 14 that missed were the live sessions, together 1.65 GiB re-parsed per
    /// listing, one of them 1.16 GiB on its own.
    ///
    /// The caller decides what a near-miss is worth; see `reusable_after_growth`.
    pub fn get_stale(&self, path: &Path) -> Option<(Stamp, &str)> {
        let (cached, row) = self.rows.get(path)?;
        Some((*cached, row.as_str()))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A listing cache rooted next to the session store.
#[derive(Debug)]
pub struct ListingCache {
    conn: Connection,
    row_version: i64,
}

impl ListingCache {
    /// Open the cache for rows of shape `row_version`.
    ///
    /// `row_version` is the caller's, not this module's: the cache stores an
    /// opaque string and cannot tell that the fields inside it changed meaning.
    /// A caller that changes what it serialises bumps its own number and every
    /// row written by the old shape is dropped.
    pub fn open(row_version: u32) -> anyhow::Result<Self> {
        let root = crate::store::default_root()?;
        std::fs::create_dir_all(&root)?;
        let conn = Connection::open(root.join("listing.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // Best-effort: a cache on a filesystem that refuses WAL still works in
        // the journal mode it has.
        if let Err(error) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
            debug!(%error, "leaving the listing cache in its current journal mode");
        }
        let cache = Self {
            conn,
            row_version: i64::from(row_version),
        };
        cache.prepare()?;
        Ok(cache)
    }

    /// Bring the file to this build's schema, dropping anything else.
    fn prepare(&self) -> anyhow::Result<()> {
        let found: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found != SCHEMA_VERSION {
            debug!(found, want = SCHEMA_VERSION, "rebuilding the listing cache");
            self.conn.execute_batch("DROP TABLE IF EXISTS rows;")?;
            self.conn.execute_batch(SCHEMA)?;
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
            return Ok(());
        }
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    /// Every row this build can use, in one query.
    pub fn load(&self) -> anyhow::Result<LoadedRows> {
        let mut statement = self
            .conn
            .prepare("SELECT path, size, mtime_millis, row FROM rows WHERE row_version = ?1")?;
        let mut rows = HashMap::new();
        let mut cursor = statement.query([self.row_version])?;
        while let Some(row) = cursor.next()? {
            let path: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let mtime_millis: i64 = row.get(2)?;
            let payload: String = row.get(3)?;
            let Ok(size) = u64::try_from(size) else {
                continue;
            };
            rows.insert(PathBuf::from(path), (Stamp { size, mtime_millis }, payload));
        }
        Ok(LoadedRows { rows })
    }

    /// Record what this run read, and drop what nothing has read in a month.
    ///
    /// One transaction: the rows are already computed, so a partial write buys
    /// nothing and a concurrent listing must not see half of it.
    pub fn store(&mut self, fresh: &[(PathBuf, Stamp, String)]) -> anyhow::Result<()> {
        let now = epoch_millis(SystemTime::now());
        let write = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut insert = write.prepare(
                "INSERT INTO rows (path, size, mtime_millis, row_version, last_seen, row)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                     size = excluded.size,
                     mtime_millis = excluded.mtime_millis,
                     row_version = excluded.row_version,
                     last_seen = excluded.last_seen,
                     row = excluded.row",
            )?;
            for (path, stamp, payload) in fresh {
                let Some(path) = path.to_str() else {
                    // A path this database cannot store is not a failure of the
                    // listing that produced it. It is read every time.
                    continue;
                };
                let Ok(size) = i64::try_from(stamp.size) else {
                    continue;
                };
                insert.execute(rusqlite::params![
                    path,
                    size,
                    stamp.mtime_millis,
                    self.row_version,
                    now,
                    payload
                ])?;
            }
        }
        let cutoff = now.saturating_sub(i64::try_from(PRUNE_AFTER.as_millis()).unwrap_or(i64::MAX));
        write.execute("DELETE FROM rows WHERE last_seen < ?1", [cutoff])?;
        write.commit()?;
        Ok(())
    }

    /// Mark rows that were reused, so reading a store keeps its cache alive.
    ///
    /// Without this a row is pruned a month after it was *written*, however
    /// often it has been read since — which would re-read every long-lived
    /// session once a month for no reason.
    pub fn touch(&mut self, hits: &[PathBuf]) -> anyhow::Result<()> {
        if hits.is_empty() {
            return Ok(());
        }
        let now = epoch_millis(SystemTime::now());
        let write = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut update = write.prepare("UPDATE rows SET last_seen = ?1 WHERE path = ?2")?;
            for path in hits {
                let Some(path) = path.to_str() else {
                    continue;
                };
                update.execute(rusqlite::params![now, path])?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn epoch_millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp distinguishes an appended file from the one it grew out of.
    #[test]
    fn a_changed_file_does_not_answer_from_its_old_row() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"one\n").expect("write");
        let before = Stamp::of(&path).expect("stamp");
        std::fs::write(&path, b"one\ntwo\n").expect("append");
        let after = Stamp::of(&path).expect("stamp");
        assert_ne!(before, after);

        let mut rows = LoadedRows::default();
        rows.rows.insert(path.clone(), (before, "row".to_string()));
        assert_eq!(rows.get(&path, before), Some("row"));
        assert_eq!(rows.get(&path, after), None);
    }

    /// A missing file has no stamp, so it has no key and cannot be cached.
    #[test]
    fn a_missing_file_has_no_stamp() {
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(Stamp::of(&dir.path().join("absent.jsonl")).is_none());
    }

    fn stamp(size: u64) -> Stamp {
        Stamp {
            size,
            mtime_millis: 0,
        }
    }

    #[test]
    fn a_file_that_only_grew_a_little_keeps_its_row() {
        // The case this exists for: a session that is open right now. Its stamp
        // can never match, so without this every listing re-parses it — measured
        // at 1.65 GiB per run on one machine.
        assert!(reusable_after_growth(stamp(1_000), stamp(1_100)));
        assert!(reusable_after_growth(stamp(1_000), stamp(1_000)));
    }

    #[test]
    fn growth_past_a_fifth_earns_a_fresh_parse() {
        // The bound is what keeps the counts from drifting without limit.
        assert!(!reusable_after_growth(stamp(1_000), stamp(1_201)));
        assert!(reusable_after_growth(stamp(1_000), stamp(1_200)));
    }

    #[test]
    fn the_bound_is_proportional_not_absolute() {
        // A fifth of a 1.16 GiB rollout is a rare event; a fifth of a small one
        // is cheap to honour. Scaling with the file is the point — an absolute
        // byte bound would re-parse the giant file constantly and the small one
        // never.
        let giant = 1_160_000_000;
        assert!(reusable_after_growth(
            stamp(giant),
            stamp(giant + 200_000_000)
        ));
        assert!(!reusable_after_growth(
            stamp(giant),
            stamp(giant + 300_000_000)
        ));
    }

    #[test]
    fn a_file_that_shrank_was_rewritten_and_keeps_nothing() {
        assert!(!reusable_after_growth(stamp(1_000), stamp(999)));
    }

    #[test]
    fn a_row_derived_from_an_empty_file_is_never_reused() {
        // Nothing was counted, so there is nothing worth carrying forward.
        assert!(!reusable_after_growth(stamp(0), stamp(10)));
    }
}
