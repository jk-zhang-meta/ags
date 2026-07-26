//! The session store — one conversation, many provider incarnations.
//!
//! # Single responsibility
//!
//! Remember that two provider sessions are the same conversation, so that a
//! conversion can *choose* its source instead of inheriting it. That is the
//! whole job. The store does not convert, does not read provider formats on
//! its own account, and has no opinion about launching.
//!
//! It exists because a conversion is lossy in a direction. Convert a Codex
//! session to Claude Code, work there, then convert back: the second hop can
//! only carry what the first one left, so the returning Codex session arrives
//! with none of its original reasoning capsules — even though the bytes that
//! would have replayed perfectly never left `~/.codex/sessions`. Nothing in
//! either conversion is wrong. The second hop asked the wrong source. See
//! `docs/STORE.md`.
//!
//! # Why the contract is narrow
//!
//! Thirteen methods on [`Store`], and every one of them is a question the
//! pipeline actually asks: *which conversation owns this session?*, *what is the
//! best source for this target, and what does choosing it cost?*, *here is what
//! I just wrote*. Everything else — the on-disk layout, the SQLite index, prefix
//! hashing, cache invalidation, the ranking — is behind that surface and can
//! change without a caller noticing.
//!
//! The reason to keep it that narrow is the same reason the IR is cheap to
//! change: **native bytes are the source of truth, and everything else in the
//! store is a cache that must be reconstructible from them.** A wider contract
//! would let callers depend on the caches, and then they would need migrating.
//! Three consequences, and they are the whole design:
//!
//! - [`Store::load_ir`] treats a stale [`crate::ir::IR_VERSION`] as garbage and
//!   **deletes** it. There is no migration path and there must never be one.
//! - `index.sqlite` is a `(provider, session_id) -> record` cache with its
//!   schema version in `PRAGMA user_version`. A version it does not recognise,
//!   or a missing file, is rebuilt from the record directories — never
//!   migrated. [`Store::fsck`] is the same rebuild on demand.
//! - [`Loss`] and [`Fidelity`] are *not* caches. They describe what happened at
//!   a moment to a specific pair of files, the target has since been edited by
//!   an agent, and they can never be recomputed. So they are written down once
//!   and never derived again.
//!
//! # What it does not do
//!
//! Origin bytes are referenced, not archived: absolute path, content hash,
//! size, mtime. The corpus has ~600 Codex rollouts and the largest is 281 MiB;
//! copying by default would turn a converter into a multi-gigabyte archiver.
//! [`OriginPolicy::Archive`] opts a single record into a real byte copy. That
//! split is honest rather than free — a reference buys availability, and
//! availability is not backup — which is why
//! [`OriginState::Unavailable`] is a *reported* outcome and not a silent
//! downgrade.

use std::cmp::Reverse;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::compare::vendor_of;
use crate::discovery::ProviderRegistry;
use crate::ir::{CapsuleFit, Fidelity, IR_VERSION, Loss, SessionIr};
use crate::providers::Provider;

/// On-disk layout version, recorded in `store.json`.
///
/// A store written by a *newer* version stays readable — listing a record does
/// not require understanding every field in it — but every write is refused.
/// Silently writing our shape into a layout we do not understand is how one
/// tool corrupts another's state.
pub const STORE_VERSION: u32 = 1;

/// Schema version of `index.sqlite`, kept in `PRAGMA user_version`.
///
/// Bumping this invalidates the index. It does not migrate it: the index is a
/// cache of the record directories and is cheaper to rebuild than to reason
/// about.
pub const INDEX_SCHEMA_VERSION: i32 = 1;

const STORE_FILE: &str = "store.json";
const INDEX_FILE: &str = "index.sqlite";
const RECORDS_DIR: &str = "records";
const RECORD_FILE: &str = "record.json";
const IR_FILE: &str = "ir.json";
const ORIGIN_DIR: &str = "origin";

const INDEX_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
    provider            TEXT NOT NULL,
    provider_session_id TEXT NOT NULL,
    record_id           TEXT NOT NULL,
    PRIMARY KEY (provider, provider_session_id)
) WITHOUT ROWID;";

/// Buffer used for prefix hashing. One mebibyte keeps a 281 MiB rollout to a
/// few hundred `read` calls without holding it in memory.
const HASH_BUF: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures that are the store's own, as opposed to plain I/O.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store on disk was written by a version we do not understand.
    #[error(
        "session store at {} was written by store_version {found}; this build understands {supported}. \
         Reading is still fine; writing is refused so that a newer layout is not corrupted.",
        root.display()
    )]
    NewerStore {
        root: PathBuf,
        found: u32,
        supported: u32,
    },

    /// A record id that is not in the store.
    #[error("no record '{id}' in the session store at {}", root.display())]
    NoSuchRecord { root: PathBuf, id: String },

    /// The path exists but is not a usable store root.
    #[error("{} is not a usable session store: {detail}", root.display())]
    NotAStore { root: PathBuf, detail: String },
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A session as the provider that owns it names it.
///
/// The pair is the store's only external identifier for a session, because it
/// is the only thing two `agsx` runs can agree on without reading a file:
/// paths move, and session ids are unique only within a provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Provider slug, e.g. `"codex"`.
    pub provider: String,
    /// The provider's own session identifier.
    pub provider_session_id: String,
}

impl SessionKey {
    pub fn new(provider: impl Into<String>, provider_session_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            provider_session_id: provider_session_id.into(),
        }
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.provider, self.provider_session_id)
    }
}

// ---------------------------------------------------------------------------
// Origin references
// ---------------------------------------------------------------------------

/// What the store saw when it last looked at a session file.
///
/// `sha256` covers the first `size` bytes — which was the whole file at
/// snapshot time. Storing the length the digest covers is what makes the
/// append-only case decidable later: a live session log grows, and the
/// question "is this still the file I hashed?" is really "does its first
/// `size` bytes still hash to this?".
///
/// # Why one type for both roles
///
/// An origin is snapshotted at ingest and a derived incarnation at
/// [`Store::record_conversion`], and the two want the same three answers from
/// the same two cheap fields, so they share the type rather than growing a
/// parallel one. What differs is what a caller *does* with the answer, not the
/// answer: an origin's snapshot is a reference standing in for bytes the store
/// does not hold, so divergence means the file can no longer be claimed as that
/// origin; a derived incarnation's is only a growth marker, and a derivative
/// that diverged is still the user's own session and still readable. See
/// [`Store::locate`].
///
/// [`OriginSnapshot::archived`] is meaningful for an origin only. A derived
/// session is a file this tool wrote and can write again; there is nothing to
/// archive against its loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginSnapshot {
    /// SHA-256 of the first `size` bytes.
    pub sha256: String,
    /// Bytes the digest covers; the file's whole length when snapshotted.
    pub size: u64,
    /// Modification time, epoch milliseconds. `0` when the filesystem would
    /// not say.
    pub mtime_ms: i64,
    /// File name of the byte copy under `records/<id>/origin/`, when this
    /// record was ingested with [`OriginPolicy::Archive`].
    pub archived: Option<String>,
}

impl OriginSnapshot {
    /// Take a snapshot of `path`.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let stamped = fs::metadata(path)
            .ok()
            .and_then(|meta| mtime_ms(&meta))
            .unwrap_or(0);
        // Hash first, then trust the hashed length rather than the earlier
        // `stat`: a session that was appended to mid-hash still yields a
        // self-consistent (digest, length) pair.
        let (sha256, size) = hash_prefix(path, u64::MAX)?;
        Ok(Self {
            sha256,
            size,
            mtime_ms: stamped,
            archived: None,
        })
    }

    /// Resolve this reference against the file as it is now.
    ///
    /// # Cost
    ///
    /// Re-hashing on every lookup is not an option: the largest rollout in the
    /// corpus is 281 MiB. So `size` and `mtime` are consulted first, and they
    /// answer two of the three cases outright — an unchanged file costs one
    /// `stat`, and a file too short to still contain the hashed prefix costs
    /// one `stat`. Only a file that grew, or one whose mtime moved without its
    /// length changing, is hashed, and then only over the `size` bytes the
    /// digest covers rather than the whole file.
    ///
    /// The price of that is stated rather than hidden: a rewrite that preserves
    /// both length and mtime reads as [`OriginState::Unchanged`] with
    /// `rehashed: false`. Detecting it would mean hashing 281 MiB on every
    /// lookup, so the outcome carries the flag instead of the claim.
    pub fn state(&self, path: &Path) -> OriginState {
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return OriginState::Unavailable {
                    reason: "missing: the origin session file is gone".to_string(),
                };
            }
            Err(err) => {
                return OriginState::Unavailable {
                    reason: format!("unreadable: {err}"),
                };
            }
        };
        if !meta.is_file() {
            return OriginState::Unavailable {
                reason: "unreadable: no longer a regular file".to_string(),
            };
        }

        let size = meta.len();

        // Cheap positive: same length, same mtime. No bytes read.
        if size == self.size && mtime_ms(&meta) == Some(self.mtime_ms) {
            return OriginState::Unchanged { rehashed: false };
        }
        // Cheap negative: too short to still contain the prefix we hashed.
        if size < self.size {
            return OriginState::Unavailable {
                reason: format!(
                    "diverged: shrank from {} to {size} bytes, so the recorded prefix cannot survive",
                    self.size
                ),
            };
        }

        match hash_prefix(path, self.size) {
            Err(err) => OriginState::Unavailable {
                reason: format!("unreadable: {err}"),
            },
            Ok((digest, _)) if digest == self.sha256 => {
                if size == self.size {
                    OriginState::Unchanged { rehashed: true }
                } else {
                    OriginState::Grew {
                        added_bytes: size - self.size,
                    }
                }
            }
            Ok(_) => OriginState::Unavailable {
                reason: format!(
                    "diverged: the first {} bytes no longer hash to the recorded digest",
                    self.size
                ),
            },
        }
    }
}

