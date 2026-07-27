//! Gemini CLI provider — reads/writes sessions under `~/.gemini/tmp/`.
//!
//! Session files: `<hash>/chats/session-<timestamp>-<id8>.{json,jsonl}`
//! Resume command: `gemini --resume <session-id>`
//!
//! # Two formats, and neither one is the old one
//!
//! Gemini writes a session as **JSONL** — `ChatRecordingService.appendRecord`
//! is `JSON.stringify(record) + "\n"` through `appendFileSync`, one record per
//! line — and has done for many releases. Before that it wrote a single
//! whole-file JSON object. Both are still live, and a long-lived install holds
//! a mixture, because the CLI converts a `.json` in place *only when the
//! session is resumed*: `initialize()` appends an `l` to the filename, replays
//! the header and every message into the new `.jsonl`, and **leaves the
//! original `.json` on disk**. So:
//!
//! - Reading only `.json` shows exactly the sessions nobody has resumed
//!   lately, truncated at an arbitrary date. That was this reader until now: on
//!   a `chats/` directory holding one of each it found 1 of 3.
//! - Reading both without deduplicating shows a migrated session twice. The
//!   CLI resolves that pair by `sessionId`, newest `lastUpdated` first and
//!   `.jsonl` ahead of `.json` on a tie (`compareIndexedSessions`), and
//!   [`Gemini::list_sessions`] does the same.
//!
//! ## Legacy whole-file JSON
//!
//! ```json
//! {
//!   "sessionId": "…",
//!   "projectHash": "…",
//!   "startTime": "…",
//!   "lastUpdated": "…",
//!   "messages": [
//!     { "type": "user"|"gemini"|"model", "content": "…"|[…], "timestamp": "…" }
//!   ]
//! }
//! ```
//!
//! Note: Gemini may use `"gemini"` or `"model"` for assistant responses.
//!
//! ## JSONL — a fold, not a log
//!
//! The `.jsonl` is an append-only *edit log*. Concatenating its message lines
//! resurrects turns the user rewound: content that, from Gemini's point of
//! view, no longer exists. [`fold_jsonl`] replays it into the resolved state
//! using the same classification and the same effects as the CLI's own
//! `loadConversationRecord`, in the same order — the order is load-bearing,
//! since a header record also carries a string `sessionId` and a `$set` payload
//! could carry anything:
//!
//! | Record | Recognised by | Effect |
//! |---|---|---|
//! | rewind | string `$rewindTo` | drop that message id **and every message after it**; if the id is not present, drop *all* messages |
//! | message | string `id` | upsert by id — a repeat replaces in place and keeps its original position |
//! | metadata update | object `$set` | merge into metadata; an `$set.messages` array *replaces* the message list wholesale |
//! | header | string `sessionId` **and** string `projectHash` | merge into metadata; a `messages` array is appended, not replaced |
//!
//! Anything else is unrecognised. Gemini drops those silently; this reader
//! counts them, records their keys under `metadata.unrecognized_records`, and
//! warns — a record type nobody handles has to be visible, because the next
//! format change will arrive as exactly that.
//!
//! This is not [`crate::replay`]. That module folds a [`crate::ir::SessionIr`]
//! — typed `Body` events with ids, turns and parent links — and Gemini has no
//! structured reader to produce one. It is also a different layer: `replay`
//! resolves an already-parsed capture, whereas `$rewindTo` and `$set.messages`
//! decide which messages the file even *contains*. Sharing the mechanism would
//! mean building a Gemini IR reader first, which is a much larger change than
//! reading the format; the two folds stay separate and this one stays private
//! to the parser.
//!
//! ## Subagent transcripts
//!
//! A subagent writes to `chats/<parent-session-id>/<session-id>.jsonl` — one
//! level deeper and without the `session-` prefix. Gemini's own
//! `isSupportedSessionFile` does not match those, so they are not resumable
//! sessions and are not listed here either. [`Gemini::read_session`] will still
//! read one if pointed straight at it, because format handling is by content.

use std::collections::{BTreeSet, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, WriteOptions, WrittenSession, store_evidence, walk_entry_reporting,
};

/// Gemini CLI provider implementation.
pub struct Gemini;

/// Compute the Gemini project hash directory name from a workspace path.
///
/// Algorithm: `SHA256(absolute_workspace_path)` as lowercase hex.
///
/// Example: `/data/projects/foo` → `sha256(b"/data/projects/foo")` (64 hex chars)
pub fn project_hash(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a Gemini session filename from a session ID and timestamp.
///
/// Convention: `session-YYYY-MM-DDThh-mm-<uuid-prefix>.json`
/// where `<uuid-prefix>` is the first 8 chars of the session UUID.
///
/// The extension is deliberately `.json`, the legacy whole-file form, even
/// though current Gemini writes `.jsonl`. See [`Gemini::write_session`].
pub fn session_filename(session_id: &str, now: &chrono::DateTime<chrono::Utc>) -> String {
    let ts = now.format("%Y-%m-%dT%H-%M").to_string();
    let prefix: String = session_id.chars().take(8).collect();
    format!("session-{ts}-{prefix}.json")
}

/// Does this filename name a resumable Gemini session file?
///
/// Mirrors the CLI's `isSupportedSessionFile`: the `session-` prefix plus
/// either extension. Both halves matter — dropping `.jsonl` hides every
/// session written or migrated by a current Gemini, and dropping the prefix
/// picks up subagent transcripts the CLI itself will not resume.
pub fn is_session_file_name(name: &str) -> bool {
    name.starts_with("session-") && (name.ends_with(".json") || name.ends_with(".jsonl"))
}

/// The session id a filename encodes, if the file body cannot supply one.
///
/// `session-2026-01-10T02-06-8c1890a5.jsonl` → `2026-01-10T02-06-8c1890a5`.
fn session_id_from_name(name: &str) -> &str {
    name.strip_prefix("session-")
        .map(|rest| {
            rest.strip_suffix(".jsonl")
                .or_else(|| rest.strip_suffix(".json"))
                .unwrap_or(rest)
        })
        .unwrap_or(name)
}

/// Which of two files for the same `sessionId` is the live one.
///
/// Resuming a legacy `.json` writes a `.jsonl` beside it and leaves the
/// original in place, so a migrated session is on disk twice. Gemini's
/// `compareIndexedSessions` breaks that tie in favour of `.jsonl`; so does
/// this, and for the same reason — the `.json` stopped being appended to the
/// moment the `.jsonl` appeared.
fn supersedes(candidate: &Path, existing: &Path) -> bool {
    is_jsonl(candidate) && !is_jsonl(existing)
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

impl Gemini {
    /// Root directory for Gemini data, in precedence order:
    ///
    /// 1. `GEMINI_HOME` — casr's own override, naming the `.gemini` directory
    ///    itself. Gemini CLI has no variable with those semantics, so this one
    ///    is casr's alone; it wins so that aiming casr at a tree never disturbs
    ///    the Gemini CLI the rest of the shell talks to.
    /// 2. `GEMINI_CLI_HOME` — the variable Gemini CLI itself honours. It
    ///    replaces the *home directory*, not the `.gemini` directory, so
    ///    `.gemini` is joined onto it exactly as the CLI does:
    ///    `homedir()` returns `$GEMINI_CLI_HOME` when set, and
    ///    `getGlobalGeminiDir()` is `path.join(homedir(), '.gemini')`.
    /// 3. `~/.gemini`.
    ///
    /// An empty value counts as unset, matching the CLI's own truthiness check.
    pub fn home_dir() -> Option<PathBuf> {
        if let Some(home) = std::env::var_os("GEMINI_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(home));
        }
        if let Some(home) = std::env::var_os("GEMINI_CLI_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(home).join(".gemini"));
        }
        dirs::home_dir().map(|h| h.join(".gemini"))
    }

    /// Tmp directory where session hashes live.
    fn tmp_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join("tmp"))
    }
}