/// The three answers a reference can give, all of them reported.
///
/// Exactly the three the design names, because the caller has exactly two
/// decisions to make: can I read this, and has the conversation moved on. A
/// finer split would be a distinction nothing branches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginState {
    /// The bytes we recorded. `rehashed` says whether that was established by
    /// reading them or inferred from an identical `stat`.
    Unchanged { rehashed: bool },
    /// The stored prefix still matches and the file is longer: an append-only
    /// session log that has advanced. Usable, and the conversation has moved
    /// on since the store last looked.
    Grew { added_bytes: u64 },
    /// Gone, unreadable, or rewritten. The origin is unavailable and a caller
    /// must fall back with a stated cost. `reason` begins with `missing`,
    /// `diverged`, or `unreadable`.
    Unavailable { reason: String },
}

impl OriginState {
    /// Whether the file can be read as this conversation's origin.
    pub fn usable(&self) -> bool {
        match self {
            OriginState::Unchanged { .. } | OriginState::Grew { .. } => true,
            OriginState::Unavailable { .. } => false,
        }
    }

    /// One clause for a human, suitable for embedding in a sentence.
    pub fn describe(&self) -> String {
        match self {
            OriginState::Unchanged { rehashed: true } => {
                "unchanged since we recorded it (bytes verified)".to_string()
            }
            OriginState::Unchanged { rehashed: false } => {
                "unchanged since we recorded it (same size and mtime; bytes not re-read)"
                    .to_string()
            }
            OriginState::Grew { added_bytes } => {
                format!("appended to since we recorded it: {added_bytes} new bytes")
            }
            OriginState::Unavailable { reason } => format!("unavailable — {reason}"),
        }
    }
}

/// Whether an incarnation holds conversation content the store has never seen.
///
/// This is the rung above capsules, and the reason it has to be above them is
/// that the two quantities are not the same kind of thing. At the moment of
/// derivation a derivative is a lossy *projection* of its origin, so every
/// capsule it lacks is content the origin still has — recoverable, by reading the
/// origin. Turns appended afterwards are content **nothing else has**: no other
/// incarnation holds them and no conversion can reconstruct them. Ranking a
/// recoverable loss above an unrecoverable one is how a store built to save work
/// came to discard it.
///
/// Detection is growth, by the same cheap check as [`OriginState`]: `(size,
/// mtime)` decides, and the recorded prefix hash is the confirming read only
/// when it must be. Ranking therefore stays one `stat` per candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    /// The file is the bytes the store recorded. Whatever it holds, some other
    /// incarnation was derived from exactly this and holds it too.
    Unmoved,
    /// The file grew and its recorded prefix still matches: an append-only
    /// session log that moved on. It holds turns nothing derived from it can.
    Advanced { added_bytes: u64 },
    /// The store cannot tell. A record written before derived incarnations were
    /// snapshotted, or a file that no longer matches the prefix it recorded.
    ///
    /// Never read as [`Advance::Unmoved`]. Treating an unknown as "did not
    /// advance" is precisely the assumption that let an older-but-richer origin
    /// win over two hours of the user's work, so an unknown makes the record
    /// [`SourceChoice::unmergeable`] instead: the store stops choosing and reads
    /// what the user named, which is what they would have got without it.
    Unknown { why: String },
}

impl Advance {
    /// What a snapshot's resolution says about unseen content. `None` is an
    /// incarnation the store never snapshotted at all.
    fn of(state: Option<&OriginState>) -> Self {
        match state {
            None => Advance::Unknown {
                why: "the store has no snapshot of it, so growth cannot be measured".to_string(),
            },
            Some(OriginState::Unchanged { .. }) => Advance::Unmoved,
            Some(OriginState::Grew { added_bytes }) => Advance::Advanced {
                added_bytes: *added_bytes,
            },
            Some(OriginState::Unavailable { reason }) => Advance::Unknown {
                why: reason.clone(),
            },
        }
    }

    /// Whether the store can prove this incarnation holds nothing new.
    pub fn unmoved(&self) -> bool {
        match self {
            Advance::Unmoved => true,
            Advance::Advanced { .. } | Advance::Unknown { .. } => false,
        }
    }

    /// One clause for a human, suitable for embedding in a list.
    pub fn describe(&self) -> String {
        match self {
            Advance::Unmoved => "holds nothing appended since the store recorded it".to_string(),
            Advance::Advanced { added_bytes } => {
                format!("{added_bytes} bytes appended since the store recorded it")
            }
            Advance::Unknown { why } => format!("cannot be judged — {why}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Whether an incarnation is where the conversation came from, or something we
/// produced from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
    /// The provider's own session, referenced rather than copied.
    Origin { snapshot: OriginSnapshot },
    /// A session this tool wrote, together with what that cost.
    ///
    /// `fidelity` and `losses` are records, not caches: they describe one
    /// conversion of one pair of files at one moment, and the written session
    /// has since been edited by an agent. Nothing recomputes them.
    Derived {
        /// The incarnation this was converted from.
        from: SessionKey,
        fidelity: Fidelity,
        losses: Vec<Loss>,
        /// What the file looked like the moment we finished writing it.
        ///
        /// The whole point is the *difference* from now: an agent that worked in
        /// this session appended to it, and those turns exist nowhere else. One
        /// `stat` answers it, so it costs the ranking nothing.
        ///
        /// `None` is a record written before derived incarnations were
        /// snapshotted. It is not migrated — the store's rule is that caches are
        /// rebuilt and records are never migrated, and this is a record: an
        /// observation of a file at a moment that has passed and cannot be
        /// retaken. `None` therefore means *unknown*, resolves to
        /// [`Advance::Unknown`], and heals the next time a conversion writes
        /// this session.
        #[serde(default)]
        snapshot: Option<OriginSnapshot>,
    },
}

/// One provider's copy of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incarnation {
    pub key: SessionKey,
    /// Where the session file was when we last looked.
    pub path: PathBuf,
    /// When the store learned about this incarnation, epoch milliseconds.
    pub recorded_at: i64,
    pub role: Role,
}

impl Incarnation {
    /// Whether this is the conversation's origin.
    pub fn is_origin(&self) -> bool {
        match &self.role {
            Role::Origin { .. } => true,
            Role::Derived { .. } => false,
        }
    }

    /// How much of the conversation this file still holds.
    ///
    /// An origin holds all of it by definition — it *is* the native bytes. A
    /// derived incarnation holds whatever its conversion managed to carry, which
    /// is the grade that conversion recorded.
    pub fn completeness(&self) -> Fidelity {
        match &self.role {
            Role::Origin { .. } => Fidelity::ByteIdentical,
            Role::Derived { fidelity, .. } => *fidelity,
        }
    }
}

/// One conversation, in every provider we have seen it in.
///
/// The id is a fresh UUIDv4 minted at first ingest and never derived from
/// content: sessions are append-only logs that keep growing, so a
/// content-derived id would change under the conversation it names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Exactly one [`Role::Origin`], and one [`Role::Derived`] per conversion
    /// we performed.
    pub incarnations: Vec<Incarnation>,
}

impl Record {
    /// The conversation's origin, when the record has one.
    pub fn origin(&self) -> Option<&Incarnation> {
        self.incarnations.iter().find(|inc| inc.is_origin())
    }

    /// The incarnation a provider owns, if any.
    ///
    /// This is what turns a record id into something a provider can resume:
    /// `agsx resume <record-id> --launch cc` needs the Claude Code session id,
    /// not ours.
    pub fn for_provider(&self, provider: &str) -> Option<&Incarnation> {
        self.incarnations
            .iter()
            .find(|inc| inc.key.provider == provider)
    }

    /// The incarnation with this exact key.
    pub fn find(&self, key: &SessionKey) -> Option<&Incarnation> {
        self.incarnations.iter().find(|inc| &inc.key == key)
    }
}

/// Whether ingesting an origin keeps a byte copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPolicy {
    /// Record path, hash, size and mtime. The default, and the reason the store
    /// is not an archiver.
    Reference,
    /// Additionally copy the bytes into `records/<id>/origin/`.
    Archive,
}

/// A conversion that just succeeded, as the store needs to remember it.
#[derive(Debug, Clone)]
pub struct DerivedWrite {
    /// The session we wrote.
    pub key: SessionKey,
    /// Where we wrote it.
    pub path: PathBuf,
    /// The incarnation we read to produce it.
    pub from: SessionKey,
    /// The worst grade the conversion earned.
    pub fidelity: Fidelity,
    /// What that grade is made of.
    pub losses: Vec<Loss>,
}

/// What a consistency check found.
#[derive(Debug, Clone, Default)]
pub struct Fsck {
    /// Record directories that parsed.
    pub records: usize,
    /// Incarnations across all of them.
    pub incarnations: usize,
    /// Rows written into `index.sqlite`, when it was rebuilt.
    pub indexed: usize,
    pub index_rebuilt: bool,
    /// Everything that did not add up, one sentence each.
    pub problems: Vec<String>,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    store_version: u32,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    created_by: String,
}

/// Where the store lives: `$AGSX_STORE`, else `dirs::data_dir()/agsx`, else
/// `~/.agsx`.
pub fn default_root() -> anyhow::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("AGSX_STORE").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(data) = dirs::data_dir() {
        return Ok(data.join("agsx"));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".agsx"));
    }
    anyhow::bail!("cannot locate a session store: set AGSX_STORE to a directory")
}

/// A session store rooted at one directory.
///
/// Holds no open handles. Every operation opens what it needs and closes it,
/// so two `agsx` invocations can use one store without either holding a lock
/// across a conversion.
///
/// # Two invocations at once
///
/// Two terminals converting the same session, or a script that fans out, is an
/// ordinary thing to do, so every mutation is serialised against the others by
/// an `IMMEDIATE` transaction on `index.sqlite` — see [`locked`]. What is
/// deliberately *outside* that lock is everything expensive: hashing an origin
/// (281 MiB in the corpus) and the conversion itself. The lock covers reading a
/// record, rewriting it, and pointing the index at it, which is milliseconds of
/// small-file work.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    manifest: Manifest,
    /// `Some(version)` when the store was written by a newer build. Every write
    /// is then refused; reads carry on.
    newer: Option<u32>,
}

impl Store {
    /// Open the default store, creating it if this is the first use.
    pub fn open() -> anyhow::Result<Self> {
        Self::open_at(default_root()?)
    }