impl Provider for Gemini {
    fn name(&self) -> &str {
        "Gemini CLI"
    }

    fn slug(&self) -> &str {
        "gemini"
    }

    fn cli_alias(&self) -> &str {
        "gmi"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("gemini").is_ok() {
            evidence.push("gemini binary found in PATH".to_string());
            installed = true;
        }

        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            evidence.push(format!("{} exists", home.display()));
            installed = true;
        }

        // Chats live under `~/.gemini/tmp/<project-hash>/chats`, not in
        // `~/.gemini` itself, which is what detection above found.
        if installed && let Some(tmp) = Self::tmp_dir() {
            evidence.push(store_evidence(&tmp));
        }

        trace!(provider = "gemini", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let Some(tmp) = Self::tmp_dir() else {
            return vec![];
        };
        if !tmp.is_dir() {
            return vec![];
        }
        // Each hash directory under tmp/ that has a chats/ subdirectory is a root.
        std::fs::read_dir(&tmp)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let chats = entry.path().join("chats");
                chats.is_dir().then_some(chats)
            })
            .collect()
    }

    fn list_sessions(&self) -> Option<SessionListing> {
        let tmp = Self::tmp_dir()?;

        // Ordered, so the listing does not reshuffle between runs, but keyed by
        // session id so a migrated session appears once. `max_depth(3)` keeps
        // this to `tmp/<hash>/chats/<file>`; subagent transcripts live one
        // level below that and are not resumable sessions.
        let mut listing = SessionListing::default();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for entry in WalkDir::new(&tmp).max_depth(3) {
            let Some(entry) = walk_entry_reporting(entry, &mut listing.unreadable) else {
                continue;
            };
            let path = entry.path();
            if !self.is_session_path(path) || !path.is_file() {
                continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let session_id = session_id_from_file(path)
                .unwrap_or_else(|| session_id_from_name(name).to_string());

            match seen.get(&session_id) {
                Some(&index) => {
                    if supersedes(path, &listing.sessions[index].1) {
                        debug!(
                            session_id,
                            superseded = %listing.sessions[index].1.display(),
                            live = %path.display(),
                            "migrated Gemini session: preferring the .jsonl"
                        );
                        listing.sessions[index].1 = path.to_path_buf();
                    }
                }
                None => {
                    seen.insert(session_id.clone(), listing.sessions.len());
                    listing.sessions.push((session_id, path.to_path_buf()));
                }
            }
        }

        Some(listing)
    }

    /// A Gemini chat is `tmp/<project-hash>/chats/session-<ts>-<id8>.{json,jsonl}`.
    /// The `chats/` parent is part of the rule: `tmp/<hash>/` also holds
    /// `shell_history`, and a subagent transcript one level below `chats/` is
    /// not a resumable session.
    fn is_session_path(&self, path: &Path) -> bool {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("chats")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_session_file_name)
    }

    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let tmp = Self::tmp_dir()?;
        if !tmp.is_dir() {
            return None;
        }

        // Gemini sessions are at <hash>/chats/session-*.{json,jsonl}.
        //
        // Real filename convention: session-YYYY-MM-DDThh-mm-<uuid_prefix8>.ext
        // so we cannot rely on exact filename == session_id.
        //
        // A migrated session matches twice; the scan runs to completion and
        // keeps the `.jsonl`, because returning the first hit would resume from
        // whichever half the directory happened to hand back first.
        let exact_names = [
            format!("session-{session_id}.json"),
            format!("session-{session_id}.jsonl"),
        ];
        let id_prefix = session_id
            .chars()
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let prefix_suffixes = [format!("-{id_prefix}.json"), format!("-{id_prefix}.jsonl")];

        let mut found: Option<PathBuf> = None;
        for entry in WalkDir::new(&tmp)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            // Files must be in a chats/ directory.
            if let Some(parent) = path.parent()
                && parent.file_name().and_then(|n| n.to_str()) == Some("chats")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                // Legacy-style exact filename.
                let hit = if exact_names.iter().any(|exact| name == exact) {
                    debug!(path = %path.display(), "found Gemini session by exact filename");
                    true
                } else if !id_prefix.is_empty() {
                    // Prefix-based lookup for modern filenames.
                    let name_lc = name.to_ascii_lowercase();
                    let matched = prefix_suffixes
                        .iter()
                        .any(|suffix| name_lc.ends_with(suffix.as_str()))
                        && session_id_from_file(path).as_deref() == Some(session_id);
                    if matched {
                        debug!(path = %path.display(), "found Gemini session by UUID prefix + sessionId body match");
                    }
                    matched
                } else {
                    false
                };

                if hit && found.as_deref().is_none_or(|old| supersedes(path, old)) {
                    found = Some(path.to_path_buf());
                }
            }
        }
        found
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Gemini session");

        let Parsed {
            metadata: root,
            messages: msg_array,
            drift,
        } = parse_session_file(path)?;

        // Session-level fields.
        let session_id = root
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                // Derive from filename: session-<uuid>.{json,jsonl} → <uuid>
                path.file_name()
                    .and_then(|s| s.to_str())
                    .filter(|name| name.starts_with("session-"))
                    .map(session_id_from_name)
                    .unwrap_or("unknown")
                    .to_string()
            });

        let project_hash = root
            .get("projectHash")
            .and_then(|v| v.as_str())
            .map(String::from);

        let started_at = root.get("startTime").and_then(parse_timestamp);
        let mut ended_at = root.get("lastUpdated").and_then(parse_timestamp);

        let mut messages: Vec<CanonicalMessage> = Vec::new();

        for (i, msg) in msg_array.iter().enumerate() {
            // Role: Gemini uses "type" field with "user" or "model".
            let role_str = msg
                .get("type")
                .or_else(|| msg.get("role"))
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let role = normalize_role(role_str);

            // Content: string or array of content parts.
            let content_val = msg.get("content");
            let text = gemini_extract_text_content(msg, content_val);
            let tool_calls = gemini_extract_tool_calls(msg, content_val);
            let tool_results = gemini_extract_tool_results(msg, content_val);

            if text.trim().is_empty() && tool_calls.is_empty() && tool_results.is_empty() {
                trace!(index = i, "skipping empty Gemini message");
                continue;
            }

            // Timestamp.
            let ts = msg.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content: text,
                timestamp: ts,
                author: None,
                tool_calls,
                tool_results,
                extra: msg.clone(),
            });
        }

        reindex_messages(&mut messages);

        // Title from first user message.
        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        // Workspace: try to extract from message content (project paths).
        let workspace = extract_workspace_from_messages(&messages);

        // Metadata.
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("gemini".to_string()),
        );
        if let Some(ref ph) = project_hash {
            metadata.insert("project_hash".into(), serde_json::Value::String(ph.clone()));
        }
        // Format drift, made visible. Gemini's own loader drops a record it
        // cannot classify without a word, which is how a reader goes blind to a
        // new record type for twenty releases. The key is absent when there was
        // nothing to report and when the file was legacy whole-file JSON, where
        // the question does not arise — it never reports a zero as a finding.
        if let Some(drift) = drift.filter(Drift::is_nonzero) {
            warn!(
                path = %path.display(),
                unclassified = drift.unclassified,
                unparseable_lines = drift.unparseable_lines,
                keys = ?drift.keys,
                "Gemini JSONL contained records this reader does not understand"
            );
            metadata.insert("unrecognized_records".into(), drift.to_json());
        }

        debug!(
            session_id,
            messages = messages.len(),
            "Gemini session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "gemini".to_string(),
            workspace,
            title,
            started_at,
            ended_at,
            messages,
            metadata: serde_json::Value::Object(metadata),
            source_path: path.to_path_buf(),
            model_name: None,
        })
    }

    /// Write the session as legacy whole-file JSON, `session-…-<id8>.json`.
    ///
    /// **This is deliberate, and current Gemini reads it.** `loadConversationRecord`
    /// tries the file as JSONL first, gets nothing (a pretty-printed object has
    /// no parseable lines), and falls through to `parseLegacyRecordFallback`,
    /// which is a whole-file `JSON.parse`; `isSupportedSessionFile` lists
    /// `.json` alongside `.jsonl`, and `deriveSessionShortId` strips either
    /// extension. Legacy JSON is therefore the *wider* target — every Gemini
    /// reads it, including the ones predating JSONL — and this reader no longer
    /// treats it as second class, so nothing here is written in a format casr
    /// itself has demoted.
    ///
    /// Each message carries an `id`, which is not decoration. The first time
    /// Gemini resumes this file it migrates it by replaying every message into
    /// a `.jsonl`, and on the *next* load `isMessageRecord` keeps only records
    /// with a string `id` — so an id-less message survives one resume and then
    /// disappears, taking the conversation with it. An id already present in
    /// `extra` (a session read back from Gemini) wins; otherwise it is derived
    /// from the new session id and the message's position, which keeps the
    /// output byte-identical across runs of the same conversion.
    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let target_session_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();

        // Determine target path.
        let tmp_dir = Self::tmp_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Gemini tmp directory"))?;

        // Use workspace hash for project directory, or a fallback hash.
        let workspace_path = session
            .workspace
            .as_deref()
            .unwrap_or(std::path::Path::new("/tmp"));
        let hash = session
            .metadata
            .get("project_hash")
            .or_else(|| session.metadata.get("projectHash"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| project_hash(workspace_path));
        let chats_dir = tmp_dir.join(&hash).join("chats");
        let filename = session_filename(&target_session_id, &now);
        let target_path = chats_dir.join(&filename);

        debug!(
            target_session_id,
            target_path = %target_path.display(),
            "writing Gemini session"
        );

        // Build the Gemini JSON structure.
        let start_time = session
            .started_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(now)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let last_updated = session
            .ended_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(now)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let mut json_messages: Vec<serde_json::Value> = Vec::with_capacity(session.messages.len());

        for (position, msg) in session.messages.iter().enumerate() {
            json_messages.push(gemini_message_entry(msg, position, &target_session_id));
        }

        let root = serde_json::json!({
            "sessionId": target_session_id,
            "projectHash": hash,
            "startTime": start_time,
            "lastUpdated": last_updated,
            "messages": json_messages,
        });

        let content_bytes = serde_json::to_string_pretty(&root)?.into_bytes();

        let outcome =
            crate::pipeline::atomic_write(&target_path, &content_bytes, opts.force, self.slug())?;

        info!(
            target_session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "Gemini session written"
        );

        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id: target_session_id.clone(),
            resume_command: self.resume_command(&target_session_id),
            backups: outcome.displaced().into_iter().collect(),
            warnings: Vec::new(),
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("gemini --resume {session_id}")
    }
}