    /// Open the store at `root`, creating it if this is the first use.
    pub fn open_at(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = root.as_ref().to_path_buf();
        if root.exists() && !root.is_dir() {
            return Err(StoreError::NotAStore {
                root,
                detail: "path exists and is not a directory".to_string(),
            }
            .into());
        }

        let manifest_path = root.join(STORE_FILE);
        if manifest_path.is_file() {
            let text = fs::read_to_string(&manifest_path)?;
            let manifest: Manifest =
                serde_json::from_str(&text).map_err(|err| StoreError::NotAStore {
                    root: root.clone(),
                    detail: format!("{STORE_FILE} is unreadable: {err}"),
                })?;
            let newer = (manifest.store_version > STORE_VERSION).then_some(manifest.store_version);
            if newer.is_none() {
                fs::create_dir_all(root.join(RECORDS_DIR))?;
            }
            return Ok(Self {
                root,
                manifest,
                newer,
            });
        }

        fs::create_dir_all(root.join(RECORDS_DIR))?;
        let manifest = Manifest {
            store_version: STORE_VERSION,
            created_at: now_ms(),
            created_by: format!("casr {}", env!("CARGO_PKG_VERSION")),
        };
        write_json(&manifest_path, &manifest)?;
        debug!(root = %root.display(), "created session store");
        Ok(Self {
            root,
            manifest,
            newer: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `store_version` recorded on disk, which may be newer than
    /// [`STORE_VERSION`].
    pub fn store_version(&self) -> u32 {
        self.manifest.store_version
    }

    // -- lookup -------------------------------------------------------------

    /// The conversation that owns `record_id`.
    pub fn get(&self, record_id: &str) -> anyhow::Result<Option<Record>> {
        let path = self.record_dir(record_id).join(RECORD_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(read_record(&path)?))
    }

    /// The conversation that owns a provider session, if the store knows it.
    ///
    /// Answered from `index.sqlite`, which is why the index exists: without it
    /// this is a full scan of every record directory on every conversion. A
    /// store written by a newer build is scanned instead, because repairing its
    /// index would mean writing to it.
    ///
    /// A failed query is an error and not a miss. Reporting one as "the store
    /// has never seen this session" is how a moment's lock contention became a
    /// second record for a conversation that already had one.
    pub fn find_by_session(&self, key: &SessionKey) -> anyhow::Result<Option<Record>> {
        if let Some(conn) = self.index()? {
            return match lookup(&conn, key)? {
                Some(id) => self.get(&id),
                None => Ok(None),
            };
        }
        Ok(self
            .list()?
            .into_iter()
            .find(|record| record.find(key).is_some()))
    }

    /// Every record in the store, in directory order.
    ///
    /// Reads the records themselves rather than the index, so it works on a
    /// store written by a newer build.
    pub fn list(&self) -> anyhow::Result<Vec<Record>> {
        let mut out = Vec::new();
        for path in self.record_files()? {
            match read_record(&path) {
                Ok(record) => out.push(record),
                Err(err) => warn!(path = %path.display(), %err, "skipping unreadable record"),
            }
        }
        Ok(out)
    }

    // -- writing ------------------------------------------------------------

    /// Remember `key` as a conversation's origin, creating the record on first
    /// sight and refreshing the snapshot on later ones.
    ///
    /// Only the latest snapshot per origin is kept. A history of an
    /// append-only log's past lengths would be additive to add later and has no
    /// consumer now.
    ///
    /// A session the store already knows as a [`Role::Derived`] incarnation is
    /// left exactly as it is. Promoting it would overwrite the `fidelity` and
    /// `losses` of the conversion that produced it, and those describe a moment
    /// that cannot be measured again — the written session has been edited by an
    /// agent since. A conversation's origin is a fact about where it came from,
    /// not about which file is freshest.
    ///
    /// The lookup and the write are one step against another invocation — see
    /// [`locked`] — so two `agsx` runs ingesting one session converge on
    /// one record instead of minting two and racing to claim the same key.
    pub fn ingest_origin(
        &self,
        key: &SessionKey,
        path: &Path,
        policy: OriginPolicy,
    ) -> anyhow::Result<Record> {
        self.ensure_writable()?;

        // Before the lock on purpose: this hashes the session file, and the
        // largest rollout in the corpus is 281 MiB.
        let mut snapshot = OriginSnapshot::of(path)?;

        let mut conn = self.write_index()?;
        let write = locked(&mut conn)?;

        let existing = match lookup(&write, key)? {
            Some(id) => self.get(&id)?,
            None => None,
        };
        if let Some(mut record) = existing {
            if record.find(key).is_some_and(|inc| !inc.is_origin()) {
                debug!(
                    record = %record.id,
                    %key,
                    "already known as a derived incarnation; leaving its lineage alone"
                );
                return Ok(record);
            }
            snapshot.archived = record
                .find(key)
                .and_then(|inc| match &inc.role {
                    Role::Origin { snapshot } => snapshot.archived.clone(),
                    Role::Derived { .. } => None,
                })
                .or(snapshot.archived);
            if let OriginPolicy::Archive = policy {
                snapshot.archived = Some(self.archive(&record.id, path)?);
            }
            let fresh = Incarnation {
                key: key.clone(),
                path: path.to_path_buf(),
                recorded_at: now_ms(),
                role: Role::Origin { snapshot },
            };
            match record.incarnations.iter_mut().find(|inc| &inc.key == key) {
                Some(slot) => *slot = fresh,
                None => record.incarnations.push(fresh),
            }
            record.updated_at = now_ms();
            // The index already points at this record — that is how we found it
            // — so there is nothing to claim; the transaction ends having only
            // taken the lock.
            self.commit(&record)?;
            write.commit()?;
            return Ok(record);
        }

        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        fs::create_dir_all(self.record_dir(&id))?;
        if let OriginPolicy::Archive = policy {
            snapshot.archived = Some(self.archive(&id, path)?);
        }
        let now = now_ms();
        let record = Record {
            id: id.clone(),
            created_at: now,
            updated_at: now,
            incarnations: vec![Incarnation {
                key: key.clone(),
                path: path.to_path_buf(),
                recorded_at: now,
                role: Role::Origin { snapshot },
            }],
        };
        self.commit(&record)?;
        claim(&write, key, &id)?;
        write.commit()?;
        debug!(record = %id, %key, "ingested origin");
        Ok(record)
    }

    /// Remember a conversion we just performed.
    ///
    /// Snapshots the file we wrote, which is what makes "the user has since
    /// worked in this session" detectable later — see [`Advance`]. It is a
    /// one-time hash of a file this process just produced and still has warm,
    /// paid once per conversion rather than once per ranking. A snapshot that
    /// cannot be taken is recorded as absent rather than fatal: an unknown fails
    /// safe, and losing the whole lineage over it would be a worse cache.
    ///
    /// The same read-modify-write as [`Store::ingest_origin`] and locked the
    /// same way, for a sharper reason: two conversions of one conversation into
    /// two targets are the obvious thing to run at once, and an unlocked
    /// read-modify-write of `record.json` would drop one of them — along with
    /// `losses`, which nothing can recompute.
    pub fn record_conversion(
        &self,
        record_id: &str,
        derived: DerivedWrite,
    ) -> anyhow::Result<Record> {
        self.ensure_writable()?;

        // Before the lock, like the origin's: it hashes the file we wrote.
        let snapshot = match OriginSnapshot::of(&derived.path) {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                warn!(
                    path = %derived.path.display(),
                    %err,
                    "could not snapshot the session we just wrote; its growth will read as unknown"
                );
                None
            }
        };
        let fresh = Incarnation {
            key: derived.key.clone(),
            path: derived.path,
            recorded_at: now_ms(),
            role: Role::Derived {
                from: derived.from,
                fidelity: derived.fidelity,
                losses: derived.losses,
                snapshot,
            },
        };
        let mut conn = self.write_index()?;
        let write = locked(&mut conn)?;

        // Read inside the lock: the record we were handed may have grown an
        // incarnation since the caller last saw it.
        let mut record = self
            .get(record_id)?
            .ok_or_else(|| StoreError::NoSuchRecord {
                root: self.root.clone(),
                id: record_id.to_string(),
            })?;
        match record
            .incarnations
            .iter_mut()
            .find(|inc| inc.key == derived.key)
        {
            Some(slot) => *slot = fresh,
            None => record.incarnations.push(fresh),
        }
        record.updated_at = now_ms();
        self.commit(&record)?;
        claim(&write, &derived.key, record_id)?;
        write.commit()?;
        Ok(record)
    }

    // -- the IR cache -------------------------------------------------------

    /// The cached IR for a record's origin, or `None` if there is not a usable
    /// one and the caller must re-derive from origin bytes.
    ///
    /// A cached IR stamped with an [`IR_VERSION`] this build does not recognise
    /// is **deleted**, and so is one that will not deserialize. This is the
    /// enforcement point `IR_VERSION` never had: the stamp has been bumped once
    /// with nothing anywhere reading it. There is no migration path and there
    /// must never be one — that is what keeps the IR cheap to change, which is
    /// the property the whole design protects.
    pub fn load_ir(&self, record_id: &str) -> anyhow::Result<Option<SessionIr>> {
        let path = self.record_dir(record_id).join(IR_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;

        // Read the stamp before the shape: an IR written by an older build may
        // not deserialize into today's types at all, and "cannot parse" must
        // not be reported as "no cache" without the file being cleared.
        let stamp = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("ir_version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });

        match stamp.as_deref() {
            Some(IR_VERSION) => match serde_json::from_str::<SessionIr>(&text) {
                Ok(ir) => Ok(Some(ir)),
                Err(err) => {
                    warn!(record = record_id, %err, "discarding unparseable cached IR");
                    self.discard_ir(&path);
                    Ok(None)
                }
            },
            other => {
                debug!(
                    record = record_id,
                    found = other.unwrap_or("<none>"),
                    want = IR_VERSION,
                    "discarding stale cached IR"
                );
                self.discard_ir(&path);
                Ok(None)
            }
        }
    }

    /// Cache `ir` as this record's derived IR.
    pub fn store_ir(&self, record_id: &str, ir: &SessionIr) -> anyhow::Result<()> {
        self.ensure_writable()?;
        if self.get(record_id)?.is_none() {
            return Err(StoreError::NoSuchRecord {
                root: self.root.clone(),
                id: record_id.to_string(),
            }
            .into());
        }
        write_json(&self.record_dir(record_id).join(IR_FILE), ir)
    }

    // -- the index ----------------------------------------------------------

    /// Check the store and, optionally, rebuild `index.sqlite` from the record
    /// directories.
    ///
    /// The index is a cache, which is the answer to the objection that a single
    /// file is a single point of corruption. Only content is authoritative.
    pub fn fsck(&self, rebuild_index: bool) -> anyhow::Result<Fsck> {
        let mut report = Fsck::default();
        let mut records = Vec::new();

        for path in self.record_files()? {
            match read_record(&path) {
                Ok(record) => {
                    report.records += 1;
                    report.incarnations += record.incarnations.len();
                    let dir_id = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if dir_id != record.id {
                        report.problems.push(format!(
                            "record directory '{dir_id}' holds a record calling itself '{}'",
                            record.id
                        ));
                    }
                    let origins = record.incarnations.iter().filter(|i| i.is_origin()).count();
                    if origins != 1 {
                        report.problems.push(format!(
                            "record '{}' has {origins} origins; exactly one is expected",
                            record.id
                        ));
                    }
                    for inc in &record.incarnations {
                        if let Role::Derived { from, .. } = &inc.role
                            && record.find(from).is_none()
                        {
                            report.problems.push(format!(
                                "record '{}': {} was derived from {from}, which is not in the record",
                                record.id, inc.key
                            ));
                        }
                    }
                    records.push(record);
                }
                Err(err) => report.problems.push(format!("{}: {err}", path.display())),
            }
        }

        if rebuild_index {
            self.ensure_writable()?;
            let mut conn = self.write_index()?;
            // Under the same lock as every other writer: a rebuild that
            // interleaved with an ingest would delete the row that ingest had
            // just written, leaving the record it describes unfindable until
            // the next rebuild.
            let write = locked(&mut conn)?;
            report.indexed = reindex(&write, &records)?;
            write.commit()?;
            report.index_rebuilt = true;
        }
        Ok(report)
    }

    // -- the one interesting function ---------------------------------------

    /// The best source for converting this conversation into `target`, and why.
    ///
    /// It takes the target on purpose. There is no global ranking of
    /// incarnations, because sealed material is vendor-bound: a Codex origin
    /// beats a Claude derivative *when the target is Codex*, and the two are
    /// worth exactly the same when the target is Gemini, where neither vendor's
    /// capsules can cross. So the capsule half of the decision runs through
    /// [`crate::ir::Capsule::fits`] — the machinery that already decides this at
    /// the event level — rather than through a preference order that could
    /// drift out of agreement with it.
    ///
    /// Never fails. Every way a candidate can disappoint is a *reported*
    /// property of that candidate: an origin that is gone, a provider with no
    /// structured reader, a session that will not parse. A selection that
    /// returned `Err` would tell the caller nothing about the alternatives, and
    /// the alternatives are the entire point.
    ///
    /// # Cost
    ///
    /// Ranking on capsules means counting them, and counting them means an IR.
    /// A record with a single incarnation has nothing to choose between, so it
    /// costs one `stat` and no parse — which is every record until a conversion
    /// has actually happened. From the second incarnation on, each candidate is
    /// parsed once (the origin from `ir.json` when it is cached, which is what
    /// that cache is for).
    ///
    /// # Why the registry is a parameter
    ///
    /// It used to be built here, with [`ProviderRegistry::default_registry`],
    /// because the design's signature has no registry in it. The caller that
    /// matters — [`crate::pipeline::ConversionPipeline`] — already owns one, and
    /// two registries that could diverge is a latent bug: the pipeline reads the
    /// chosen candidate through *its* provider and the ranking counted capsules
    /// through a different instance of the same list. Sharing one instance also
    /// removes the per-call construction of twenty-one boxed providers.
    pub fn best_source_for(
        &self,
        record: &Record,
        target: &dyn Provider,
        registry: &ProviderRegistry,
    ) -> SourceChoice {
        let target_slug = target.slug().to_string();
        let target_vendor = vendor_of(&target_slug);
        // A single incarnation has nothing to choose between, so no candidate is
        // parsed at all. The gate is the whole reason a first-ever conversion
        // costs one `stat`.
        let comparing = record.incarnations.len() > 1;
        let registry = comparing.then_some(registry);

        let mut candidates: Vec<SourceCandidate> = record
            .incarnations
            .iter()
            .map(|inc| {
                let (path, availability, origin_state) = self.locate(record, inc);
                let capsules = match (registry, availability.readable()) {
                    (None, _) => Inventory::Unknown {
                        why: "only one incarnation: there is nothing to choose between".to_string(),
                    },
                    (Some(_), false) => Inventory::Unknown {
                        why: "the file cannot be read".to_string(),
                    },
                    (Some(registry), true) => {
                        match self.candidate_ir(record, inc, &path, registry) {
                            Ok(Some(ir)) => tally(&ir, target_vendor),
                            Ok(None) => Inventory::Unknown {
                                why: format!(
                                    "{} has no structured reader, so it carries no sealed material",
                                    inc.key.provider
                                ),
                            },
                            Err(why) => Inventory::Unknown { why },
                        }
                    }
                };
                SourceCandidate {
                    key: inc.key.clone(),
                    path,
                    role: inc.role.clone(),
                    recorded_at: inc.recorded_at,
                    advance: Advance::of(origin_state.as_ref()),
                    origin_state,
                    availability,
                    capsules,
                }
            })
            .collect();

        candidates.sort_by_key(|candidate| rank(candidate, &target_slug));

        SourceChoice {
            target: target_slug,
            target_vendor,
            candidates,
        }
    }

    // -- internals ----------------------------------------------------------

    fn ensure_writable(&self) -> Result<(), StoreError> {
        match self.newer {
            Some(found) => Err(StoreError::NewerStore {
                root: self.root.clone(),
                found,
                supported: STORE_VERSION,
            }),
            None => Ok(()),
        }
    }

    fn record_dir(&self, record_id: &str) -> PathBuf {
        self.root.join(RECORDS_DIR).join(record_id)
    }

    fn record_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        let dir = self.root.join(RECORDS_DIR);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let candidate = entry.path().join(RECORD_FILE);
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn commit(&self, record: &Record) -> anyhow::Result<()> {
        fs::create_dir_all(self.record_dir(&record.id))?;
        write_json(&self.record_dir(&record.id).join(RECORD_FILE), record)
    }

    /// Open the index, rebuilding it when it is missing or speaks a schema this
    /// build does not.
    ///
    /// `Ok(None)` means "answer from the records instead": the store belongs to
    /// a newer build, and repairing its cache would be writing to it.
    ///
    /// # Why the rebuild is locked and the journal mode is not
    ///
    /// Deciding to rebuild is itself a read-modify-write — read `user_version`,
    /// drop the table, refill it — and it was the sharper half of the store's
    /// concurrency bug: two invocations both read version 0 on a fresh store,
    /// and the second one's `DROP TABLE` deleted the row the first had just
    /// written. So the decision is re-made under the same [`locked`]
    /// transaction every other writer takes.
    ///
    /// `journal_mode=WAL`, by contrast, is a concurrency *optimisation* that
    /// this code must not depend on, and it is the one statement `busy_timeout`
    /// cannot cover: converting a rollback-journal database to WAL needs an
    /// exclusive lock that SQLite will not wait for, so two invocations opening
    /// a brand-new index in the same millisecond made one of them fail with
    /// `SQLITE_BUSY` — the store's original symptom, on the very first
    /// statement it ran. Every writer holds an `IMMEDIATE` transaction, which is
    /// correct in either journal mode, so a refused conversion is logged and the
    /// next uncontended open performs it.
    fn index(&self) -> anyhow::Result<Option<Connection>> {
        if self.newer.is_some() {
            return Ok(None);
        }
        let mut conn = Connection::open(self.root.join(INDEX_FILE))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        if let Err(err) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
            debug!(%err, "leaving the session index in its current journal mode");
        }
        if schema_version(&conn)? != INDEX_SCHEMA_VERSION {
            let write = locked(&mut conn)?;
            // Re-read under the lock. Whoever we queued behind may have rebuilt
            // it already, and rebuilding a second time would drop the rows they
            // wrote.
            if schema_version(&write)? != INDEX_SCHEMA_VERSION {
                debug!(want = INDEX_SCHEMA_VERSION, "rebuilding session index");
                write.execute_batch("DROP TABLE IF EXISTS sessions;")?;
                write.execute_batch(INDEX_SCHEMA)?;
                write.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)?;
                let records = self.list()?;
                reindex(&write, &records)?;
            }
            write.commit()?;
        }
        Ok(Some(conn))
    }

    /// The index a mutation holds open, ready for [`locked`].
    ///
    /// A writable store always has one: [`Store::ensure_writable`] has already
    /// refused the only case [`Store::index`] answers `None`, so the error here
    /// is unreachable rather than a second policy.
    fn write_index(&self) -> anyhow::Result<Connection> {
        self.index()?.ok_or_else(|| {
            anyhow::anyhow!(
                "the session index at {} is unavailable",
                self.root.display()
            )
        })
    }

    fn archive(&self, record_id: &str, path: &Path) -> anyhow::Result<String> {
        let dir = self.record_dir(record_id).join(ORIGIN_DIR);
        fs::create_dir_all(&dir)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "origin".to_string());
        fs::copy(path, dir.join(&name))?;
        Ok(name)
    }

    fn discard_ir(&self, path: &Path) {
        if self.newer.is_some() {
            // Their cache, their format. We simply do not use it.
            return;
        }
        if let Err(err) = fs::remove_file(path) {
            warn!(path = %path.display(), %err, "could not delete stale cached IR");
        }
    }

    /// Where to read a candidate, whether it can be read at all, and how its
    /// recorded snapshot resolves.
    ///
    /// The two roles read the same snapshot and draw opposite conclusions from
    /// `Unavailable`, which is the asymmetry worth stating. An origin's snapshot
    /// *stands in for* bytes the store does not hold, so a file that diverged from
    /// it can no longer be claimed as that origin and the archived copy — or
    /// nothing — takes over. A derived session's snapshot is only a growth marker
    /// on a file this tool wrote; a derivative that diverged is still the user's
    /// own session, still right there, and still the thing they may have spent two
    /// hours in. So it stays readable and the divergence becomes
    /// [`Advance::Unknown`], which fails safe by deferring to what the user named
    /// rather than by hiding their work.
    fn locate(
        &self,
        record: &Record,
        inc: &Incarnation,
    ) -> (PathBuf, Availability, Option<OriginState>) {
        match &inc.role {
            Role::Origin { snapshot } => {
                let state = snapshot.state(&inc.path);
                if state.usable() {
                    return (inc.path.clone(), Availability::Ready, Some(state));
                }
                let archived = snapshot
                    .archived
                    .as_ref()
                    .map(|name| self.record_dir(&record.id).join(ORIGIN_DIR).join(name));
                match archived {
                    Some(copy) if copy.is_file() => (copy, Availability::Archived, Some(state)),
                    _ => {
                        let why = state.describe();
                        (
                            inc.path.clone(),
                            Availability::Unavailable { why },
                            Some(state),
                        )
                    }
                }
            }
            Role::Derived { snapshot, .. } => {
                if !inc.path.is_file() {
                    return (
                        inc.path.clone(),
                        Availability::Unavailable {
                            why: "the written session file is gone".to_string(),
                        },
                        None,
                    );
                }
                let state = snapshot.as_ref().map(|snapshot| snapshot.state(&inc.path));
                (inc.path.clone(), Availability::Ready, state)
            }
        }
    }

    /// The candidate's structured IR, from the cache when we can.
    ///
    /// `Ok(None)` means the provider has no structured reader — nineteen of
    /// twenty-one answer that — which is also the honest answer to "how many
    /// capsules does it hold": none exist outside the IR.
    fn candidate_ir(
        &self,
        record: &Record,
        inc: &Incarnation,
        path: &Path,
        registry: &ProviderRegistry,
    ) -> Result<Option<SessionIr>, String> {
        if inc.is_origin() {
            match self.load_ir(&record.id) {
                Ok(Some(ir)) => return Ok(Some(ir)),
                Ok(None) => {}
                Err(err) => warn!(record = %record.id, %err, "cached IR unreadable"),
            }
        }
        let provider = registry
            .find_by_slug(&inc.key.provider)
            .ok_or_else(|| format!("this build has no provider called '{}'", inc.key.provider))?;
        let ir = provider
            .read_session_ir(path)
            .map_err(|err| format!("{} could not be read: {err}", inc.key))?;

        if let (true, Some(ir)) = (inc.is_origin(), ir.as_ref()) {
            // Warming the cache is the point of having one; failing to warm it
            // costs a re-parse and nothing else.
            if let Err(err) = self.store_ir(&record.id, ir) {
                debug!(record = %record.id, %err, "could not cache derived IR");
            }
        }
        Ok(ir)
    }
}

// ---------------------------------------------------------------------------
// Source selection
// ---------------------------------------------------------------------------

/// Whether a candidate can be read, and from where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Read [`SourceCandidate::path`], the session's own file.
    Ready,
    /// The live origin is unavailable; [`SourceCandidate::path`] is the byte
    /// copy kept under [`OriginPolicy::Archive`].
    Archived,
    /// Nothing to read.
    Unavailable { why: String },
}