// ---------------------------------------------------------------------------
// Reading: one parser, two formats
// ---------------------------------------------------------------------------

/// A session file resolved to session-level fields plus the messages that
/// still exist, whichever of the two formats it was written in.
struct Parsed {
    /// Session-level fields — `sessionId`, `projectHash`, `startTime`,
    /// `lastUpdated`, and whatever else the file carried. Never the `messages`
    /// array: that is [`Parsed::messages`], and copying a whole conversation
    /// into the metadata blob would put it in `casr info --json` twice.
    metadata: serde_json::Map<String, serde_json::Value>,
    messages: Vec<serde_json::Value>,
    /// `Some` only for JSONL. Legacy whole-file JSON has no records to fail to
    /// classify, so the question does not arise and nothing is reported.
    drift: Option<Drift>,
}

/// Records in a `.jsonl` this reader could not classify.
#[derive(Default)]
struct Drift {
    /// Valid JSON, but matching none of the four record shapes.
    unclassified: usize,
    /// Lines that are not JSON at all.
    unparseable_lines: usize,
    /// The object keys those unclassified records carried, deduplicated. This
    /// is the diagnostic that names the next format change; the records
    /// themselves are not kept, because a session file is unbounded and this
    /// travels in `metadata`.
    keys: BTreeSet<String>,
}

impl Drift {
    fn is_nonzero(&self) -> bool {
        self.unclassified > 0 || self.unparseable_lines > 0
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "unclassified": self.unclassified,
            "unparseable_lines": self.unparseable_lines,
            "keys": self.keys.iter().collect::<Vec<_>>(),
        })
    }
}

/// Read a Gemini session file in whichever format it is in.
///
/// The format test is Gemini's, and it is not the extension: the fold runs
/// first and the file is JSONL only if the fold established both `sessionId`
/// and `projectHash`, which is exactly the condition on
/// `parseLegacyRecordFallback`. Extensions lie — the CLI's migration turns
/// `.json` into `.jsonl` by appending a letter, casr writes legacy JSON under
/// `.json`, and a user can hand `casr info` any path.
///
/// The strictness is the whole point, and a weaker test looks fine until it is
/// not. "Did any line classify as a record?" fails on the most ordinary input
/// there is: a *pretty-printed* legacy file ends its `messages` array with one
/// element alone on a line, no trailing comma, and that line parses as a
/// complete object with a string `id` — a message record. The fold then returns
/// that single message and the reader reports a two-message session as
/// one-message, silently. Requiring the session-level fields cannot be spoofed
/// that way: no fragment of a pretty-printed object carries both.
///
/// The last resort below is the one place this goes past the CLI. Gemini gives
/// up when neither route works; a file that classified real records is still
/// better read than refused.
fn parse_session_file(path: &Path) -> anyhow::Result<Parsed> {
    let folded = fold_jsonl(path)?;
    if folded.is_jsonl() {
        return Ok(folded.parsed);
    }

    // Legacy whole-file JSON. Reopened rather than buffered, so that the JSONL
    // path — the common one now — streams instead of holding the file in memory.
    let file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let root: serde_json::Result<serde_json::Value> =
        serde_json::from_reader(std::io::BufReader::new(file));

    let root = match root {
        Ok(root) => root,
        Err(error) if folded.recognized > 0 => {
            debug!(
                path = %path.display(),
                records = folded.recognized,
                %error,
                "Gemini file parses as neither whole-file JSON nor a complete JSONL header; \
                 keeping what the fold recognised"
            );
            return Ok(folded.parsed);
        }
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("failed to parse JSON {}", path.display())));
        }
    };

    let messages = root
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let metadata = match root {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    Ok(Parsed {
        metadata,
        messages,
        drift: None,
    })
}

/// What folding a file as JSONL produced, and how much of it classified.
struct Folded {
    parsed: Parsed,
    /// Lines that matched one of the four record shapes.
    recognized: usize,
}

impl Folded {
    /// Gemini's own test for "this file is JSONL": the fold established the
    /// session-level fields. See [`parse_session_file`] for why a weaker one
    /// misreads pretty-printed legacy files.
    fn is_jsonl(&self) -> bool {
        string_field_in(&self.parsed.metadata, "sessionId").is_some()
            && string_field_in(&self.parsed.metadata, "projectHash").is_some()
    }
}

/// Replay a `.jsonl` edit log into the state it describes.
///
/// Runs on every file, because the extension does not decide the format;
/// [`parse_session_file`] judges the result. Every effect below mirrors
/// Gemini's `loadConversationRecord`, including the ones that look like
/// mistakes: a `$rewindTo` naming a message that is not present clears the
/// whole conversation, and an `$set.messages` array replaces it. Diverging from
/// the CLI here would mean showing the user a conversation their own agent does
/// not have.
fn fold_jsonl(path: &Path) -> anyhow::Result<Folded> {
    let file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut metadata = serde_json::Map::new();
    // Insertion order, with upsert-in-place. This is a JS `Map`: re-appending a
    // message id — which Gemini does whenever it attaches tokens or tool calls
    // to a turn it already wrote — replaces that entry and leaves it where it
    // was. Appending instead would show the turn twice, out of order.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, serde_json::Value> = HashMap::new();
    let mut recognized = 0usize;
    let mut drift = Drift::default();

    for line in std::io::BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            // A binary or truncated file is not a fatal error here: whatever
            // was read still folds, and a file that classified nothing falls
            // through to the whole-file parse, which reports properly.
            Err(error) => {
                trace!(path = %path.display(), %error, "stopped reading Gemini JSONL");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            drift.unparseable_lines += 1;
            continue;
        };

        // The classification order is Gemini's and is load-bearing: a header
        // record also carries a string `sessionId`, and a `$set` payload can
        // carry anything at all, so testing these in a different order files
        // records under the wrong rule.
        if let Some(rewind_to) = string_field(&record, "$rewindTo") {
            recognized += 1;
            rewind(&mut order, &mut by_id, rewind_to);
        } else if string_field(&record, "id").is_some() {
            recognized += 1;
            upsert(&mut order, &mut by_id, record);
        } else if let Some(set) = record.get("$set").and_then(|v| v.as_object()) {
            recognized += 1;
            // A `messages` array here is a wholesale replacement, unlike the
            // header's.
            if let Some(replacement) = set.get("messages").and_then(|v| v.as_array()) {
                order.clear();
                by_id.clear();
                for message in replacement {
                    if string_field(message, "id").is_some() {
                        upsert(&mut order, &mut by_id, message.clone());
                    }
                }
            }
            merge_metadata(&mut metadata, set);
        } else if string_field(&record, "sessionId").is_some()
            && string_field(&record, "projectHash").is_some()
        {
            recognized += 1;
            if let Some(appended) = record.get("messages").and_then(|v| v.as_array()) {
                for message in appended {
                    if string_field(message, "id").is_some() {
                        upsert(&mut order, &mut by_id, message.clone());
                    }
                }
            }
            if let Some(header) = record.as_object() {
                merge_metadata(&mut metadata, header);
            }
        } else {
            drift.unclassified += 1;
            if let Some(object) = record.as_object() {
                drift.keys.extend(object.keys().cloned());
            }
        }
    }

    let messages = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();

    Ok(Folded {
        parsed: Parsed {
            metadata,
            messages,
            drift: Some(drift),
        },
        recognized,
    })
}

/// `$rewindTo`: drop the named message and everything after it.
///
/// A name that is not in the conversation clears it. That is Gemini's rule, not
/// a fallback invented here: the CLI rewinds by walking its map until it sees
/// the id and deleting from there, and `messagesMap.clear()` is what it does
/// when the walk never finds one.
fn rewind(
    order: &mut Vec<String>,
    by_id: &mut HashMap<String, serde_json::Value>,
    rewind_to: &str,
) {
    match order.iter().position(|id| id == rewind_to) {
        Some(index) => {
            for id in order.drain(index..) {
                by_id.remove(&id);
            }
        }
        None => {
            order.clear();
            by_id.clear();
        }
    }
}

/// Insert a message, or replace one already recorded under the same id without
/// moving it.
fn upsert(
    order: &mut Vec<String>,
    by_id: &mut HashMap<String, serde_json::Value>,
    message: serde_json::Value,
) {
    let Some(id) = string_field(&message, "id").map(str::to_string) else {
        return;
    };
    if by_id.insert(id.clone(), message).is_none() {
        order.push(id);
    }
}

/// Shallow-merge a metadata payload, minus `messages`.
///
/// The conversation is tracked separately, and letting it through here would
/// duplicate the entire session inside `CanonicalSession::metadata`.
fn merge_metadata(
    metadata: &mut serde_json::Map<String, serde_json::Value>,
    updates: &serde_json::Map<String, serde_json::Value>,
) {
    for (key, value) in updates {
        if key == "messages" {
            continue;
        }
        metadata.insert(key.clone(), value.clone());
    }
}

fn string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn string_field_in<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    map.get(key).and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One entry of the written `messages` array.
///
/// `position` and `session_id` exist only to derive an `id` for a message that
/// has none; see [`Gemini::write_session`] for why an id-less message is a
/// session that empties itself the second time Gemini opens it.
fn gemini_message_entry(
    msg: &CanonicalMessage,
    position: usize,
    session_id: &str,
) -> serde_json::Value {
    let ts = msg
        .timestamp
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let mut entry = serde_json::json!({
        "type": gemini_message_type(msg),
        "content": gemini_message_content(msg),
    });
    if let Some(t) = ts {
        entry["timestamp"] = serde_json::Value::String(t);
    }

    merge_gemini_extra_fields(&mut entry, &msg.extra);
    // After the merge, so a real Gemini id carried in `extra` wins and only a
    // message that has none gets a derived one.
    if !entry.get("id").is_some_and(serde_json::Value::is_string) {
        entry["id"] = serde_json::Value::String(format!("{session_id}-m{position}"));
    }
    entry
}

fn gemini_message_type(msg: &CanonicalMessage) -> String {
    match msg.role {
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "model".to_string(),
        MessageRole::Tool => "tool".to_string(),
        MessageRole::System => "system".to_string(),
        MessageRole::Other(ref other) => other.clone(),
    }
}