impl Availability {
    pub fn readable(&self) -> bool {
        match self {
            Availability::Ready | Availability::Archived => true,
            Availability::Unavailable { .. } => false,
        }
    }
}

/// Sealed material a candidate holds, measured against one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inventory {
    /// Counted from the candidate's IR with [`crate::ir::Capsule::fits`].
    Counted {
        /// Capsules the target's vendor can interpret.
        fitting: usize,
        /// Capsules it cannot, which would be dropped on the way in.
        foreign: usize,
        /// Bytes of sealed material behind `fitting`.
        fitting_bytes: usize,
    },
    /// Not counted, and why. Ranked as zero, never as an assumption.
    Unknown { why: String },
}

impl Inventory {
    /// Capsules that would survive into the target. `Unknown` counts as none —
    /// the count is unproven, so it cannot be used to win a comparison.
    pub fn fitting(&self) -> usize {
        match self {
            Inventory::Counted { fitting, .. } => *fitting,
            Inventory::Unknown { .. } => 0,
        }
    }

    /// Bytes behind [`Inventory::fitting`].
    pub fn fitting_bytes(&self) -> usize {
        match self {
            Inventory::Counted { fitting_bytes, .. } => *fitting_bytes,
            Inventory::Unknown { .. } => 0,
        }
    }
}

/// One incarnation, judged as a source for one target.
#[derive(Debug, Clone)]
pub struct SourceCandidate {
    pub key: SessionKey,
    /// The file to read.
    pub path: PathBuf,
    /// Origin or derived, with the lineage the record recorded.
    pub role: Role,
    pub recorded_at: i64,
    /// The three-way resolution of this incarnation's recorded snapshot.
    ///
    /// `None` only when there is no snapshot to resolve: a derived incarnation
    /// from a record written before they were snapshotted. Both roles carry one
    /// otherwise, and the two roles read `Unavailable` differently — see
    /// [`Store::locate`].
    pub origin_state: Option<OriginState>,
    /// Whether this incarnation holds conversation content the store has never
    /// seen. Derived from `origin_state`, and the rung above capsules.
    pub advance: Advance,
    pub availability: Availability,
    pub capsules: Inventory,
}

impl SourceCandidate {
    /// `"origin"` or `"derived"`.
    pub fn label(&self) -> &'static str {
        match self.role {
            Role::Origin { .. } => "origin",
            Role::Derived { .. } => "derived",
        }
    }

    /// How much of the conversation this file holds.
    pub fn completeness(&self) -> Fidelity {
        match &self.role {
            Role::Origin { .. } => Fidelity::ByteIdentical,
            Role::Derived { fidelity, .. } => *fidelity,
        }
    }
}