fn gemini_extract_text_content(
    message: &serde_json::Value,
    content: Option<&serde_json::Value>,
) -> String {
    let extracted = match content {
        Some(value) => match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => {
                let mut text_parts: Vec<String> = Vec::new();
                for part in parts {
                    match part {
                        serde_json::Value::String(s) => text_parts.push(s.clone()),
                        serde_json::Value::Object(obj) => {
                            let block_type = obj.get("type").and_then(|v| v.as_str());
                            if (matches!(
                                block_type,
                                Some("text") | Some("input_text") | Some("output_text")
                            ) || block_type.is_none())
                                && let Some(text) = obj.get("text").and_then(|v| v.as_str())
                            {
                                text_parts.push(text.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                text_parts.join("\n")
            }
            serde_json::Value::Object(obj) => obj
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        },
        None => String::new(),
    };

    if !extracted.trim().is_empty() {
        return extracted;
    }

    // Gemini often stores assistant prose in `thoughts` while keeping
    // `content` empty when messages are tool-heavy. Preserve this fallback so
    // list/info metrics and cross-provider transforms don't look artificially
    // sparse.
    message
        .get("thoughts")
        .map(gemini_extract_thoughts_text)
        .unwrap_or_default()
}

fn gemini_extract_thoughts_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                match item {
                    serde_json::Value::String(s) => {
                        if !s.trim().is_empty() {
                            parts.push(s.to_string());
                        }
                    }
                    serde_json::Value::Object(obj) => {
                        let subject = obj
                            .get("subject")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        let description = obj
                            .get("description")
                            .or_else(|| obj.get("text"))
                            .or_else(|| obj.get("summary"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();

                        if !subject.is_empty() && !description.is_empty() {
                            parts.push(format!("{subject}: {description}"));
                        } else if !description.is_empty() {
                            parts.push(description.to_string());
                        } else if !subject.is_empty() {
                            parts.push(subject.to_string());
                        }
                    }
                    _ => {
                        let flat = flatten_content(item);
                        if !flat.trim().is_empty() {
                            parts.push(flat);
                        }
                    }
                }
            }
            parts.join("\n\n")
        }
        serde_json::Value::Object(obj) => obj
            .get("description")
            .or_else(|| obj.get("text"))
            .or_else(|| obj.get("summary"))
            .or_else(|| obj.get("subject"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn gemini_extract_tool_calls(
    message: &serde_json::Value,
    content: Option<&serde_json::Value>,
) -> Vec<ToolCall> {
    let mut calls: Vec<ToolCall> = Vec::new();

    if let Some(serde_json::Value::Array(parts)) = content {
        for part in parts {
            let Some(obj) = part.as_object() else {
                continue;
            };
            if obj.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }

            calls.push(ToolCall {
                id: obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: obj.get("input").cloned().unwrap_or(serde_json::Value::Null),
            });
        }
    }

    if let Some(tool_calls) = message.get("toolCalls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            let Some(obj) = call.as_object() else {
                continue;
            };
            calls.push(ToolCall {
                id: obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: obj.get("args").cloned().unwrap_or(serde_json::Value::Null),
            });
        }
    }

    calls
}

fn gemini_extract_tool_results(
    message: &serde_json::Value,
    content: Option<&serde_json::Value>,
) -> Vec<ToolResult> {
    let mut results: Vec<ToolResult> = Vec::new();

    if let Some(serde_json::Value::Array(parts)) = content {
        for part in parts {
            let Some(obj) = part.as_object() else {
                continue;
            };
            if obj.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }

            let content_text = obj
                .get("content")
                .map(flatten_content)
                .or_else(|| obj.get("output").map(flatten_content))
                .unwrap_or_default();

            results.push(ToolResult {
                call_id: obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                content: content_text,
                is_error: obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
    }

    if let Some(tool_calls) = message.get("toolCalls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            let Some(obj) = call.as_object() else {
                continue;
            };

            let has_result = obj.get("result").is_some() || obj.get("resultDisplay").is_some();
            if !has_result {
                continue;
            }

            let content_text = gemini_tool_call_result_text(call);
            results.push(ToolResult {
                call_id: obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                content: content_text,
                is_error: obj.get("status").and_then(|v| v.as_str()) == Some("error"),
            });
        }
    }

    results
}

fn gemini_tool_call_result_text(call: &serde_json::Value) -> String {
    if let Some(s) = call.get("resultDisplay").and_then(|v| v.as_str())
        && !s.trim().is_empty()
    {
        return s.to_string();
    }

    if let Some(s) = call
        .pointer("/result/0/functionResponse/response/output")
        .and_then(|v| v.as_str())
        && !s.trim().is_empty()
    {
        return s.to_string();
    }

    if let Some(s) = call
        .pointer("/result/0/functionResponse/response/error")
        .and_then(|v| v.as_str())
        && !s.trim().is_empty()
    {
        return s.to_string();
    }

    if let Some(result) = call.get("result") {
        let flat = flatten_content(result);
        if !flat.trim().is_empty() {
            return flat;
        }
        if let Ok(serialized) = serde_json::to_string(result) {
            return serialized;
        }
    }

    String::new()
}

fn gemini_message_content(msg: &CanonicalMessage) -> serde_json::Value {
    if let Some(content) = msg.extra.get("content")
        && !content.is_null()
    {
        return content.clone();
    }

    if msg.tool_calls.is_empty() && msg.tool_results.is_empty() {
        return serde_json::Value::String(msg.content.clone());
    }

    let mut blocks: Vec<serde_json::Value> = Vec::new();
    if !msg.content.is_empty() {
        blocks.push(serde_json::json!({
            "type": "text",
            "text": msg.content,
        }));
    }
    for tc in &msg.tool_calls {
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": tc.id.as_deref().unwrap_or(""),
            "name": tc.name,
            "input": tc.arguments,
        }));
    }
    for tr in &msg.tool_results {
        blocks.push(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tr.call_id.as_deref().unwrap_or(""),
            "content": tr.content,
            "is_error": tr.is_error,
        }));
    }

    if blocks.is_empty() {
        serde_json::Value::String(msg.content.clone())
    } else {
        serde_json::Value::Array(blocks)
    }
}