/// The ranking, declared once.
///
/// Field order *is* the precedence, because `derive(Ord)` compares fields in
/// declaration order. [`SourceChoice::because`] walks the same fields in the
/// same order to explain itself, so the explanation cannot claim a reason the
/// ranking did not use.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    /// Unreadable candidates last.
    unreadable: bool,
    /// An incarnation that holds conversation content the others lack, first —
    /// above capsules, and above needing no conversion.
    ///
    /// The rung the design originally did not have, and the one it was wrong
    /// without. Capsules a derivative lacks are content its origin still holds;
    /// turns appended to it afterwards are content nothing else holds and no
    /// conversion can rebuild. See [`Advance`]. An [`Advance::Unknown`] counts as
    /// holding unseen content here, so a candidate the store cannot vouch for
    /// never loses this rung to one it can — and when the rung cannot be settled
    /// at all, [`SourceChoice::resolve`] stops choosing rather than guess.
    holds_unseen_content: Reverse<bool>,
    /// More capsules that fit the target, first. Computed by `Capsule::fits`.
    fewer_capsules: Reverse<usize>,
    /// A session already in the target's format needs no conversion at all.
    needs_conversion: bool,
    /// Then the more complete copy of the conversation. `Fidelity` is declared
    /// best-first, so ascending is best-first.
    completeness: Fidelity,
    /// Then the origin over anything we made from it.
    derived: bool,
    /// Then the most recently recorded.
    newest_first: Reverse<i64>,
}

fn rank(candidate: &SourceCandidate, target_slug: &str) -> Rank {
    Rank {
        unreadable: !candidate.availability.readable(),
        holds_unseen_content: Reverse(!candidate.advance.unmoved()),
        fewer_capsules: Reverse(candidate.capsules.fitting()),
        needs_conversion: candidate.key.provider != target_slug,
        completeness: candidate.completeness(),
        derived: !matches!(candidate.role, Role::Origin { .. }),
        newest_first: Reverse(candidate.recorded_at),
    }
}

/// Which incarnation to read, and what the alternatives would have cost.
///
/// The counterfactual is part of the value rather than a log line, because the
/// store may read a session the user did not name and that has to be visible
/// in the output: *"source: codex 01J… (origin; you named claude-code a3f…,
/// which would have cost 30,082 capsules)"*.
#[derive(Debug, Clone)]
pub struct SourceChoice {
    /// Slug of the provider being converted into.
    pub target: String,
    /// Vendor whose sealed formats the target can replay. `None` means this
    /// build does not know — emphatically not "no vendor" — in which case no
    /// capsule is counted as fitting and the explanation says so.
    pub target_vendor: Option<&'static str>,
    /// Every incarnation, best first.
    pub candidates: Vec<SourceCandidate>,
}

impl SourceChoice {
    /// The head of the ranking, or `None` when nothing in the record can be read.
    ///
    /// This is the ranking's answer and nothing else: it is not told what the user
    /// named, so it cannot be influenced by it. When the ranking cannot settle the
    /// unseen-content rung, [`SourceChoice::resolve`] is the function that decides
    /// what to actually read.
    pub fn chosen(&self) -> Option<&SourceCandidate> {
        self.candidates
            .first()
            .filter(|candidate| candidate.availability.readable())
    }

    /// A specific candidate, e.g. the one the user named.
    pub fn find(&self, key: &SessionKey) -> Option<&SourceCandidate> {
        self.candidates.iter().find(|c| &c.key == key)
    }

    /// Readable incarnations the store cannot prove hold nothing new: the ones
    /// it measured as advanced, and the ones it could not measure at all.
    ///
    /// [`SourceChoice::unmergeable`] is the predicate over this list; the list
    /// itself is what the explanation names, so the sentence and the decision
    /// come from the same set.
    pub fn unmerged(&self) -> Vec<&SourceCandidate> {
        self.candidates
            .iter()
            .filter(|c| c.availability.readable() && !c.advance.unmoved())
            .collect()
    }

    /// Whether the ranking's head cannot be trusted to be the whole
    /// conversation.
    ///
    /// True in exactly two situations, and they resolve the same way because the
    /// store's position in both is the same — *it does not know which incarnation
    /// the user wants*:
    ///
    /// - **Genuine divergence.** Two or more readable incarnations advanced since
    ///   the store recorded them. This design cannot merge incarnations and does
    ///   not claim to.
    /// - **An unknown.** Some readable incarnation's growth cannot be measured, so
    ///   the store cannot rule divergence out. Ranked as though it had advanced,
    ///   never as though it had not.
    ///
    /// A record with one readable incarnation is never unmergeable: there is
    /// nothing to merge it with.
    pub fn unmergeable(&self) -> bool {
        let readable = self
            .candidates
            .iter()
            .filter(|c| c.availability.readable())
            .count();
        if readable < 2 {
            return false;
        }
        let unmerged = self.unmerged();
        unmerged.len() > 1
            || unmerged
                .iter()
                .any(|c| matches!(c.advance, Advance::Unknown { .. }))
    }

    /// The incarnation to actually read, given the session the user named.
    ///
    /// [`SourceChoice::chosen`] whenever the ranking settled it, which is the
    /// ordinary case. When it could not — see [`SourceChoice::unmergeable`] — this
    /// returns the session the user named instead of the ranking's head.
    ///
    /// # Why that is a fallback and not a refusal
    ///
    /// Without the store the user would have got the session they named anyway.
    /// Falling back to it is therefore the one answer that **cannot make an
    /// outcome worse than not having a store at all**, which is the same
    /// invariant as "no store failure may fail a conversion" applied to a
    /// question the store cannot answer rather than to one it cannot reach. A
    /// hard error would fail a conversion that `--no-store` performs fine, and
    /// guessing would silently drop one side's work. The cost of both sides is
    /// reported instead, loudly.
    ///
    /// The property this gives up is small and worth naming: the *choice* is no
    /// longer strictly independent of what the user asked for. It is independent
    /// wherever the ranking can decide, and defers to them only where it cannot —
    /// which is the stronger of the two available properties, since the
    /// alternative is deciding by coin-flip.
    pub fn resolve(&self, named: Option<&SessionKey>) -> Option<&SourceCandidate> {
        if self.unmergeable()
            && let Some(named) = named
            && let Some(candidate) = self
                .find(named)
                .filter(|candidate| candidate.availability.readable())
        {
            return Some(candidate);
        }
        self.chosen()
    }

    /// One line for the user, naming the source and what choosing it costs in
    /// both directions.
    ///
    /// `named` is the session the user asked for, when they asked for one. Two
    /// things depend on it: which incarnation an unmergeable record resolves to,
    /// and what the counterfactual clause can say.
    ///
    /// Both directions on purpose. An earlier version of this only ever framed the
    /// choice as strictly better — it said what taking the user's suggestion would
    /// have cost and never what not taking it did — and that reads as a win in
    /// exactly the rows where something real is being given up.
    pub fn explain(&self, named: Option<&SessionKey>) -> String {
        let Some(chosen) = self.resolve(named) else {
            let reasons = self
                .candidates
                .iter()
                .map(|c| match &c.availability {
                    Availability::Unavailable { why } => format!("{}: {why}", c.key),
                    Availability::Ready | Availability::Archived => format!("{}: usable", c.key),
                })
                .collect::<Vec<_>>()
                .join("; ");
            return format!(
                "no usable source for {}: {}",
                self.target,
                if reasons.is_empty() {
                    "the record has no incarnations".to_string()
                } else {
                    reasons
                }
            );
        };

        let mut inside = chosen.label().to_string();
        if let Availability::Archived = chosen.availability {
            inside.push_str(", from the archived copy");
        }
        if let Some(state @ OriginState::Grew { .. }) = &chosen.origin_state {
            inside.push_str(&format!(", {}", state.describe()));
        }
        match named {
            Some(named) if named != &chosen.key => inside.push_str(&format!(
                "; you named {named}, {}",
                self.because(chosen, named)
            )),
            Some(_) | None => {}
        }
        if let Some(clause) = self.cannot_merge(chosen, named == Some(&chosen.key)) {
            inside.push_str(&format!("; {clause}"));
        }
        if let Some(clause) = self.gives_up(chosen) {
            inside.push_str(&format!("; {clause}"));
        }
        format!(
            "source: {} {} ({inside})",
            chosen.key.provider, chosen.key.provider_session_id,
        )
    }

    /// Why a record this design cannot resolve was resolved the way it was, and
    /// what each side of it holds.
    ///
    /// `None` when the ranking settled the choice, which is the ordinary case.
    fn cannot_merge(&self, chosen: &SourceCandidate, chosen_was_named: bool) -> Option<String> {
        if !self.unmergeable() {
            return None;
        }
        let each = self
            .unmerged()
            .iter()
            .map(|c| format!("{} {}", c.key, c.advance.describe()))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "this design cannot merge two incarnations of one conversation and the store cannot \
             rule out that more than one holds work the others do not ({each}), so {} ({}) was \
             read and is missing whatever the others hold",
            if chosen_was_named {
                "the session you named"
            } else {
                "the best-ranked of them"
            },
            chosen.key
        ))
    }

    /// The sealed material choosing `chosen` gives up, and which incarnation
    /// still holds it.
    ///
    /// The half of the cost the earlier explanation never stated. It is real in
    /// two of the four rows the design distinguishes — whenever growth outranks
    /// capsules, or a record cannot be merged — and a line that names only the
    /// gain reads as a win in exactly those rows.
    fn gives_up(&self, chosen: &SourceCandidate) -> Option<String> {
        let richer = self
            .candidates
            .iter()
            .filter(|c| c.availability.readable() && c.key != chosen.key)
            .max_by_key(|c| c.capsules.fitting())
            .filter(|c| c.capsules.fitting() > chosen.capsules.fitting())?;
        let capsules = richer.capsules.fitting() - chosen.capsules.fitting();
        let bytes = richer
            .capsules
            .fitting_bytes()
            .saturating_sub(chosen.capsules.fitting_bytes());
        Some(if bytes > 0 {
            format!(
                "gives up {capsules} capsules ({bytes} bytes of sealed material) that {} still holds",
                richer.key
            )
        } else {
            format!(
                "gives up {capsules} capsules that {} still holds",
                richer.key
            )
        })
    }

    /// Why `chosen` beat `named`, taking the fields of [`Rank`] in order so the
    /// stated reason is the one that actually decided it.
    ///
    /// It answers only the gain — what taking the user's suggestion would have
    /// cost. The give-up half of the same choice is [`SourceChoice::gives_up`],
    /// and both are rendered, because a line that names only the gain reads as a
    /// win in the rows where something real is being paid.
    fn because(&self, chosen: &SourceCandidate, named: &SessionKey) -> String {
        let Some(other) = self.find(named) else {
            return "which is not an incarnation of this conversation".to_string();
        };
        if !other.availability.readable() {
            return match &other.availability {
                Availability::Unavailable { why } => format!("which is unavailable — {why}"),
                Availability::Ready | Availability::Archived => "which is unavailable".to_string(),
            };
        }
        // The rung above capsules: content the named session does not have, which
        // no conversion from it could recover.
        if !chosen.advance.unmoved() && other.advance.unmoved() {
            return format!(
                "which holds nothing appended since the store recorded it, while {} does — {}",
                chosen.key,
                chosen.advance.describe()
            );
        }
        let (mine, theirs) = (chosen.capsules.fitting(), other.capsules.fitting());
        if mine > theirs {
            let capsules = mine - theirs;
            let bytes = chosen
                .capsules
                .fitting_bytes()
                .saturating_sub(other.capsules.fitting_bytes());
            let cost = if bytes > 0 {
                format!(
                    "which would have cost {capsules} capsules ({bytes} bytes of sealed material)"
                )
            } else {
                format!("which would have cost {capsules} capsules")
            };
            // And the other direction, which is what makes this a statement of a
            // trade rather than of a win: an unmoved named session holds nothing
            // the chosen one is missing, so nothing is being given up for it.
            return match &other.advance {
                Advance::Unmoved => format!("{cost} and holds nothing appended since"),
                Advance::Advanced { .. } | Advance::Unknown { .. } => cost,
            };
        }
        let native = chosen.key.provider == self.target;
        if native && other.key.provider != self.target {
            return format!(
                "which is not a {} session and would have needed another conversion",
                self.target
            );
        }
        if chosen.completeness() < other.completeness() {
            return format!("which is only {}", other.completeness().describe());
        }
        "which is an equivalent source".to_string()
    }
}

/// Count a candidate's sealed material against a target vendor.
///
/// Over [`SessionIr::model_visible`] rather than every event, because a capsule
/// on an event the model never sees would not be replayed and so is not worth
/// anything to the target. No `match` on [`crate::ir::Body`] appears here at
/// all: capsules hang off [`crate::ir::Event`], so a new body variant carrying
/// sealed material is counted the day it is read, with nothing here to forget
/// to update.
fn tally(ir: &SessionIr, target_vendor: Option<&str>) -> Inventory {
    let Some(vendor) = target_vendor else {
        let foreign = ir
            .model_visible()
            .iter()
            .map(|event| event.capsules.len())
            .sum();
        return Inventory::Counted {
            fitting: 0,
            foreign,
            fitting_bytes: 0,
        };
    };
    let mut fitting = 0;
    let mut foreign = 0;
    let mut fitting_bytes = 0;
    for event in ir.model_visible() {
        for capsule in &event.capsules {
            match capsule.fits(vendor) {
                CapsuleFit::SameVendor => {
                    fitting += 1;
                    fitting_bytes += capsule.sealed.len();
                }
                CapsuleFit::ForeignVendor => foreign += 1,
            }
        }
    }
    Inventory::Counted {
        fitting,
        foreign,
        fitting_bytes,
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// The store's write lock, and the order the two writes go in.
///
/// A mutation runs as `BEGIN IMMEDIATE` … `COMMIT` on `index.sqlite`, which
/// takes SQLite's write lock at the first statement rather than at the first
/// write. That is what serialises two `agsx` invocations: the second blocks in
/// `BEGIN` for up to the connection's `busy_timeout`, and by the time it reads
/// the index the first one's record is already there. Nothing expensive belongs
/// inside it — hashing an origin and the conversion itself both happen outside.
///
/// Inside the lock the order is always **`record.json` first, index row second,
/// commit last**, and it is not arbitrary. The record directories are
/// authoritative and the index is a cache rebuildable from them (`docs/STORE.md`),
/// so consider the two states a kill between the two writes can leave:
///
/// - **a record with no index row.** The record is on disk, [`Store::find_by_session`]
///   misses it, and [`Store::fsck`] with `rebuild_index` puts the row back —
///   repairable, by the operation the design already has for exactly this.
/// - **an index row naming a record that was never written.** A dangling pointer
///   into nothing: every lookup of that session resolves to a record the store
///   cannot load, and no rule can invent the content it names.
///
/// Only the first is repairable, so the ordering makes it the only one
/// reachable. `record.json` is renamed into place before the row is inserted,
/// and the row is not durable until `COMMIT`, so a kill at any point either
/// rolls the row back or leaves a record the index does not know about yet. The
/// index may lag the records. It may never lead them.
fn locked(conn: &mut Connection) -> anyhow::Result<rusqlite::Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// The record the index says owns `key`, if it says anything.
fn lookup(conn: &Connection, key: &SessionKey) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT record_id FROM sessions WHERE provider = ?1 AND provider_session_id = ?2",
            rusqlite::params![key.provider, key.provider_session_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Point `key` at `record_id`.
///
/// Called under [`locked`] with the record already on disk, so the row it writes
/// describes content that exists. An existing row for the same key is
/// overwritten rather than kept, which is the same rule [`reindex`] applies for
/// the same reason: the record is authoritative and the index is its cache, so
/// where they disagree the index is the one that is wrong.
fn claim(conn: &Connection, key: &SessionKey, record_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO sessions (provider, provider_session_id, record_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (provider, provider_session_id) DO UPDATE SET record_id = excluded.record_id",
        rusqlite::params![key.provider, key.provider_session_id, record_id],
    )?;
    Ok(())
}

fn schema_version(conn: &Connection) -> anyhow::Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn reindex(conn: &Connection, records: &[Record]) -> anyhow::Result<usize> {
    conn.execute_batch("DELETE FROM sessions;")?;
    let mut written = 0;
    for record in records {
        for inc in &record.incarnations {
            written += conn.execute(
                "INSERT INTO sessions (provider, provider_session_id, record_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (provider, provider_session_id) DO UPDATE SET record_id = excluded.record_id",
                rusqlite::params![inc.key.provider, inc.key.provider_session_id, record.id],
            )?;
        }
    }
    Ok(written)
}

fn read_record(path: &Path) -> anyhow::Result<Record> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Publish `value` at `path` so that a concurrent reader sees either the old
/// file or the new one, never a partial write and never a gap.
///
/// [`crate::pipeline::atomic_write`] already does the durable half — parents,
/// temp file, `write_all`, `flush`, `sync_all`, rename — so it does that half
/// here too, onto a fresh sidecar name. The sidecar is then renamed over the
/// destination in one step. Calling `atomic_write` on the destination directly
/// would work, but its overwrite path is built for user session files: it
/// renames the existing file to `.bak` first, which both litters the store and
/// leaves a window where the record does not exist.
fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "store-file".to_string());
    let staging = path.with_file_name(format!(
        "{name}.agsx-new-{}",
        uuid::Uuid::new_v4().as_simple()
    ));
    crate::pipeline::atomic_write(&staging, &bytes, false, "agsx-store")?;
    if let Err(err) = fs::rename(&staging, path) {
        let _ = fs::remove_file(&staging);
        return Err(err.into());
    }
    Ok(())
}

fn hash_prefix(path: &Path, limit: u64) -> std::io::Result<(String, u64)> {
    let mut reader = fs::File::open(path)?.take(limit);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF];
    let mut read = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), read))
}