fn merge_gemini_extra_fields(entry: &mut serde_json::Value, extra: &serde_json::Value) {
    let Some(entry_obj) = entry.as_object_mut() else {
        return;
    };
    let Some(extra_obj) = extra.as_object() else {
        return;
    };

    for (k, v) in extra_obj {
        if k == "type" || k == "content" || k == "timestamp" {
            continue;
        }
        entry_obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

/// Try to extract a workspace path from message content.
///
/// Scans the first N messages for common path patterns:
/// - `"# AGENTS.md instructions for /data/projects/foo"`
/// - `"Working directory: /path/to/project"`
/// - Any `/data/projects/X` reference
fn extract_workspace_from_messages(messages: &[CanonicalMessage]) -> Option<PathBuf> {
    let scan_limit = messages.len().min(50);
    for msg in &messages[..scan_limit] {
        // Look for /data/projects/ patterns (common convention).
        if let Some(idx) = msg.content.find("/data/projects/") {
            let rest = &msg.content[idx..];
            // Extract project name (next path segment after /data/projects/).
            let project_path: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'' && *c != ')')
                .collect();
            // Normalize to just /data/projects/<name>
            let parts: Vec<&str> = project_path.split('/').collect();
            if parts.len() >= 4 {
                let normalized = format!("/{}/{}/{}", parts[1], parts[2], parts[3]);
                return normalize_workspace_candidate(&normalized);
            }
        }
        // Look for absolute paths on common prefixes.
        for prefix in ["/home/", "/Users/", "/root/"] {
            if let Some(idx) = msg.content.find(prefix) {
                let rest = &msg.content[idx..];
                let path: String = rest
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                    .collect();
                if path.len() > prefix.len() + 3 {
                    return normalize_workspace_candidate(&path);
                }
            }
        }
    }
    None
}

fn normalize_workspace_candidate(raw: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(raw);
    if candidate.as_os_str().is_empty() {
        return None;
    }

    if candidate.exists() && candidate.is_file() {
        return candidate
            .parent()
            .map(Path::to_path_buf)
            .or(Some(candidate));
    }

    let looks_like_file = candidate
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.contains('.') && !name.starts_with('.'));
    if looks_like_file
        && let Some(parent) = candidate.parent()
        && !parent.as_os_str().is_empty()
    {
        return Some(parent.to_path_buf());
    }

    Some(candidate)
}

/// The `sessionId` a file declares, read as cheaply as the format allows.
///
/// JSONL puts it on the first line — the header record `initialize()` writes
/// before anything else, and the line Gemini's own deletion path reads with a
/// single 4 KiB `read`. Only when that fails is the whole file parsed as legacy
/// JSON. Listing a directory calls this once per file, so a whole-file parse of
/// every multi-megabyte transcript to recover one string is not free.
///
/// A rewind cannot change the answer: `$rewindTo` removes messages and
/// `$set` never rewrites `sessionId` to anything but itself
/// (`updateMetadata({ sessionId })` at migration), so the first line stays
/// authoritative for the life of the file.
fn session_id_from_file(path: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct GeminiHeader {
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
    }

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);

    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_ok()
        && let Ok(header) = serde_json::from_str::<GeminiHeader>(&first_line)
        && let Some(session_id) = header.session_id
    {
        return Some(session_id);
    }

    let file = std::fs::File::open(path).ok()?;
    let header: GeminiHeader = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    header.session_id
}

#[cfg(test)]
mod tests {
    use super::{
        Gemini, gemini_message_content, gemini_message_entry, gemini_message_type,
        is_session_file_name, merge_gemini_extra_fields, normalize_workspace_candidate,
        project_hash, session_filename,
    };
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::path::Path;

    use crate::model::{CanonicalMessage, MessageRole, ToolCall, ToolResult};
    use crate::providers::Provider;

    #[test]
    fn project_hash_matches_observed_sha256_mapping() {
        let workspace = Path::new("/data/projects/flywheel_gateway");
        let hash = project_hash(workspace);
        assert_eq!(
            hash,
            "b7da685261f0fff76430fd68dd709a693a8abac1c72c19c49f2fd1c7424c6d4e"
        );
    }

    #[test]
    fn workspace_candidate_file_path_normalizes_to_parent_dir() {
        let got = normalize_workspace_candidate("/data/projects/foo/README.md")
            .expect("workspace should normalize");
        assert_eq!(got, Path::new("/data/projects/foo"));
    }

    #[test]
    fn workspace_candidate_hidden_directory_is_preserved() {
        let got = normalize_workspace_candidate("/home/ubuntu/.config")
            .expect("workspace should normalize");
        assert_eq!(got, Path::new("/home/ubuntu/.config"));
    }

    #[test]
    fn session_filename_uses_timestamp_and_uuid_prefix() {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 10, 2, 6, 44)
            .single()
            .expect("valid timestamp");
        let filename = session_filename("8c1890a5-eb39-4c5c-acff-93790d35dd3f", &now);
        assert_eq!(filename, "session-2026-01-10T02-06-8c1890a5.json");
    }

    #[test]
    fn message_content_prefers_extra_content_and_preserves_blocks() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "fallback".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({
                "content": [
                    {"type": "text", "text": "primary"},
                    {"type": "grounding", "source": "doc://1"}
                ]
            }),
        };

        let content = gemini_message_content(&msg);
        assert_eq!(
            content,
            json!([
                {"type": "text", "text": "primary"},
                {"type": "grounding", "source": "doc://1"}
            ])
        );
    }

    #[test]
    fn message_content_falls_back_to_tool_blocks_when_needed() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![ToolCall {
                id: Some("call-7".to_string()),
                name: "read_file".to_string(),
                arguments: json!({"path":"README.md"}),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("call-7".to_string()),
                content: "ok".to_string(),
                is_error: false,
            }],
            extra: serde_json::Value::Null,
        };

        let content = gemini_message_content(&msg);
        let blocks = content
            .as_array()
            .expect("tool-rich Gemini content should serialize as array");
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
        assert!(blocks.iter().any(|b| b["type"] == "tool_result"));
    }

    #[test]
    fn merge_gemini_extra_fields_keeps_annotations() {
        let mut entry = json!({
            "type": "model",
            "content": "hello"
        });
        let extra = json!({
            "groundingMetadata": {"sourceCount": 2},
            "citations": [{"uri":"doc://x"}],
            "timestamp": "should-not-overwrite",
            "content": "should-not-overwrite",
            "type": "should-not-overwrite"
        });

        merge_gemini_extra_fields(&mut entry, &extra);
        assert_eq!(entry["groundingMetadata"]["sourceCount"], 2);
        assert_eq!(entry["citations"][0]["uri"], "doc://x");
        assert_eq!(entry["type"], "model");
        assert_eq!(entry["content"], "hello");
    }

    #[test]
    fn message_type_preserves_non_user_roles() {
        let assistant = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: String::new(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };
        let tool = CanonicalMessage {
            role: MessageRole::Tool,
            ..assistant.clone()
        };
        let system = CanonicalMessage {
            role: MessageRole::System,
            ..assistant.clone()
        };
        let other = CanonicalMessage {
            role: MessageRole::Other("reviewer".to_string()),
            ..assistant
        };

        assert_eq!(gemini_message_type(&tool), "tool");
        assert_eq!(gemini_message_type(&system), "system");
        assert_eq!(gemini_message_type(&other), "reviewer");
    }

    #[test]
    fn resume_command_uses_resume_flag() {
        let provider = Gemini;
        assert_eq!(
            <Gemini as Provider>::resume_command(&provider, "abc123"),
            "gemini --resume abc123"
        );
    }

    // -----------------------------------------------------------------------
    // Reader unit tests
    // -----------------------------------------------------------------------

    use std::io::Write as _;

    /// Write JSON content to a temp file and read it back.
    fn read_gemini_json(content: &str) -> crate::model::CanonicalSession {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();
        Gemini
            .read_session(tmp.path())
            .unwrap_or_else(|e| panic!("read_session failed: {e}"))
    }

    #[test]
    fn reader_basic_user_model_exchange() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-test-1",
                "startTime": "2026-01-01T00:00:00Z",
                "lastUpdated": "2026-01-01T00:05:00Z",
                "messages": [
                    {"type": "user", "content": "Hello", "timestamp": "2026-01-01T00:00:00Z"},
                    {"type": "model", "content": "Hi there", "timestamp": "2026-01-01T00:01:00Z"}
                ]
            }"#,
        );
        assert_eq!(session.session_id, "gmi-test-1");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Hi there");
        assert!(session.started_at.is_some());
    }

    #[test]
    fn reader_gemini_role_maps_to_assistant() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-role-test",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "gemini", "content": "A"}
                ]
            }"#,
        );
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn reader_array_content_blocks() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-blocks",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": [
                        {"type": "text", "text": "Main answer."},
                        {"type": "grounding", "source": "doc://ref"}
                    ]}
                ]
            }"#,
        );
        assert_eq!(session.messages[1].content, "Main answer.");
    }

    #[test]
    fn reader_falls_back_to_thoughts_when_content_empty() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-thoughts-fallback",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "gemini", "content": "", "thoughts": "Reasoned answer hidden in thoughts"}
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(
            session.messages[1].content,
            "Reasoned answer hidden in thoughts"
        );
    }

    #[test]
    fn reader_extracts_tool_blocks_while_flattening_text() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-tool-blocks",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": [
                        {"type": "text", "text": "Main answer."},
                        {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                        {"type": "tool_result", "tool_use_id": "call-1", "content": "ok"}
                    ]}
                ]
            }"#,
        );
        assert_eq!(session.messages[1].content, "Main answer.");
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert_eq!(session.messages[1].tool_calls[0].name, "Bash");
        assert_eq!(session.messages[1].tool_results.len(), 1);
        assert_eq!(session.messages[1].tool_results[0].content, "ok");
    }

    #[test]
    fn reader_extracts_top_level_tool_calls_and_results() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-top-toolcalls",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {
                        "type": "gemini",
                        "content": "A",
                        "toolCalls": [
                            {
                                "id": "call-1",
                                "name": "read_file",
                                "args": {"file_path": "README.md"},
                                "status": "success",
                                "resultDisplay": "file contents"
                            }
                        ]
                    }
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert_eq!(session.messages[1].tool_calls[0].name, "read_file");
        assert_eq!(session.messages[1].tool_results.len(), 1);
        assert_eq!(session.messages[1].tool_results[0].content, "file contents");
    }

    #[test]
    fn reader_keeps_tool_only_messages() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-tool-only",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {
                        "type": "model",
                        "content": [
                            {"type": "tool_use", "id": "call-9", "name": "Bash", "input": {"command": "ls"}}
                        ]
                    }
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert!(session.messages[1].content.is_empty());
    }

    #[test]
    fn reader_preserves_extra_fields() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-extra",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": "A", "groundingMetadata": {"count": 3}, "citations": []}
                ]
            }"#,
        );
        assert!(session.messages[1].extra.get("groundingMetadata").is_some());
        assert!(session.messages[1].extra.get("citations").is_some());
    }

    #[test]
    fn reader_skips_empty_messages() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-empty",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": ""},
                    {"type": "model", "content": "   "},
                    {"type": "model", "content": "Valid"}
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "Valid");
    }

    #[test]
    fn reader_keeps_thoughts_only_messages() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-thoughts-only",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": "", "thoughts": "Internal explanation"}
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "Internal explanation");
    }

    #[test]
    fn reader_extracts_structured_thoughts_array() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-thoughts-array",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {
                        "type": "gemini",
                        "content": "",
                        "thoughts": [
                            {"subject": "Plan", "description": "Investigate parser edge cases"},
                            {"subject": "Result", "description": "Patched fallback extraction"}
                        ]
                    }
                ]
            }"#,
        );
        assert_eq!(session.messages.len(), 2);
        assert!(
            session.messages[1]
                .content
                .contains("Plan: Investigate parser edge cases")
        );
        assert!(
            session.messages[1]
                .content
                .contains("Result: Patched fallback extraction")
        );
    }

    #[test]
    fn reader_session_id_fallback_to_filename() {
        let session = read_gemini_json(
            r#"{
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": "A"}
                ]
            }"#,
        );
        // No sessionId in JSON → falls back to filename stem minus "session-" prefix.
        assert!(!session.session_id.is_empty());
    }

    #[test]
    fn reader_empty_messages_array() {
        let session = read_gemini_json(r#"{"sessionId": "gmi-empty-arr", "messages": []}"#);
        assert_eq!(session.messages.len(), 0);
    }

    #[test]
    fn reader_missing_messages_key() {
        let session = read_gemini_json(r#"{"sessionId": "gmi-no-msgs"}"#);
        assert_eq!(session.messages.len(), 0);
    }

    #[test]
    fn reader_project_hash_preserved_in_metadata() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-hash",
                "projectHash": "abc123def",
                "messages": [
                    {"type": "user", "content": "Q"},
                    {"type": "model", "content": "A"}
                ]
            }"#,
        );
        assert_eq!(session.metadata["project_hash"].as_str(), Some("abc123def"));
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-title",
                "messages": [
                    {"type": "user", "content": "Explain the architecture of this system"},
                    {"type": "model", "content": "The system uses..."}
                ]
            }"#,
        );
        assert_eq!(
            session.title.as_deref(),
            Some("Explain the architecture of this system")
        );
    }

    #[test]
    fn reader_timestamp_tracking() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-ts",
                "startTime": "2026-01-01T00:00:00Z",
                "lastUpdated": "2026-01-01T01:00:00Z",
                "messages": [
                    {"type": "user", "content": "Q", "timestamp": "2026-01-01T00:30:00Z"},
                    {"type": "model", "content": "A", "timestamp": "2026-01-01T00:45:00Z"}
                ]
            }"#,
        );
        assert!(session.started_at.is_some());
        assert!(session.ended_at.is_some());
        // ended_at should be max of lastUpdated and message timestamps.
        assert!(session.ended_at.unwrap() >= session.started_at.unwrap());
    }

    // -----------------------------------------------------------------------
    // Writer helper unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn writer_content_plain_string_without_extra() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Simple text".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };
        let content = gemini_message_content(&msg);
        assert!(
            content.is_string(),
            "Gemini content without extra should be plain string"
        );
        assert_eq!(content.as_str().unwrap(), "Simple text");
    }

    #[test]
    fn writer_user_type_is_user() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: String::new(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };
        assert_eq!(gemini_message_type(&msg), "user");
    }

    // -----------------------------------------------------------------------
    // JSONL: the format Gemini actually writes
    // -----------------------------------------------------------------------

    /// Read JSONL content the way a real `chats/` directory holds it.
    ///
    /// The suffix is `.jsonl` for realism only — [`super::parse_session_file`]
    /// decides by content, because the CLI's own migration produces `.jsonl`
    /// files by appending a letter to a name and nothing guarantees the two
    /// agree.
    fn read_gemini_jsonl(content: &str) -> crate::model::CanonicalSession {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();
        Gemini
            .read_session(tmp.path())
            .unwrap_or_else(|e| panic!("read_session failed: {e}"))
    }

    const HEADER: &str = r#"{"sessionId":"gmi-jsonl","projectHash":"deadbeef","startTime":"2026-03-02T09:00:00.000Z","lastUpdated":"2026-03-02T09:00:00.000Z"}"#;

    /// A `.jsonl` is read at all.
    ///
    /// Against the old reader — a whole-file `serde_json::from_reader` — this
    /// fails with `failed to parse JSON … trailing characters at line 2`.
    #[test]
    fn reader_reads_jsonl_sessions() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"Q\"}}\n\
             {{\"id\":\"g1\",\"type\":\"gemini\",\"content\":\"A\"}}\n"
        ));

        assert_eq!(session.session_id, "gmi-jsonl");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.metadata["project_hash"].as_str(), Some("deadbeef"));
    }

    /// The rewind. A reader that collects lines shows the user content their
    /// own agent has thrown away.
    #[test]
    fn reader_jsonl_rewind_removes_the_rewound_turns() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"kept\"}}\n\
             {{\"id\":\"u2\",\"type\":\"user\",\"content\":\"rewound\"}}\n\
             {{\"id\":\"g2\",\"type\":\"gemini\",\"content\":\"also rewound\"}}\n\
             {{\"$rewindTo\":\"u2\"}}\n\
             {{\"id\":\"u3\",\"type\":\"user\",\"content\":\"after\"}}\n"
        ));

        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            contents,
            ["kept", "after"],
            "$rewindTo removes the named message and everything after it"
        );
    }

    /// `$rewindTo` naming a message that is not there clears the conversation.
    ///
    /// Gemini's loader falls back to `messagesMap.clear()` when its walk never
    /// finds the id, so a session that looks empty in the CLI has to look empty
    /// here. Guessing "it probably meant nothing" would show turns the agent
    /// will not.
    #[test]
    fn reader_jsonl_rewind_to_an_absent_id_clears_the_conversation() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"gone\"}}\n\
             {{\"$rewindTo\":\"never-recorded\"}}\n"
        ));

        assert!(session.messages.is_empty());
    }

    /// `$set.messages` is a wholesale replacement, not an append.
    #[test]
    fn reader_jsonl_set_messages_replaces_the_conversation() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"displaced\"}}\n\
             {{\"$set\":{{\"messages\":[{{\"id\":\"u9\",\"type\":\"user\",\"content\":\"replacement\"}}],\"summary\":\"s\"}}}}\n"
        ));

        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["replacement"]);
        // …and the replacement list does not also land in the metadata blob.
        assert!(session.metadata.get("messages").is_none());
    }

    /// Re-appending a message id replaces that message where it already is.
    ///
    /// Gemini does this on every turn it later attaches tokens or tool calls
    /// to. Appending instead would show the turn twice and out of order.
    #[test]
    fn reader_jsonl_repeated_id_replaces_in_place() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"g1\",\"type\":\"gemini\",\"content\":\"draft\"}}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"next\"}}\n\
             {{\"id\":\"g1\",\"type\":\"gemini\",\"content\":\"final\"}}\n"
        ));

        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["final", "next"]);
    }

    /// Records nobody understands are counted and named, not dropped.
    #[test]
    fn reader_jsonl_unrecognized_records_are_reported() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n\
             {{\"id\":\"u1\",\"type\":\"user\",\"content\":\"Q\"}}\n\
             {{\"$somethingNew\":1,\"extra\":2}}\n\
             this line is not json at all\n"
        ));

        assert_eq!(session.messages.len(), 1);
        let report = &session.metadata["unrecognized_records"];
        assert_eq!(report["unclassified"], 1);
        assert_eq!(report["unparseable_lines"], 1);
        assert_eq!(report["keys"], json!(["$somethingNew", "extra"]));
    }

    /// A clean JSONL says so by saying nothing.
    #[test]
    fn reader_jsonl_reports_nothing_when_every_record_classified() {
        let session = read_gemini_jsonl(&format!(
            "{HEADER}\n{{\"id\":\"u1\",\"type\":\"user\",\"content\":\"Q\"}}\n"
        ));

        assert!(session.metadata.get("unrecognized_records").is_none());
    }

    /// Pretty-printed legacy JSON is not JSONL, however much one of its lines
    /// looks like a record.
    ///
    /// The last element of a pretty-printed `messages` array sits alone on a
    /// line with no trailing comma, so it parses as a complete object with a
    /// string `id` — a message record. A format test of "did any line
    /// classify?" therefore reads a whole legacy session as its final message
    /// and reports the loss nowhere. Requiring the fold to have established the
    /// session-level fields is what makes that impossible.
    #[test]
    fn reader_pretty_printed_legacy_json_is_not_folded_as_jsonl() {
        let session = read_gemini_json(
            r#"{
                "sessionId": "gmi-legacy-pretty",
                "projectHash": "deadbeef",
                "messages": [
                    {"id": "m1", "type": "user", "content": "first"},
                    {"id": "m2", "type": "gemini", "content": "second"}
                ]
            }"#,
        );

        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["first", "second"]);
        assert!(session.metadata.get("unrecognized_records").is_none());
    }

    /// A legacy session on one line carries both session-level fields, so it
    /// folds as a header record — and reads the same either way.
    #[test]
    fn reader_single_line_legacy_json_reads_the_same() {
        let session = read_gemini_jsonl(
            r#"{"sessionId":"gmi-one-line","projectHash":"deadbeef","messages":[{"id":"m1","type":"user","content":"first"},{"id":"m2","type":"gemini","content":"second"}]}"#,
        );

        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["first", "second"]);
    }

    #[test]
    fn session_file_names_cover_both_extensions_but_not_subagents() {
        assert!(is_session_file_name(
            "session-2026-01-10T02-06-8c1890a5.json"
        ));
        assert!(is_session_file_name(
            "session-2026-01-10T02-06-8c1890a5.jsonl"
        ));
        // Subagent transcripts are `<session-id>.jsonl` in a nested directory.
        // Gemini will not resume one, so it is not a session file here either.
        assert!(!is_session_file_name(
            "99999999-aaaa-bbbb-cccc-dddddddddddd.jsonl"
        ));
        assert!(!is_session_file_name("session-notes.txt"));
    }

    // -----------------------------------------------------------------------
    // Writer helper unit tests (continued)
    // -----------------------------------------------------------------------

    /// Every written message carries an `id`.
    ///
    /// Without one the session survives exactly one Gemini resume: the resume
    /// migrates the file by replaying each message into a `.jsonl`, and the
    /// next load keeps only records with a string `id`.
    #[test]
    fn writer_gives_every_message_a_stable_id() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Simple text".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };

        let entry = gemini_message_entry(&msg, 3, "sess-1");
        assert_eq!(entry["id"], json!("sess-1-m3"));
        // Derived from the position, so re-running the same conversion twice
        // produces the same bytes.
        assert_eq!(gemini_message_entry(&msg, 3, "sess-1"), entry);
    }

    /// A real Gemini id survives the round trip instead of being overwritten.
    #[test]
    fn writer_keeps_the_id_a_gemini_session_already_had() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "A".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({"id": "g-1", "model": "gemini-3-pro"}),
        };

        let entry = gemini_message_entry(&msg, 0, "sess-1");
        assert_eq!(entry["id"], json!("g-1"));
        assert_eq!(entry["model"], json!("gemini-3-pro"));
    }

    #[test]
    fn writer_assistant_type_is_model() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: String::new(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: serde_json::Value::Null,
        };
        assert_eq!(gemini_message_type(&msg), "model");
    }
}