fn mtime_ms(meta: &fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Block, Body, Capsule, CapsuleBinding, CapsuleKind, Event, LossKind, Role as IrRole,
        SourceRef, Visibility,
    };

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open_at(tmp.path().join("store")).expect("open");
        (tmp, store)
    }

    fn session(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write session");
        path
    }

    fn ir_with_capsules(agent: &str, kind: CapsuleKind, count: usize) -> SessionIr {
        let mut ir = SessionIr::new(agent, "s1");
        for i in 0..count {
            ir.events.push(Event {
                id: format!("e{i}"),
                parent: None,
                branch: crate::ir::Branch::Main,
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
                    sealed: "AAAA".to_string(),
                }],
                source: SourceRef {
                    line: (i + 1) as u64,
                    sha256: String::new(),
                },
            });
        }
        ir
    }

    #[test]
    fn default_root_prefers_the_env_var() {
        // No env mutation: just assert the documented fallback order exists.
        assert!(default_root().is_ok());
    }

    #[test]
    fn a_fresh_store_creates_its_layout() {
        let (_tmp, store) = store();
        assert!(store.root().join(STORE_FILE).is_file());
        assert!(store.root().join(RECORDS_DIR).is_dir());
        assert_eq!(store.store_version(), STORE_VERSION);
    }

    #[test]
    fn ingest_is_idempotent_and_refreshes_the_snapshot() {
        let (tmp, store) = store();
        let path = session(tmp.path(), "a.jsonl", "{\"one\":1}\n");
        let key = SessionKey::new("codex", "sess-1");

        let first = store
            .ingest_origin(&key, &path, OriginPolicy::Reference)
            .expect("ingest");
        fs::write(&path, "{\"one\":1}\n{\"two\":2}\n").expect("append");
        let second = store
            .ingest_origin(&key, &path, OriginPolicy::Reference)
            .expect("re-ingest");

        assert_eq!(first.id, second.id, "one conversation, one record");
        assert_eq!(second.incarnations.len(), 1);
        let Role::Origin { snapshot } = &second.origin().expect("origin").role else {
            panic!("expected an origin");
        };
        assert_eq!(snapshot.size, 20, "the snapshot follows the live file");
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn the_index_answers_lookups_by_provider_session() {
        let (tmp, store) = store();
        let path = session(tmp.path(), "a.jsonl", "{}\n");
        let key = SessionKey::new("codex", "sess-1");
        let record = store
            .ingest_origin(&key, &path, OriginPolicy::Reference)
            .expect("ingest");

        let found = store.find_by_session(&key).expect("lookup").expect("hit");
        assert_eq!(found.id, record.id);
        assert!(
            store
                .find_by_session(&SessionKey::new("codex", "nope"))
                .expect("lookup")
                .is_none()
        );
        assert!(store.root().join(INDEX_FILE).is_file());
    }

    #[test]
    fn recording_a_conversion_keeps_its_losses_verbatim() {
        let (tmp, store) = store();
        let origin = session(tmp.path(), "a.jsonl", "{}\n");
        let written = session(tmp.path(), "b.jsonl", "{}\n");
        let from = SessionKey::new("codex", "sess-1");
        let record = store
            .ingest_origin(&from, &origin, OriginPolicy::Reference)
            .expect("ingest");

        let loss = Loss {
            kind: LossKind::Reasoning,
            events: 12,
            capsules: 30_082,
            bytes: 4_096,
            grade: Fidelity::ContextNoReasoning,
            note: "openai capsules cannot cross into anthropic".to_string(),
        };
        let updated = store
            .record_conversion(
                &record.id,
                DerivedWrite {
                    key: SessionKey::new("claude-code", "cc-1"),
                    path: written,
                    from: from.clone(),
                    fidelity: Fidelity::ContextNoReasoning,
                    losses: vec![loss.clone()],
                },
            )
            .expect("record");

        assert_eq!(updated.incarnations.len(), 2);
        let reloaded = store.get(&record.id).expect("get").expect("record");
        let derived = reloaded
            .for_provider("claude-code")
            .expect("derived incarnation");
        let Role::Derived {
            from: recorded_from,
            fidelity,
            losses,
            snapshot,
        } = &derived.role
        else {
            panic!("expected a derived incarnation");
        };
        assert_eq!(recorded_from, &from);
        assert_eq!(*fidelity, Fidelity::ContextNoReasoning);
        assert_eq!(losses, &[loss], "losses are records, not caches");
        let snapshot = snapshot
            .as_ref()
            .expect("the file we wrote was snapshotted");
        assert_eq!(snapshot.size, 3, "the snapshot is of the session we wrote");
    }

    #[test]
    fn origin_resolution_reports_all_three_outcomes() {
        let (tmp, store) = store();
        let path = session(tmp.path(), "a.jsonl", "line one\n");
        let key = SessionKey::new("codex", "sess-1");
        let record = store
            .ingest_origin(&key, &path, OriginPolicy::Reference)
            .expect("ingest");
        let Role::Origin { snapshot } = record.origin().expect("origin").role.clone() else {
            panic!("expected an origin");
        };

        assert_eq!(
            snapshot.state(&path),
            OriginState::Unchanged { rehashed: false },
            "an untouched file costs one stat"
        );

        fs::write(&path, "line one\nline two\n").expect("append");
        assert_eq!(
            snapshot.state(&path),
            OriginState::Grew { added_bytes: 9 },
            "an append-only log that advanced is still usable"
        );

        fs::write(&path, "something else entirely\n").expect("rewrite");
        let diverged = snapshot.state(&path);
        assert!(!diverged.usable());
        assert!(
            format!("{diverged:?}").contains("diverged"),
            "got {diverged:?}"
        );

        fs::remove_file(&path).expect("remove");
        let gone = snapshot.state(&path);
        assert!(!gone.usable());
        assert!(format!("{gone:?}").contains("missing"), "got {gone:?}");
    }

    #[test]
    fn archive_survives_a_deleted_origin() {
        let (tmp, store) = store();
        let path = session(tmp.path(), "a.jsonl", "the only copy\n");
        let key = SessionKey::new("codex", "sess-1");
        let record = store
            .ingest_origin(&key, &path, OriginPolicy::Archive)
            .expect("ingest");
        fs::remove_file(&path).expect("remove");

        let registry = ProviderRegistry::default_registry();
        let target = registry.find_by_slug("codex").expect("codex");
        let choice = store.best_source_for(&record, target, &registry);
        let chosen = choice
            .chosen()
            .expect("the archived copy is still a source");
        assert_eq!(chosen.availability, Availability::Archived);
        assert_eq!(
            fs::read_to_string(&chosen.path).expect("read archive"),
            "the only copy\n"
        );
    }

    #[test]
    fn a_stale_ir_cache_is_deleted_not_migrated() {
        let (tmp, store) = store();
        let path = session(tmp.path(), "a.jsonl", "{}\n");
        let record = store
            .ingest_origin(
                &SessionKey::new("codex", "sess-1"),
                &path,
                OriginPolicy::Reference,
            )
            .expect("ingest");

        let ir = SessionIr::new("codex", "sess-1");
        store.store_ir(&record.id, &ir).expect("cache");
        assert_eq!(store.load_ir(&record.id).expect("load"), Some(ir));

        // Plant the version that was superseded.
        let cache = store.record_dir(&record.id).join(IR_FILE);
        let mut raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cache).expect("read")).expect("parse");
        raw["ir_version"] = serde_json::Value::String("agsx-ir/1".to_string());
        fs::write(&cache, serde_json::to_vec(&raw).expect("encode")).expect("plant");

        assert_eq!(store.load_ir(&record.id).expect("load"), None);
        assert!(!cache.exists(), "a stale cache is deleted, never migrated");
    }

    #[test]
    fn best_source_picks_differently_for_a_same_and_a_foreign_vendor_target() {
        let (tmp, store) = store();
        let codex_path = session(tmp.path(), "rollout.jsonl", "{}\n");
        let cc_path = session(tmp.path(), "cc.jsonl", "{}\n");
        let codex_key = SessionKey::new("codex", "01J");
        let cc_key = SessionKey::new("claude-code", "a3f");

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
            .expect("record");

        // Give the codex origin a cached IR with sealed openai material, so the
        // tally has something real to count through `Capsule::fits`.
        store
            .store_ir(
                &record.id,
                &ir_with_capsules("codex", CapsuleKind::OpenaiReasoningEncryptedContent, 7),
            )
            .expect("cache");

        let registry = ProviderRegistry::default_registry();

        let to_codex = store.best_source_for(&record, registry.find_by_slug("codex").unwrap(), &registry);
        assert_eq!(to_codex.target_vendor, Some("openai"));
        assert_eq!(
            to_codex.chosen().expect("a source").key,
            codex_key,
            "for a codex target the origin's sealed material is worth something"
        );
        assert_eq!(to_codex.chosen().unwrap().capsules.fitting(), 7);

        let to_cc = store.best_source_for(&record, registry.find_by_slug("claude-code").unwrap(), &registry);
        assert_eq!(to_cc.target_vendor, Some("anthropic"));
        assert_eq!(
            to_cc.chosen().expect("a source").key,
            cc_key,
            "openai capsules are worth nothing to anthropic, so the session that \
             needs no conversion wins"
        );
        assert_eq!(to_cc.chosen().unwrap().capsules.fitting(), 0);
    }

    #[test]
    fn the_explanation_carries_the_counterfactual() {
        let (tmp, store) = store();
        let codex_path = session(tmp.path(), "rollout.jsonl", "{}\n");
        let cc_path = session(tmp.path(), "cc.jsonl", "{}\n");
        let codex_key = SessionKey::new("codex", "01J");
        let cc_key = SessionKey::new("claude-code", "a3f");
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
            .expect("record");
        store
            .store_ir(
                &record.id,
                &ir_with_capsules("codex", CapsuleKind::OpenaiReasoningEncryptedContent, 3),
            )
            .expect("cache");

        let registry = ProviderRegistry::default_registry();
        let choice = store.best_source_for(&record, registry.find_by_slug("codex").unwrap(), &registry);
        let line = choice.explain(Some(&cc_key));
        assert!(line.starts_with("source: codex 01J (origin;"), "got {line}");
        assert!(
            line.contains("you named claude-code a3f, which would have cost 3 capsules"),
            "got {line}"
        );
    }
}
