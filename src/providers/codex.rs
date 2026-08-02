//! Codex provider — reads/writes JSONL sessions under `~/.codex/sessions/`.
//!
//! Session files: `YYYY/MM/DD/rollout-N.jsonl`
//! Resume command: `codex resume <session-id>`
//!
//! ## JSONL format (modern envelope)
//!
//! Each line: `{ "type": "session_meta|response_item|event_msg", "timestamp": …, "payload": {…} }`
//!
//! - `session_meta` → workspace (`payload.cwd`), session ID (`payload.id`).
//! - `response_item` → main conversational messages (`payload.role`, `payload.content`).
//! - `event_msg` → sub-typed: `user_message`, `agent_reasoning` (conversational);
//!   `token_count`, `turn_aborted` (non-conversational).
//!
//! ## Legacy JSON format
//!
//! Single object: `{ "session": { "id", "cwd" }, "items": [ {role, content, timestamp} ] }`
//!
//! ## What Codex itself will list — measured, not assumed
//!
//! Everything below was measured against `@openai/codex` 0.145.0 (the shipped
//! `codex app-server`, driven over stdio with `thread/list`) by planting real
//! rollouts at chosen paths under a throwaway `CODEX_HOME`. It is what
//! [`Codex::is_session_path`] and [`Codex::list_sessions`] encode.
//!
//! * **Position.** The scan reaches exactly
//!   `<sessions|archived_sessions>/<a>/<b>/<c>/rollout-*` — one file, three
//!   directories, no more and no less. The same rollout planted directly in
//!   `sessions/`, one level down, two levels down, four levels down or five
//!   levels down is not listed.
//! * **Those three components are integers, not a date.** `2026/07/18` lists;
//!   so do `2026/7/8`, `+026/07/18`, `0000/00/00`, `2026/255/18` and
//!   `65535/07/18`. `65536/07/18`, `2026/256/18`, `2026/07/256`, `70000/07/18`,
//!   `-1/07/18`, `aaaa/bb/cc`, `a026/07/18`, `2026/07/1a`, `2026/0_7/18`,
//!   `2026/07/ 18` and `2026/07/18.0` do not. Those widths are exactly
//!   `u16 / u8 / u8` and Rust's `FromStr`, which is why [`is_rollout_layout`]
//!   parses rather than pattern-matches: it reproduces every one of those
//!   observations, including the `+` and the two overflow boundaries, without
//!   guessing at a calendar rule the artifact does not apply.
//! * **Extension.** `.jsonl` and `.jsonl.zst` are rollouts; `.json` and `.ZST`
//!   are not, at the correct depth, with genuine rollout content. 0.145.0
//!   compresses rollouts in place (`rollout/src/compression.rs`), so a
//!   `.jsonl.zst` is an ordinary session of the user's — see
//!   [`reject_compressed_rollout`] for why casr reports rather than decodes it.
//! * **Prefix.** `rollout-` is required; the same content under another name is
//!   not listed.
//! * **Content, not position.** `thread/list` filters on the `ThreadSourceKind`
//!   it derives from `session_meta.payload.source`, and omitting `sourceKinds`
//!   "defaults to interactive sources". A `subagent` source is excluded there
//!   while sitting in the same day directory as its parent, so no path
//!   predicate can express it — see [`is_subagent_rollout`]. The default also
//!   drops `source: "exec"`, which casr keeps on purpose;
//!   [`Codex::list_sessions`] says why.
//! * **Archived.** `archived_sessions/` is a flat *sibling* of `sessions/`, and
//!   a rollout's archive state is which of the two it is under. `thread/list`
//!   returns archived threads only when asked (`archived: true`), so casr
//!   resolves an archived session by id but does not list it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

use crate::discovery::DetectionResult;
use crate::ir::SessionIr;
use crate::model::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult, flatten_content,
    normalize_role, parse_timestamp, reindex_messages, truncate_title,
};
use crate::providers::{
    Provider, SessionListing, StructuredWrite, WriteOptions, WrittenSession, store_evidence,
    walk_entry_reporting,
};

/// Codex provider implementation.
pub struct Codex;

/// The `CODEX_HOME` subdirectory holding live rollouts.
const SESSIONS_DIR: &str = "sessions";

/// The `CODEX_HOME` subdirectory holding archived rollouts.
///
/// A flat sibling of [`SESSIONS_DIR`], not a child of it: `codex doctor`
/// checks that "rows under `archived_sessions` are archived and rows under
/// `sessions` are active", and a rollout moved to
/// `<CODEX_HOME>/archived_sessions/<y>/<m>/<d>/` comes back from `thread/list`
/// with `archived: true` set while one under
/// `<CODEX_HOME>/sessions/archived_sessions/…` does not.
const ARCHIVED_SESSIONS_DIR: &str = "archived_sessions";

/// The compressed rollout extension 0.145.0 writes.
const ROLLOUT_ZST_SUFFIX: &str = ".jsonl.zst";

/// Generate the Codex rollout file path for a new session.
///
/// Convention: `~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<session-id>.jsonl`
///
/// The session ID is a ULID (timestamp-prefixed UUID).
pub fn rollout_path(
    sessions_dir: &Path,
    session_id: &str,
    now: &chrono::DateTime<chrono::Utc>,
) -> PathBuf {
    let date_dir = now.format("%Y/%m/%d").to_string();
    let ts_part = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    let filename = format!("rollout-{ts_part}-{session_id}.jsonl");
    sessions_dir.join(date_dir).join(filename)
}

impl Codex {
    /// Root directory for Codex data.
    /// Respects `CODEX_HOME` env var override.
    fn home_dir() -> Option<PathBuf> {
        if let Ok(home) = std::env::var("CODEX_HOME") {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir().map(|h| h.join(".codex"))
    }

    /// Sessions directory where rollout files live.
    fn sessions_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join(SESSIONS_DIR))
    }

    /// Where `codex archive <id>` moves a rollout to.
    ///
    /// Its own top-level directory, not a subdirectory of
    /// [`Codex::sessions_dir`]; see [`ARCHIVED_SESSIONS_DIR`].
    fn archived_sessions_dir() -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join(ARCHIVED_SESSIONS_DIR))
    }

    /// Both rollout roots, live first, whether or not they exist yet.
    fn rollout_root_paths() -> Vec<PathBuf> {
        [Self::sessions_dir(), Self::archived_sessions_dir()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// The rollout roots that are directories right now.
    fn rollout_roots() -> Vec<PathBuf> {
        Self::rollout_root_paths()
            .into_iter()
            .filter(|dir| dir.is_dir())
            .collect()
    }
}

impl Provider for Codex {
    fn name(&self) -> &str {
        "Codex"
    }

    fn slug(&self) -> &str {
        "codex"
    }

    fn cli_alias(&self) -> &str {
        "cod"
    }

    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut installed = false;

        if which::which("codex").is_ok() {
            evidence.push("codex binary found in PATH".to_string());
            installed = true;
        }

        if let Some(home) = Self::home_dir()
            && home.is_dir()
        {
            evidence.push(format!("{} exists", home.display()));
            installed = true;
        }

        // `~/.codex` exists as soon as a config is written; `~/.codex/sessions`
        // only once a session has run, and it is the one `list` reads.
        if installed && let Some(sessions) = Self::sessions_dir() {
            evidence.push(store_evidence(&sessions));
        }

        trace!(provider = "codex", ?evidence, installed, "detection");
        DetectionResult {
            installed,
            version: None,
            evidence,
        }
    }

    /// Both `sessions/` and `archived_sessions/`.
    ///
    /// The archived root is here so that `casr <path-to-an-archived-rollout>`
    /// is attributed to Codex — `ProviderRegistry::resolve_session` decides
    /// ownership of an explicit path by `path.starts_with(root)`, and with only
    /// the live root an archived rollout fell through to the signature-sniffing
    /// fallback. It is deliberately *not* what `list` reads; see
    /// [`Codex::list_sessions`].
    fn session_roots(&self) -> Vec<PathBuf> {
        Self::rollout_roots()
    }

    /// The rollouts a user could resume in Codex, minus the agent's own.
    ///
    /// Two exclusions, and they are different kinds of statement.
    ///
    /// `archived_sessions/` is left out because `thread/list` returns archived
    /// threads only for an explicit `archived: true` — a plain call does not
    /// see them, and `codex archive` exists precisely to take a session out of
    /// the picker. [`Codex::owns_session`] still finds one by id, because
    /// asking to convert a named session is not the same question as asking
    /// what there is.
    ///
    /// Subagent rollouts are left out because Codex excludes them by
    /// `sourceKinds`, which is a fact about the record and not about where it
    /// sits: they are written into the same `<y>/<m>/<d>` directory as the
    /// thread that spawned them. On the corpus this was measured against that
    /// is 576 of 660 files — a listing that shows them is not a longer answer
    /// to the user's question, it is a different one.
    ///
    /// One measured difference is deliberately *not* reproduced. A default
    /// `thread/list` also drops `source: "exec"` — pointed at a store holding
    /// three `cli`, two `exec` and two subagent rollouts it returned the three
    /// `cli`; `sourceKinds: ["exec"]` returned the two. But a `codex exec` run
    /// is the user's own work, not the agent's plumbing, and casr converts
    /// sessions rather than offering them to Codex's picker: withholding those
    /// 39 of the user's 660 would be losing sessions to match a filter whose
    /// purpose is a resume menu. If that trade is ever revisited, it is one
    /// line here, and the fact it turns on is recorded above.
    fn list_sessions(&self) -> Option<SessionListing> {
        let sessions_dir = Self::sessions_dir()?;

        let mut listing = SessionListing::default();
        // Three directories and the file: the exact reach of Codex's own scan.
        for entry in WalkDir::new(&sessions_dir).max_depth(4) {
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

            // One read answers both questions the listing has about this file.
            // A compressed rollout answers neither — see
            // `reject_compressed_rollout`; it is listed on its filename and
            // reported when the reader gets to it.
            let meta = session_meta_payload(path);
            if meta.as_ref().is_some_and(is_subagent_rollout) {
                trace!(
                    path = %path.display(),
                    "skipping Codex subagent rollout: `codex resume` does not offer it"
                );
                continue;
            }

            // Prefer authoritative ID from session_meta payload; otherwise
            // retain the filename for best-effort diagnostics.
            let session_id = meta
                .as_ref()
                .and_then(|payload| payload.get("id"))
                .and_then(|id| id.as_str())
                .map_or_else(|| rollout_stem(name).to_string(), ToString::to_string);
            listing.sessions.push((session_id, path.to_path_buf()));
        }

        Some(listing)
    }

    /// A Codex rollout is
    /// `<CODEX_HOME>/<sessions|archived_sessions>/<y>/<m>/<d>/rollout-*.jsonl[.zst]`.
    ///
    /// All three halves of that are the artifact's own rule and all three were
    /// measured; the module comment records what was planted and what came
    /// back. The root is resolved rather than matched by name, which is what
    /// separates `<home>/sessions/2026/07/28/` from
    /// `<home>/sessions/archived_sessions/2026/07/28/` — the second is three
    /// levels under a directory *called* `archived_sessions` and four levels
    /// under `sessions/`, and 0.145.0 lists neither it nor anything else at
    /// that depth. Naming the root also stops a rollout someone copied to
    /// `~/Downloads/sessions/2026/07/28/` being claimed as a live session.
    fn is_session_path(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_rollout_file_name)
            && is_rollout_layout(path, &Self::rollout_root_paths())
    }

    /// Resolve a named session, including ones `list` deliberately withholds.
    ///
    /// Listing answers "what is there"; this answers "where is the one I
    /// named", and the two have different right answers. An archived rollout,
    /// a subagent rollout and a legacy `rollout-*.json` are all sessions the
    /// user can point at by id, and refusing to resolve them would turn an
    /// exclusion from the picker into data loss. Only a compressed rollout is
    /// resolved-and-then-refused, by the reader rather than here, so that the
    /// refusal names the file.
    fn owns_session(&self, session_id: &str) -> Option<PathBuf> {
        let roots = Self::rollout_roots();
        if roots.is_empty() {
            return None;
        }

        // Codex session IDs can be:
        // 1. A UUID embedded in the file content
        // 2. A relative path like "2026/02/06/rollout-1"
        //
        // Strategy: check if session_id is a relative path first,
        // then scan files for matching UUIDs.

        // Try as relative path (with or without extension).
        //
        // `is_relative` is load-bearing, not a formality. `join` discards the
        // receiver when handed an absolute path, so without it any absolute
        // path at all came back out of here as a "Codex session" — including
        // Claude Code transcripts, which then got parsed by the Codex reader.
        // The registry refuses such an argument before this is reached; the
        // check is repeated because this is the branch that deliberately reads
        // the identifier as a path, and it should say which paths it means.
        if Path::new(session_id).is_relative() {
            for root in &roots {
                let as_path = root.join(session_id);
                for suffix in ["", ".jsonl", ROLLOUT_ZST_SUFFIX, ".json"] {
                    // Appended, not `with_extension`: the compressed form has
                    // two extensions and `with_extension` would replace the
                    // first rather than add the second.
                    let mut name = as_path.clone().into_os_string();
                    name.push(suffix);
                    let candidate = PathBuf::from(name);
                    if candidate.is_file() {
                        debug!(path = %candidate.display(), "found Codex session by path");
                        return Some(candidate);
                    }
                }
            }
        }

        // Scan rollout files recursively.
        for root in &roots {
            for entry in WalkDir::new(root).max_depth(4).into_iter().flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // Wider than `is_session_path` on purpose: the legacy
                // whole-file `.json` form is not something Codex lists any
                // more, but a user who still has one can still name it.
                if !name.starts_with("rollout-")
                    || !(name.ends_with(".jsonl")
                        || name.ends_with(ROLLOUT_ZST_SUFFIX)
                        || name.ends_with(".json"))
                    || !path.is_file()
                {
                    continue;
                }

                // Check if the relative path (minus extension) matches session_id.
                if let Ok(rel) = path.strip_prefix(root)
                    && let Some(parent) = rel.parent()
                    && parent.join(rollout_stem(name)).to_string_lossy() == session_id
                {
                    debug!(path = %path.display(), "found Codex session");
                    return Some(path.to_path_buf());
                }

                // Match by UUID suffix embedded in rollout filename:
                // rollout-YYYY-MM-DDThh-mm-ss-<session-id>.jsonl
                if rollout_stem(name).ends_with(session_id) {
                    debug!(path = %path.display(), "found Codex session by filename suffix");
                    return Some(path.to_path_buf());
                }

                // Fallback: inspect `session_meta.payload.id` in file body.
                if session_meta_id(path).as_deref() == Some(session_id) {
                    debug!(path = %path.display(), "found Codex session by session_meta payload.id");
                    return Some(path.to_path_buf());
                }
            }
        }
        None
    }

    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession> {
        debug!(path = %path.display(), "reading Codex session");

        reject_compressed_rollout(path)?;

        // Try JSONL first, fall back to legacy JSON.
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        // Detect format: if first non-whitespace char is '{' and the file has
        // multiple JSON lines, it's JSONL. If the top-level parse yields a
        // "session" or "items" key, it's legacy JSON.
        let trimmed = content.trim_start();
        if let Some(first_line) = trimmed.lines().next()
            && let Ok(obj) = serde_json::from_str::<serde_json::Value>(first_line)
            && (obj.get("session").is_some() || obj.get("items").is_some())
        {
            return self.read_legacy_json(path, &content);
        }

        self.read_jsonl(path, &content)
    }

    fn write_session(
        &self,
        session: &CanonicalSession,
        opts: &WriteOptions,
    ) -> anyhow::Result<WrittenSession> {
        let target_session_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();

        let sessions_dir = Self::sessions_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Codex sessions directory"))?;
        let target_path = rollout_path(&sessions_dir, &target_session_id, &now);

        debug!(
            target_session_id,
            target_path = %target_path.display(),
            "writing Codex session"
        );

        let mut lines: Vec<String> = Vec::with_capacity(session.messages.len() + 1);

        // 1. session_meta line.
        let cwd = session
            .workspace
            .as_deref()
            .unwrap_or(std::path::Path::new("/tmp"))
            .to_string_lossy()
            .to_string();

        // Current Codex readers deserialize each rollout line's top-level
        // `timestamp` as an RFC3339 *string* (not a numeric epoch). Emit the
        // string form both at the envelope level and inside the payload.
        let now_iso = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        lines.push(serde_json::to_string(&serde_json::json!({
            "type": "session_meta",
            "timestamp": now_iso,
            "payload": {
                // Codex indexes threads by `id`; recent builds also read
                // `session_id`. Emit both so discovery works across versions.
                "id": target_session_id,
                "session_id": target_session_id,
                "cwd": cwd,
                "timestamp": now_iso,
                "originator": "casr",
                "cli_version": env!("CARGO_PKG_VERSION"),
                "source": "cli",
                "thread_source": "user",
                "model_provider": "openai",
            }
        }))?);

        // 2. Messages. Each envelope carries an RFC3339 string timestamp.
        for msg in &session.messages {
            let msg_iso = msg
                .timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .unwrap_or(now)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

            for event in codex_events_for_message(msg, &msg_iso) {
                lines.push(serde_json::to_string(&event)?);
            }
        }

        let content_bytes = lines.join("\n").into_bytes();

        let outcome =
            crate::pipeline::atomic_write(&target_path, &content_bytes, opts.force, self.slug())?;

        info!(
            target_session_id,
            path = %outcome.target_path.display(),
            messages = session.messages.len(),
            "Codex session written"
        );

        // 3. Register the session in Codex's thread index so `codex resume <id>`
        //    can discover it. Codex does not resume from a bare JSONL file — it
        //    looks the id up in `~/.codex/state_*.sqlite` (`threads` table).
        //    Failure here is non-fatal: the rollout file is already written.
        let first_user = session
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let title = session
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                let t = truncate_title(first_user, 100);
                if t.is_empty() {
                    "Resumed session (via casr)".to_string()
                } else {
                    t
                }
            });
        let first_user_message = first_user.to_string();
        let preview = if first_user_message.trim().is_empty() {
            title.clone()
        } else {
            first_user_message.clone()
        };
        let warnings = Self::register_thread(
            &target_session_id,
            &outcome.target_path,
            &cwd,
            &title,
            &first_user_message,
            &preview,
            &now,
        );

        Ok(WrittenSession {
            paths: vec![outcome.target_path.clone()],
            session_id: target_session_id.clone(),
            resume_command: self.resume_command(&target_session_id),
            backups: outcome.displaced().into_iter().collect(),
            warnings,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("codex resume {session_id}")
    }

    /// Codex is on the high-fidelity track; see [`super::codex_ir`].
    fn read_session_ir(&self, path: &Path) -> anyhow::Result<Option<SessionIr>> {
        reject_compressed_rollout(path)?;
        super::codex_ir::read(path).map(Some)
    }

    fn supports_structured_read(&self) -> bool {
        true
    }

    fn supports_structured_write(&self) -> bool {
        true
    }

    /// Write the structured IR as a native rollout; see [`super::codex_ir_write`].
    ///
    /// Placement, naming, atomic write and thread-index registration are the
    /// flat writer's, unchanged — the only thing this path does differently is
    /// what goes *in* the file. `Ok(None)` when the replay is empty, so the
    /// caller falls back to the flat projection rather than registering a
    /// thread that resumes into nothing.
    ///
    /// The grade half of this is [`Provider::grade_session_ir`], which stops
    /// after the render below; a dry run takes that and nothing else.
    fn write_session_ir(
        &self,
        ir: &SessionIr,
        opts: &WriteOptions,
        budget: &crate::budget::ContextBudget,
    ) -> anyhow::Result<Option<StructuredWrite>> {
        let target_session_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let Some(rendered) = super::codex_ir_write::render(ir, &target_session_id, now, budget)
        else {
            debug!("structured Codex write skipped: the replay is empty");
            return Ok(None);
        };

        let sessions_dir = Self::sessions_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Codex sessions directory"))?;
        let target_path = rollout_path(&sessions_dir, &target_session_id, &now);
        let cwd = ir
            .workspace
            .cwd
            .as_deref()
            .unwrap_or(std::path::Path::new("/tmp"))
            .to_string_lossy()
            .to_string();

        debug!(
            target_session_id,
            target_path = %target_path.display(),
            events = rendered.lines.len(),
            "writing structured Codex session"
        );

        // Codex appends new turns to the rollout on resume; without a trailing
        // newline its first appended record lands on casr's last line.
        let mut content = rendered.lines.join("\n");
        content.push('\n');
        let outcome = crate::pipeline::atomic_write(
            &target_path,
            content.as_bytes(),
            opts.force,
            self.slug(),
        )?;

        info!(
            target_session_id,
            path = %outcome.target_path.display(),
            fidelity = ?rendered.fidelity,
            "structured Codex session written"
        );

        let title = {
            let title = truncate_title(&rendered.first_user_text, 100);
            if title.is_empty() {
                "Resumed session (via casr)".to_string()
            } else {
                title
            }
        };
        let preview = if rendered.first_user_text.trim().is_empty() {
            title.clone()
        } else {
            rendered.first_user_text.clone()
        };
        let mut warnings = rendered.warnings;
        warnings.extend(Self::register_thread(
            &target_session_id,
            &outcome.target_path,
            &cwd,
            &title,
            &rendered.first_user_text,
            &preview,
            &now,
        ));

        Ok(Some(StructuredWrite {
            written: WrittenSession {
                paths: vec![outcome.target_path.clone()],
                session_id: target_session_id.clone(),
                resume_command: self.resume_command(&target_session_id),
                backups: outcome.displaced().into_iter().collect(),
                warnings,
            },
            fidelity: rendered.fidelity,
            losses: rendered.losses,
        }))
    }

    /// The first half of `write_session_ir`, stopped before the file is placed.
    ///
    /// Neither the session id nor the timestamp reaches `fidelity` or `losses`
    /// — they only fill envelope fields — so the placeholders below cost the
    /// answer nothing, and nothing here touches the filesystem or the thread
    /// index.
    fn grade_session_ir(
        &self,
        ir: &SessionIr,
        budget: &crate::budget::ContextBudget,
    ) -> anyhow::Result<Option<(crate::ir::Fidelity, Vec<crate::ir::Loss>)>> {
        let Some(rendered) =
            super::codex_ir_write::render(ir, "dry-run", chrono::Utc::now(), budget)
        else {
            return Ok(None);
        };
        Ok(Some((rendered.fidelity, rendered.losses)))
    }
}

// ---------------------------------------------------------------------------
// Codex thread-index (state_*.sqlite) registration
//
// `codex resume <id>` does NOT scan rollout JSONL files. It looks the id up in
// `~/.codex/state_*.sqlite`, table `threads`, whose `rollout_path` column
// points back at the rollout file. Writing the JSONL alone leaves the session
// undiscoverable ("No saved session found with ID"). We therefore register the
// converted session by upserting a `threads` row after the rollout is written.
//
// Safety posture (this mutates a live Codex DB):
//   * Introspect the actual `threads` schema; only write columns that exist.
//   * Never modify or delete rows for any other session id — the id is a fresh
//     UUIDv4, so the upsert only ever touches our own new row.
//   * Refuse to write (degrade with a warning) if the schema has a required
//     column we cannot populate, or if the state DB / `threads` table is absent.
//   * All writes run inside a single transaction with a busy timeout.
// ---------------------------------------------------------------------------

impl Codex {
    /// Locate the newest Codex thread-index database under `CODEX_HOME`/`~/.codex`.
    ///
    /// Matches `state.sqlite` and `state_<N>.sqlite`, preferring the highest
    /// `<N>` (the current schema). Sidecar `-wal`/`-shm` files are ignored.
    fn latest_state_db() -> Option<PathBuf> {
        let home = Self::home_dir()?;
        let mut best: Option<(i64, PathBuf)> = None;
        for entry in std::fs::read_dir(&home).ok()?.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(version) = state_db_version(name) {
                let replace = best.as_ref().is_none_or(|(v, _)| version > *v);
                if replace {
                    best = Some((version, path));
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// Register the converted session in Codex's thread index.
    ///
    /// Returns any non-fatal warnings to surface to the user (empty on success).
    /// A rollout file that could not be registered is still on disk, so the
    /// warning includes that path as a fallback.
    fn register_thread(
        session_id: &str,
        rollout_path: &Path,
        cwd: &str,
        title: &str,
        first_user_message: &str,
        preview: &str,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Vec<String> {
        match register_thread_required(
            session_id,
            rollout_path,
            cwd,
            title,
            first_user_message,
            preview,
            now,
            None,
        ) {
            Ok(()) => {
                debug!(session_id, "registered Codex thread");
                Vec::new()
            }
            Err(e) => {
                warn!(error = %e, "failed to register Codex thread");
                vec![format!(
                    "Could not register the session in the Codex thread index \
                     ({e}). `codex resume {session_id}` may report \
                     \"No saved session found\"; the rollout file is at {path}.",
                    path = rollout_path.display(),
                )]
            }
        }
    }
}

/// Register a checkpoint-restored rollout in the live Codex thread index.
///
/// Unlike the normal conversion path, failure is fatal: AGS calls this before
/// completing its restore transaction and rolls the new rollout back.
pub fn register_restored_thread(
    session_id: &str,
    rollout_path: &Path,
    cwd: &Path,
) -> anyhow::Result<()> {
    let sessions_dir =
        Codex::sessions_dir().ok_or_else(|| anyhow::anyhow!("cannot determine Codex home"))?;
    let sessions_dir = sessions_dir
        .canonicalize()
        .with_context(|| format!("resolve Codex sessions root {}", sessions_dir.display()))?;
    let rollout_path = rollout_path
        .canonicalize()
        .with_context(|| format!("resolve restored rollout {}", rollout_path.display()))?;
    anyhow::ensure!(
        rollout_path.starts_with(&sessions_dir),
        "restored rollout is outside the Codex sessions root"
    );

    let session = <Codex as Provider>::read_session(&Codex, &rollout_path)?;
    anyhow::ensure!(
        session.session_id == session_id,
        "restored rollout identity does not match {session_id}"
    );
    let first_user = session
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.as_str())
        .unwrap_or("");
    let title = session
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            let title = truncate_title(first_user, 100);
            if title.is_empty() {
                "Restored checkpoint (via agsx)".to_string()
            } else {
                title
            }
        });
    let preview = if first_user.trim().is_empty() {
        title.as_str()
    } else {
        first_user
    };
    let model_provider = session_meta_payload(&rollout_path)
        .and_then(|payload| {
            payload
                .get("model_provider")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("restored rollout has no model_provider"))?;

    register_thread_required(
        session_id,
        &rollout_path,
        &cwd.to_string_lossy(),
        &title,
        first_user,
        preview,
        &chrono::Utc::now(),
        Some(&model_provider),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the Codex thread row fields passed to the schema-aware writer"
)]
fn register_thread_required(
    session_id: &str,
    rollout_path: &Path,
    cwd: &str,
    title: &str,
    first_user_message: &str,
    preview: &str,
    now: &chrono::DateTime<chrono::Utc>,
    model_provider: Option<&str>,
) -> anyhow::Result<()> {
    let db_path = Codex::latest_state_db()
        .ok_or_else(|| anyhow::anyhow!("Codex thread index (~/.codex/state_*.sqlite) not found"))?;
    register_thread_in_db(
        &db_path,
        session_id,
        rollout_path,
        cwd,
        title,
        first_user_message,
        preview,
        now,
        model_provider,
    )
}

/// Parse the schema version from a Codex state DB filename.
///
/// `state.sqlite` → 0, `state_5.sqlite` → 5. Returns `None` for anything else
/// (including the `-wal`/`-shm` sidecars).
fn state_db_version(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".sqlite")?;
    if stem == "state" {
        return Some(0);
    }
    stem.strip_prefix("state_")?.parse::<i64>().ok()
}

/// Introspected metadata for one `threads` column.
struct ColInfo {
    notnull: bool,
    has_default: bool,
}

/// Read `PRAGMA table_info(threads)` into a name → [`ColInfo`] map.
/// Returns an empty map if the table does not exist.
fn introspect_threads(conn: &Connection) -> anyhow::Result<HashMap<String, ColInfo>> {
    let mut map = HashMap::new();
    let Ok(mut stmt) = conn.prepare("PRAGMA table_info(threads)") else {
        return Ok(map);
    };
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        let notnull: i64 = row.get(3)?;
        let dflt: Option<String> = row.get(4)?;
        Ok((
            name,
            ColInfo {
                notnull: notnull != 0,
                has_default: dflt.is_some(),
            },
        ))
    })?;
    for row in rows {
        let (name, info) = row?;
        map.insert(name, info);
    }
    Ok(map)
}

/// Environment-shaped column defaults copied from an existing (Codex-authored)
/// `threads` row, so values like `sandbox_policy` are guaranteed to be ones
/// Codex itself wrote and can parse back. Falls back to conservative literals.
struct EnvTemplate {
    source: String,
    model_provider: String,
    sandbox_policy: String,
    approval_mode: String,
    memory_mode: String,
    cli_version: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

fn read_env_template(conn: &Connection, cols: &HashMap<String, ColInfo>) -> EnvTemplate {
    let mut t = EnvTemplate {
        source: "cli".to_string(),
        model_provider: "openai".to_string(),
        sandbox_policy: r#"{"type":"read-only"}"#.to_string(),
        approval_mode: "on-request".to_string(),
        memory_mode: "enabled".to_string(),
        cli_version: None,
        model: None,
        reasoning_effort: None,
    };

    // Prefer real values from the most recent normal user session.
    let wanted = [
        "source",
        "model_provider",
        "sandbox_policy",
        "approval_mode",
        "memory_mode",
        "cli_version",
        "model",
        "reasoning_effort",
    ];
    let sel: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|c| cols.contains_key(*c))
        .collect();
    if sel.is_empty() {
        return t;
    }
    let where_clause = if cols.contains_key("thread_source") {
        "WHERE thread_source = 'user' OR thread_source IS NULL"
    } else {
        ""
    };
    let order = if cols.contains_key("updated_at") {
        "ORDER BY updated_at DESC, rowid DESC"
    } else {
        "ORDER BY rowid DESC"
    };
    let sql = format!(
        "SELECT {} FROM threads {} {} LIMIT 1",
        sel.join(", "),
        where_clause,
        order
    );

    let got = conn.query_row(&sql, [], |row| {
        let mut vals: Vec<Option<String>> = Vec::with_capacity(sel.len());
        for i in 0..sel.len() {
            vals.push(row.get::<_, Option<String>>(i)?);
        }
        Ok(vals)
    });

    if let Ok(vals) = got {
        for (name, val) in sel.iter().zip(vals) {
            let Some(v) = val else { continue };
            if v.is_empty() {
                continue;
            }
            match *name {
                "source" => t.source = v,
                "model_provider" => t.model_provider = v,
                "sandbox_policy" => t.sandbox_policy = v,
                "approval_mode" => t.approval_mode = v,
                "memory_mode" => t.memory_mode = v,
                "cli_version" => t.cli_version = Some(v),
                "model" => t.model = Some(v),
                "reasoning_effort" => t.reasoning_effort = Some(v),
                _ => {}
            }
        }
    }
    t
}

/// Upsert one `threads` row for the converted session. See module comment above
/// for the safety posture. Only ever touches the row keyed by `session_id`.
#[expect(
    clippy::too_many_arguments,
    reason = "maps the several thread columns Codex needs; a struct would not add clarity"
)]
fn register_thread_in_db(
    db_path: &Path,
    session_id: &str,
    rollout_path: &Path,
    cwd: &str,
    title: &str,
    first_user_message: &str,
    preview: &str,
    now: &chrono::DateTime<chrono::Utc>,
    model_provider: Option<&str>,
) -> anyhow::Result<()> {
    let mut conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open Codex state DB {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let cols = introspect_threads(&conn)?;
    if cols.is_empty() {
        anyhow::bail!("`threads` table is absent (unrecognized Codex schema)");
    }

    let mut env = read_env_template(&conn, &cols);
    if let Some(model_provider) = model_provider {
        env.model_provider = model_provider.to_string();
    }

    // Absolute rollout path (Codex resolves the rollout by this exact value).
    let abs = if rollout_path.is_absolute() {
        rollout_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(rollout_path))
            .unwrap_or_else(|_| rollout_path.to_path_buf())
    };
    let abs_str = abs.to_string_lossy().to_string();
    let created = now.timestamp();
    let created_ms = now.timestamp_millis();

    // Desired (column, value) pairs. Filtered to columns that actually exist so
    // the write is resilient across Codex schema versions.
    let mut desired: Vec<(&str, Value)> = vec![
        ("id", Value::Text(session_id.to_string())),
        ("rollout_path", Value::Text(abs_str.clone())),
        ("created_at", Value::Integer(created)),
        ("updated_at", Value::Integer(created)),
        ("created_at_ms", Value::Integer(created_ms)),
        ("updated_at_ms", Value::Integer(created_ms)),
        ("recency_at", Value::Integer(created)),
        ("recency_at_ms", Value::Integer(created_ms)),
        ("source", Value::Text(env.source)),
        ("model_provider", Value::Text(env.model_provider)),
        ("cwd", Value::Text(cwd.to_string())),
        ("title", Value::Text(title.to_string())),
        ("sandbox_policy", Value::Text(env.sandbox_policy)),
        ("approval_mode", Value::Text(env.approval_mode)),
        ("memory_mode", Value::Text(env.memory_mode)),
        (
            "first_user_message",
            Value::Text(first_user_message.to_string()),
        ),
        ("preview", Value::Text(preview.to_string())),
        ("thread_source", Value::Text("user".to_string())),
        ("has_user_event", Value::Integer(1)),
        (
            "cli_version",
            Value::Text(env.cli_version.unwrap_or_default()),
        ),
    ];
    // Optional Codex-native model metadata, only when we have a real value.
    if let Some(m) = env.model {
        desired.push(("model", Value::Text(m)));
    }
    if let Some(r) = env.reasoning_effort {
        desired.push(("reasoning_effort", Value::Text(r)));
    }

    let present: Vec<(&str, Value)> = desired
        .into_iter()
        .filter(|(c, _)| cols.contains_key(*c))
        .collect();
    let provided: std::collections::HashSet<&str> = present.iter().map(|(c, _)| *c).collect();

    // Defensive: refuse to insert if the schema has a NOT NULL column with no
    // default that we do not populate (an unknown/incompatible schema version).
    let mut missing: Vec<&str> = cols
        .iter()
        .filter(|(name, info)| {
            info.notnull && !info.has_default && !provided.contains(name.as_str())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !missing.is_empty() {
        missing.sort_unstable();
        anyhow::bail!(
            "Codex `threads` schema requires column(s) casr cannot populate: {}",
            missing.join(", ")
        );
    }

    let col_names: Vec<&str> = present.iter().map(|(c, _)| *c).collect();
    let placeholders: Vec<String> = (1..=col_names.len()).map(|i| format!("?{i}")).collect();
    // Preserve original creation columns on conflict; refresh the rest.
    let update_set: Vec<String> = col_names
        .iter()
        .filter(|c| !matches!(**c, "id" | "created_at" | "created_at_ms"))
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    let conflict = if update_set.is_empty() {
        "ON CONFLICT(id) DO NOTHING".to_string()
    } else {
        format!("ON CONFLICT(id) DO UPDATE SET {}", update_set.join(", "))
    };
    let sql = format!(
        "INSERT INTO threads ({}) VALUES ({}) {}",
        col_names.join(", "),
        placeholders.join(", "),
        conflict,
    );
    let params: Vec<Value> = present.into_iter().map(|(_, v)| v).collect();

    let tx = conn.transaction().context("begin transaction")?;
    tx.execute(&sql, rusqlite::params_from_iter(params.iter()))
        .context("insert thread row")?;
    tx.commit().context("commit thread row")?;

    // Verify the row landed and points at our rollout file.
    let ok = conn
        .query_row(
            "SELECT 1 FROM threads WHERE id = ?1 AND rollout_path = ?2",
            rusqlite::params![session_id, abs_str],
            |_| Ok(true),
        )
        .optional()
        .context("verify thread row")?
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("post-write verification failed: thread row not found for {session_id}");
    }
    Ok(())
}

/// Build the Codex JSONL event(s) for one canonical message.
///
/// `msg_ts` is the event timestamp as an RFC3339 string, matching the
/// top-level `timestamp` format current Codex readers expect.
fn codex_events_for_message(msg: &CanonicalMessage, msg_ts: &str) -> Vec<serde_json::Value> {
    // User messages that carry tool payloads must be serialized as response_item
    // envelopes; event_msg/user_message cannot represent tool_use/tool_result blocks.
    let user_needs_response_item = msg.role == MessageRole::User
        && (!msg.tool_calls.is_empty() || !msg.tool_results.is_empty());

    match msg.role {
        MessageRole::User if !user_needs_response_item => vec![serde_json::json!({
            "type": "event_msg",
            "timestamp": msg_ts,
            "payload": {
                "type": "user_message",
                "message": msg.content,
            }
        })],
        MessageRole::User => vec![serde_json::json!({
            "type": "response_item",
            "timestamp": msg_ts,
            "payload": {
                "type": "message",
                "role": codex_role_string(&msg.role),
                "content": codex_response_content(msg),
            }
        })],
        MessageRole::Assistant if msg.author.as_deref() == Some("reasoning") => {
            vec![serde_json::json!({
                "type": "event_msg",
                "timestamp": msg_ts,
                "payload": {
                    "type": "agent_reasoning",
                    "text": msg.content,
                }
            })]
        }
        MessageRole::Assistant
        | MessageRole::Tool
        | MessageRole::System
        | MessageRole::Other(_) => {
            let mut events = vec![serde_json::json!({
                "type": "response_item",
                "timestamp": msg_ts,
                "payload": {
                    "type": "message",
                    "role": codex_role_string(&msg.role),
                    "content": codex_response_content(msg),
                }
            })];

            if let Some(info) = codex_token_count_info(&msg.extra) {
                events.push(serde_json::json!({
                    "type": "event_msg",
                    "timestamp": msg_ts,
                    "payload": {
                        "type": "token_count",
                        "info": info,
                    }
                }));
            }

            events
        }
    }
}

fn codex_role_string(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "assistant".to_string(),
        MessageRole::Tool => "tool".to_string(),
        MessageRole::System => "developer".to_string(),
        MessageRole::Other(other) => other.clone(),
    }
}

fn codex_response_content(msg: &CanonicalMessage) -> serde_json::Value {
    let mut blocks: Vec<serde_json::Value> = Vec::new();

    // Codex expects "output_text" for assistant-generated content blocks,
    // "input_text" for user-supplied content blocks.
    let text_type = if msg.role == MessageRole::Assistant {
        "output_text"
    } else {
        "input_text"
    };

    if !msg.content.is_empty() {
        blocks.push(serde_json::json!({
            "type": text_type,
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

    // Avoid empty response payloads in provider-native output.
    if blocks.is_empty() {
        blocks.push(serde_json::json!({
            "type": text_type,
            "text": msg.content,
        }));
    }

    serde_json::Value::Array(blocks)
}

fn codex_token_count_info(extra: &serde_json::Value) -> Option<serde_json::Value> {
    let mut sources: Vec<&serde_json::Value> = Vec::new();
    sources.push(extra);
    if let Some(payload) = extra.get("payload") {
        sources.push(payload);
    }

    let mut candidates: Vec<&serde_json::Value> = Vec::new();
    for source in sources {
        if let Some(usage) = source.get("usage") {
            candidates.push(usage);
        }
        if let Some(token_count) = source.get("token_count") {
            if let Some(info) = token_count.get("info") {
                candidates.push(info);
            }
            candidates.push(token_count);
        }
        candidates.push(source);
    }

    for candidate in candidates {
        let Some(obj) = candidate.as_object() else {
            continue;
        };

        let mut info = serde_json::Map::new();
        insert_token_count(&mut info, obj, "input_tokens", "inputTokens");
        insert_token_count(&mut info, obj, "output_tokens", "outputTokens");
        insert_token_count(&mut info, obj, "total_tokens", "totalTokens");
        insert_token_count(&mut info, obj, "cached_input_tokens", "cachedInputTokens");
        insert_token_count(&mut info, obj, "reasoning_tokens", "reasoningTokens");

        if !info.is_empty() {
            return Some(serde_json::Value::Object(info));
        }
    }

    None
}

fn insert_token_count(
    out: &mut serde_json::Map<String, serde_json::Value>,
    obj: &serde_json::Map<String, serde_json::Value>,
    snake: &str,
    camel: &str,
) {
    if let Some(value) = obj.get(snake).or_else(|| obj.get(camel))
        && let Some(num) = token_count_number(value)
    {
        out.insert(snake.to_string(), serde_json::Value::Number(num.into()));
    }
}

fn token_count_number(value: &serde_json::Value) -> Option<i64> {
    if let Some(i) = value.as_i64() {
        return Some(i);
    }
    if let Some(u) = value.as_u64() {
        return i64::try_from(u).ok();
    }
    value.as_str().and_then(|s| s.parse::<i64>().ok())
}

// ---------------------------------------------------------------------------
// JSONL / legacy JSON parsing
// ---------------------------------------------------------------------------

impl Codex {
    /// Parse modern JSONL envelope format.
    fn read_jsonl(&self, path: &Path, content: &str) -> anyhow::Result<CanonicalSession> {
        let reader = BufReader::new(content.as_bytes());

        let mut session_id: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        let mut messages: Vec<CanonicalMessage> = Vec::new();
        let mut skipped: usize = 0;
        let mut line_num: usize = 0;

        for line_result in reader.lines() {
            line_num += 1;
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    warn!(line = line_num, error = %e, "skipping unreadable line");
                    skipped += 1;
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let envelope: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!(line = line_num, error = %e, "skipping malformed JSON line");
                    skipped += 1;
                    continue;
                }
            };

            let event_type = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let payload = envelope.get("payload");

            // Extract timestamp from envelope level.
            let ts = envelope.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                started_at = Some(started_at.map_or(t, |s: i64| s.min(t)));
                ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
            }

            match event_type {
                "session_meta" => {
                    if let Some(p) = payload {
                        if session_id.is_none() {
                            session_id = p.get("id").and_then(|v| v.as_str()).map(String::from);
                        }
                        if workspace.is_none() {
                            workspace = p.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                        }
                    }
                }
                "response_item" => {
                    if let Some(p) = payload {
                        // `function_call_output` / `custom_tool_call_output` events
                        // carry no `role` field and would otherwise default to
                        // "assistant". The Anthropic API (and Claude Code resume)
                        // require tool results to live in *user* turns, so we
                        // classify them as Tool — target writers map Tool → user side.
                        let payload_type =
                            p.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                        let role = if matches!(
                            payload_type,
                            "function_call_output" | "custom_tool_call_output"
                        ) {
                            MessageRole::Tool
                        } else {
                            let role_str = p
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("assistant");
                            normalize_role(role_str)
                        };

                        let content_val = p.get("content");
                        let text = codex_extract_text_content(content_val);
                        let mut tool_calls = codex_extract_tool_calls(content_val);
                        tool_calls.extend(codex_extract_payload_tool_calls(p));
                        let mut tool_results = codex_extract_tool_results(content_val);
                        tool_results.extend(codex_extract_payload_tool_results(p));

                        if text.trim().is_empty()
                            && tool_calls.is_empty()
                            && tool_results.is_empty()
                        {
                            trace!(line = line_num, "skipping empty response_item");
                            continue;
                        }

                        let next_message = CanonicalMessage {
                            idx: 0,
                            role,
                            content: text,
                            timestamp: ts,
                            author: None,
                            tool_calls,
                            tool_results,
                            extra: envelope,
                        };

                        // Some Codex files mirror user turns in both
                        // `response_item(message:user)` and `event_msg(user_message)`.
                        // Drop exact adjacent duplicates to preserve clean alternation.
                        let is_adjacent_user_duplicate = messages.last().is_some_and(|prev| {
                            prev.role == MessageRole::User
                                && next_message.role == MessageRole::User
                                && prev.content == next_message.content
                                && prev.timestamp == next_message.timestamp
                        });
                        if is_adjacent_user_duplicate {
                            trace!(line = line_num, "skipping duplicate user response_item");
                            continue;
                        }

                        messages.push(next_message);
                    }
                }
                "event_msg" => {
                    if let Some(p) = payload {
                        let sub_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match sub_type {
                            "user_message" => {
                                let text = p
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.trim().is_empty() {
                                    let next_message = CanonicalMessage {
                                        idx: 0,
                                        role: MessageRole::User,
                                        content: text,
                                        timestamp: ts,
                                        author: None,
                                        tool_calls: vec![],
                                        tool_results: vec![],
                                        extra: envelope,
                                    };

                                    let is_adjacent_user_duplicate =
                                        messages.last().is_some_and(|prev| {
                                            prev.role == MessageRole::User
                                                && prev.content == next_message.content
                                                && prev.timestamp == next_message.timestamp
                                        });
                                    if is_adjacent_user_duplicate {
                                        trace!(
                                            line = line_num,
                                            "skipping duplicate user event_msg"
                                        );
                                        continue;
                                    }

                                    messages.push(next_message);
                                }
                            }
                            "agent_reasoning" => {
                                let text = p
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.trim().is_empty() {
                                    messages.push(CanonicalMessage {
                                        idx: 0,
                                        role: MessageRole::Assistant,
                                        content: text,
                                        timestamp: ts,
                                        author: Some("reasoning".to_string()),
                                        tool_calls: vec![],
                                        tool_results: vec![],
                                        extra: envelope,
                                    });
                                }
                            }
                            _ => {
                                trace!(
                                    line = line_num,
                                    sub_type, "skipping non-conversational event_msg"
                                );
                            }
                        }
                    }
                }
                "compacted" => {
                    // A compaction event replaces all accumulated history with a
                    // condensed `replacement_history` snapshot — the source
                    // agent's live context at that point. Resetting here means
                    // the converted session mirrors the *live* context rather than
                    // replaying the full on-disk archive (a session can compact
                    // dozens of times; only the final snapshot plus post-compaction
                    // events are actually in context).
                    if let Some(p) = payload {
                        let mut replacement: Vec<CanonicalMessage> = Vec::new();
                        if let Some(items) = p.get("replacement_history").and_then(|v| v.as_array())
                        {
                            for item in items {
                                let item_type = item
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                let role = if matches!(
                                    item_type,
                                    "function_call_output" | "custom_tool_call_output"
                                ) {
                                    MessageRole::Tool
                                } else {
                                    let role_str = item
                                        .get("role")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("assistant");
                                    normalize_role(role_str)
                                };
                                let content_val = item.get("content");
                                let text = codex_extract_text_content(content_val);
                                let mut tool_calls = codex_extract_tool_calls(content_val);
                                tool_calls.extend(codex_extract_payload_tool_calls(item));
                                let mut tool_results = codex_extract_tool_results(content_val);
                                tool_results.extend(codex_extract_payload_tool_results(item));
                                if text.trim().is_empty()
                                    && tool_calls.is_empty()
                                    && tool_results.is_empty()
                                {
                                    continue;
                                }
                                replacement.push(CanonicalMessage {
                                    idx: 0,
                                    role,
                                    content: text,
                                    timestamp: ts,
                                    author: None,
                                    tool_calls,
                                    tool_results,
                                    extra: serde_json::Value::Null,
                                });
                            }
                        }
                        // An optional free-text summary accompanying the compaction.
                        if let Some(summary) = p.get("message").and_then(|v| v.as_str())
                            && !summary.trim().is_empty()
                        {
                            replacement.push(CanonicalMessage {
                                idx: 0,
                                role: MessageRole::Assistant,
                                content: summary.to_string(),
                                timestamp: ts,
                                author: Some("summary".to_string()),
                                tool_calls: vec![],
                                tool_results: vec![],
                                extra: serde_json::Value::Null,
                            });
                        }
                        debug!(
                            line = line_num,
                            replaced = messages.len(),
                            kept = replacement.len(),
                            "codex compaction: resetting history to replacement_history"
                        );
                        messages = replacement;
                    }
                }
                _ => {
                    trace!(line = line_num, event_type, "skipping unknown event type");
                }
            }
        }

        reindex_messages(&mut messages);
        self.build_session(
            path, session_id, workspace, started_at, ended_at, messages, skipped,
        )
    }

    /// Parse legacy single-JSON format: `{ "session": {…}, "items": […] }`.
    fn read_legacy_json(&self, path: &Path, content: &str) -> anyhow::Result<CanonicalSession> {
        let root: serde_json::Value = serde_json::from_str(content)
            .with_context(|| format!("failed to parse legacy JSON {}", path.display()))?;

        let session_obj = root.get("session");
        let session_id = session_obj
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let workspace = session_obj
            .and_then(|s| s.get("cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let items = root
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut messages = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        for item in &items {
            let role_str = item
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("assistant");
            let role = normalize_role(role_str);

            let text = item.get("content").map(flatten_content).unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }

            let ts = item.get("timestamp").and_then(parse_timestamp);
            if let Some(t) = ts {
                started_at = Some(started_at.map_or(t, |s: i64| s.min(t)));
                ended_at = Some(ended_at.map_or(t, |e: i64| e.max(t)));
            }

            messages.push(CanonicalMessage {
                idx: 0,
                role,
                content: text,
                timestamp: ts,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: item.clone(),
            });
        }

        reindex_messages(&mut messages);
        self.build_session(
            path, session_id, workspace, started_at, ended_at, messages, 0,
        )
    }

    /// Assemble the final `CanonicalSession` from parsed data.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal builder; clarity > refactoring"
    )]
    fn build_session(
        &self,
        path: &Path,
        session_id: Option<String>,
        workspace: Option<PathBuf>,
        started_at: Option<i64>,
        ended_at: Option<i64>,
        messages: Vec<CanonicalMessage>,
        skipped: usize,
    ) -> anyhow::Result<CanonicalSession> {
        // Derive session ID from relative path if not in content. Both roots,
        // because an archived rollout is still a session with an id, and
        // `sessions/`-only left it named after its bare filename instead.
        let session_id = session_id.unwrap_or_else(|| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            for root in [Self::sessions_dir(), Self::archived_sessions_dir()]
                .into_iter()
                .flatten()
            {
                if let Ok(rel) = path.strip_prefix(&root)
                    && let Some(parent) = rel.parent()
                {
                    return parent
                        .join(rollout_stem(name))
                        .to_string_lossy()
                        .to_string();
                }
            }
            match rollout_stem(name) {
                "" => "unknown".to_string(),
                stem => stem.to_string(),
            }
        });

        let title = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| truncate_title(&m.content, 100));

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "source".into(),
            serde_json::Value::String("codex".to_string()),
        );

        debug!(
            session_id,
            messages = messages.len(),
            skipped,
            "Codex session parsed"
        );

        Ok(CanonicalSession {
            session_id,
            provider_slug: "codex".to_string(),
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract only plain assistant/user text from Codex content blocks.
///
/// We intentionally ignore `tool_use` and `tool_result` blocks here because
/// those are parsed into structured `tool_calls` / `tool_results` separately.
/// Including tool blocks in flattened text causes read-back content inflation
/// and spurious verification mismatches.
fn codex_extract_text_content(content: Option<&serde_json::Value>) -> String {
    let Some(value) = content else {
        return String::new();
    };

    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                match block {
                    serde_json::Value::String(s) => parts.push(s.clone()),
                    serde_json::Value::Object(obj) => {
                        let block_type = obj.get("type").and_then(|v| v.as_str());
                        if (matches!(
                            block_type,
                            Some("text") | Some("input_text") | Some("output_text")
                        ) || block_type.is_none())
                            && let Some(text) = obj.get("text").and_then(|v| v.as_str())
                        {
                            parts.push(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        serde_json::Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Extract tool calls from Codex content blocks.
fn codex_extract_tool_calls(content: Option<&serde_json::Value>) -> Vec<ToolCall> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return vec![];
    };
    blocks
        .iter()
        .filter_map(|block| {
            let obj = block.as_object()?;
            if obj.get("type")?.as_str()? != "tool_use" {
                return None;
            }
            Some(ToolCall {
                id: obj.get("id").and_then(|v| v.as_str()).map(String::from),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: obj.get("input").cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}

/// Extract tool results from Codex content blocks.
fn codex_extract_tool_results(content: Option<&serde_json::Value>) -> Vec<ToolResult> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return vec![];
    };
    blocks
        .iter()
        .filter_map(|block| {
            let obj = block.as_object()?;
            if obj.get("type")?.as_str()? != "tool_result" {
                return None;
            }
            Some(ToolResult {
                call_id: obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                content: obj
                    .get("content")
                    .and_then(|v| v.as_str())
                    .or_else(|| obj.get("output").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string(),
                is_error: obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn codex_extract_payload_tool_calls(payload: &serde_json::Value) -> Vec<ToolCall> {
    let payload_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !matches!(payload_type, "function_call" | "custom_tool_call") {
        return vec![];
    }

    let arguments = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("args"))
        .map(codex_parse_arguments_value)
        .unwrap_or(serde_json::Value::Null);

    vec![ToolCall {
        id: payload
            .get("call_id")
            .or_else(|| payload.get("id"))
            .or_else(|| payload.get("tool_use_id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        name: payload
            .get("name")
            .or_else(|| payload.pointer("/function/name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        arguments,
    }]
}

fn codex_extract_payload_tool_results(payload: &serde_json::Value) -> Vec<ToolResult> {
    let payload_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !matches!(
        payload_type,
        "function_call_output" | "custom_tool_call_output"
    ) {
        return vec![];
    }

    let content = payload
        .get("output")
        .or_else(|| payload.get("content"))
        .or_else(|| payload.get("result"))
        .map(flatten_content)
        .unwrap_or_default();
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            payload
                .get("status")
                .and_then(|v| v.as_str())
                .map(|status| status == "error")
        })
        .unwrap_or(false);

    vec![ToolResult {
        call_id: payload
            .get("call_id")
            .or_else(|| payload.get("tool_use_id"))
            .or_else(|| payload.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        content,
        is_error,
    }]
}

fn codex_parse_arguments_value(value: &serde_json::Value) -> serde_json::Value {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            parsed
        } else {
            serde_json::Value::String(text.to_string())
        }
    } else {
        value.clone()
    }
}

/// Extract the `session_meta` payload from a Codex rollout file.
///
/// `None` for a file with no `session_meta` in its first 64 lines, and for a
/// compressed rollout, whose bytes this cannot read. Both mean "unknown", and
/// callers must not read "unknown" as "no" — [`Codex::list_sessions`] lists a
/// rollout it could not classify rather than dropping it.
fn session_meta_payload(path: &Path) -> Option<serde_json::Value> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(64) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let envelope: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if envelope.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return envelope.get("payload").cloned();
        }
    }
    None
}

/// Extract `session_meta.payload.id` from a Codex rollout file.
fn session_meta_id(path: &Path) -> Option<String> {
    session_meta_payload(path)?
        .get("id")?
        .as_str()
        .map(ToString::to_string)
}

/// Whether this rollout is one `codex resume` will not offer.
///
/// 0.145.0 records what started a thread in `session_meta.payload.source`
/// (`SessionSource`), and `thread/list` filters on the `ThreadSourceKind` it
/// derives from that. Omitting `sourceKinds` "defaults to interactive
/// sources", and a `thread/list` against real rollouts returned the `cli` ones
/// and neither of the two `{"subagent":{"thread_spawn":…}}` ones; asking for
/// `["subAgentThreadSpawn"]` returned exactly those two. The subagent variants
/// the artifact's own `SubAgentSource` enum names are `review`, `compact`,
/// `thread_spawn`, `memory_consolidation` and `other`, so the discriminator is
/// the outer `subagent` tag rather than a list of inner ones that would go
/// stale the next time Codex adds a kind of subagent.
///
/// This cannot be a path rule. A subagent rollout is written into the same
/// `<y>/<m>/<d>` directory, with the same `rollout-` name, as the thread that
/// spawned it — the only thing that distinguishes it is inside the file.
fn is_subagent_rollout(payload: &serde_json::Value) -> bool {
    payload.pointer("/source/subagent").is_some()
}

/// Does this filename name a Codex rollout?
///
/// The `rollout-` prefix plus `.jsonl` or `.jsonl.zst`, and nothing else.
/// `.json` is excluded deliberately: a genuine rollout renamed to
/// `rollout-….json` and planted at the correct depth is not returned by
/// `thread/list`, so listing one would be casr inventing a session. The legacy
/// whole-file `{session, items}` form is still *read* — by
/// [`Codex::read_legacy_json`], reached by content — and still resolved by
/// [`Codex::owns_session`]; it is only the enumeration that no longer claims
/// it. `.ZST` is excluded for the same reason: it was planted and not listed.
fn is_rollout_file_name(name: &str) -> bool {
    name.starts_with("rollout-") && (name.ends_with(".jsonl") || name.ends_with(ROLLOUT_ZST_SUFFIX))
}

/// The identifier-bearing part of a rollout filename.
///
/// [`Path::file_stem`] cannot answer this: for `rollout-x.jsonl.zst` it says
/// `rollout-x.jsonl`, which matches no session id and no relative-path form.
fn rollout_stem(name: &str) -> &str {
    name.strip_suffix(ROLLOUT_ZST_SUFFIX)
        .or_else(|| name.strip_suffix(".jsonl"))
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(name)
}

/// Whether `path` sits where Codex's scanner reaches:
/// `<root>/<u16>/<u8>/<u8>/<file>` for one of `roots`.
///
/// The parses are the rule, not a stand-in for one. Measured against 0.145.0,
/// `2026/7/8`, `+026/07/18`, `0000/00/00`, `2026/255/18` and `65535/07/18` all
/// list, while `65536/07/18`, `70000/07/18`, `2026/256/18`, `2026/07/256`,
/// `-1/07/18`, `2026/0_7/18`, `2026/07/ 18`, `2026/07/18.0`, `a026/07/18` and
/// `2026/07/1a` do not — which is exactly `u16`/`u8`/`u8` through Rust's
/// `FromStr` and is neither a `*/*/*` glob nor a calendar check. Encoding a
/// date check instead would drop `2026/255/18`, a directory the artifact
/// accepts; encoding a bare glob would pick up `aaaa/bb/cc`, one it does not.
fn is_rollout_layout(path: &Path, roots: &[PathBuf]) -> bool {
    let mut ancestors = path.ancestors().skip(1);
    let (Some(day), Some(month), Some(year), Some(root)) = (
        ancestors.next(),
        ancestors.next(),
        ancestors.next(),
        ancestors.next(),
    ) else {
        return false;
    };
    fn component(dir: &Path) -> Option<&str> {
        dir.file_name().and_then(|name| name.to_str())
    }
    roots.iter().any(|candidate| candidate == root)
        && component(year).is_some_and(|c| c.parse::<u16>().is_ok())
        && component(month).is_some_and(|c| c.parse::<u8>().is_ok())
        && component(day).is_some_and(|c| c.parse::<u8>().is_ok())
}

/// Refuse a rollout casr can see but cannot decode, by name.
///
/// 0.145.0 compresses rollouts in place — `rollout/src/compression.rs`, whose
/// outcomes include `compressed`, `skipped_already_compressed` and
/// `plain_exists` — leaving `rollout-….jsonl.zst` where the `.jsonl` was, and
/// `thread/list` goes on listing the thread. casr has no zstd decoder and
/// adding one would be a new C toolchain dependency for a format the shipped
/// corpus this was measured against contains none of.
///
/// So it reports. `cmd_list` turns this `Err` into a `skipped` row naming the
/// path and the reason, which is the #38/#40 bargain this codebase already
/// made: a session the user can see is missing is a bug they can act on, and a
/// session silently absent is one they cannot. Dropping `.jsonl.zst` from
/// [`is_rollout_file_name`] instead would have been the quiet failure.
fn reject_compressed_rollout(path: &Path) -> anyhow::Result<()> {
    let compressed = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(ROLLOUT_ZST_SUFFIX));
    if compressed {
        let display = path.display();
        anyhow::bail!(
            "{display} is a zstd-compressed Codex rollout and casr has no zstd \
             decoder. Decompress it first (`zstd -d {display}`) and convert the \
             resulting .jsonl."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Codex, codex_events_for_message, rollout_path};
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::path::Path;

    use crate::model::{CanonicalMessage, MessageRole, ToolCall, ToolResult};
    use crate::providers::Provider;

    #[test]
    fn rollout_path_includes_date_hierarchy_and_uuid_suffix() {
        let now = Utc
            .with_ymd_and_hms(2026, 2, 9, 6, 7, 8)
            .single()
            .expect("valid timestamp");
        let path = rollout_path(
            Path::new("/tmp/codex/sessions"),
            "019c40fd-3c51-7621-a418-68203585f589",
            &now,
        );
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with(
                "2026/02/09/rollout-2026-02-09T06-07-08-019c40fd-3c51-7621-a418-68203585f589.jsonl"
            ),
            "{path_str}"
        );
    }

    #[test]
    fn assistant_events_include_tool_calls_results_and_token_count() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "Applied the patch".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![ToolCall {
                id: Some("call-1".to_string()),
                name: "apply_patch".to_string(),
                arguments: json!({"path":"src/providers/codex.rs"}),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("call-1".to_string()),
                content: "ok".to_string(),
                is_error: false,
            }],
            extra: json!({
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 22,
                    "total_tokens": 33
                }
            }),
        };

        let events = codex_events_for_message(&msg, "2026-02-09T06:07:08.000Z");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "response_item");
        assert_eq!(events[0]["payload"]["type"], "message");
        let content_blocks = events[0]["payload"]["content"]
            .as_array()
            .expect("response_item content should be array");
        assert!(content_blocks.iter().any(|b| b["type"] == "tool_use"));
        assert!(content_blocks.iter().any(|b| b["type"] == "tool_result"));

        assert_eq!(events[1]["type"], "event_msg");
        assert_eq!(events[1]["payload"]["type"], "token_count");
        assert_eq!(events[1]["payload"]["info"]["input_tokens"], 11);
        assert_eq!(events[1]["payload"]["info"]["output_tokens"], 22);
        assert_eq!(events[1]["payload"]["info"]["total_tokens"], 33);
    }

    #[test]
    fn user_message_with_tool_payload_is_serialized_as_response_item() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: String::new(),
            timestamp: None,
            author: None,
            tool_calls: vec![ToolCall {
                id: Some("call-7".to_string()),
                name: "Read".to_string(),
                arguments: json!({"file_path":"src/main.rs"}),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("call-7".to_string()),
                content: "fn main() {}".to_string(),
                is_error: false,
            }],
            extra: json!({}),
        };

        let events = codex_events_for_message(&msg, "2026-02-09T06:07:08.000Z");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response_item");
        assert_eq!(events[0]["payload"]["type"], "message");
        assert_eq!(events[0]["payload"]["role"], "user");
        let blocks = events[0]["payload"]["content"]
            .as_array()
            .expect("response_item content should be array");
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
        assert!(blocks.iter().any(|b| b["type"] == "tool_result"));
    }

    #[test]
    fn response_item_with_only_tool_result_is_not_dropped() {
        let file_text = serde_json::to_string(&json!({
            "type": "response_item",
            "timestamp": 1700000000.0,
            "payload": {
                "role": "assistant",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call-2",
                    "content": "lint clean",
                    "is_error": false
                }]
            }
        }))
        .expect("serializable test envelope");

        let provider = Codex;
        let session = provider
            .read_jsonl(Path::new("/tmp/rollout-test.jsonl"), &file_text)
            .expect("Codex JSONL reader should parse tool_result-only response_item");

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].tool_results.len(), 1);
        assert_eq!(session.messages[0].tool_results[0].content, "lint clean");
    }

    #[test]
    fn payload_function_call_is_parsed_as_tool_call() {
        let file_text = serde_json::to_string(&json!({
            "type": "response_item",
            "timestamp": 1700000000.0,
            "payload": {
                "type": "function_call",
                "role": "assistant",
                "call_id": "call-42",
                "name": "Read",
                "arguments": "{\"file_path\":\"src/main.rs\"}"
            }
        }))
        .expect("serializable test envelope");

        let provider = Codex;
        let session = provider
            .read_jsonl(Path::new("/tmp/rollout-fc.jsonl"), &file_text)
            .expect("Codex JSONL reader should parse payload-level function_call");

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].tool_calls.len(), 1);
        assert_eq!(session.messages[0].tool_calls[0].name, "Read");
        assert_eq!(
            session.messages[0].tool_calls[0].id.as_deref(),
            Some("call-42")
        );
        assert_eq!(
            session.messages[0].tool_calls[0].arguments["file_path"],
            "src/main.rs"
        );
    }

    #[test]
    fn payload_function_call_output_is_parsed_as_tool_result() {
        let file_text = serde_json::to_string(&json!({
            "type": "response_item",
            "timestamp": 1700000000.0,
            "payload": {
                "type": "function_call_output",
                "role": "assistant",
                "call_id": "call-42",
                "output": "done"
            }
        }))
        .expect("serializable test envelope");

        let provider = Codex;
        let session = provider
            .read_jsonl(Path::new("/tmp/rollout-fco.jsonl"), &file_text)
            .expect("Codex JSONL reader should parse payload-level function_call_output");

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].tool_results.len(), 1);
        assert_eq!(
            session.messages[0].tool_results[0].call_id.as_deref(),
            Some("call-42")
        );
        assert_eq!(session.messages[0].tool_results[0].content, "done");
    }

    #[test]
    fn resume_command_uses_subcommand_form() {
        let provider = Codex;
        assert_eq!(
            <Codex as Provider>::resume_command(&provider, "abc123"),
            "codex resume abc123"
        );
    }

    // -----------------------------------------------------------------------
    // Reader unit tests
    // -----------------------------------------------------------------------

    /// Read Codex JSONL from an inline string.
    fn read_codex_jsonl(content: &str) -> crate::model::CanonicalSession {
        let provider = Codex;
        provider
            .read_jsonl(Path::new("/tmp/test-rollout.jsonl"), content)
            .unwrap_or_else(|e| panic!("read_jsonl failed: {e}"))
    }

    /// Read Codex legacy JSON from an inline string.
    fn read_codex_legacy(content: &str) -> crate::model::CanonicalSession {
        let provider = Codex;
        provider
            .read_legacy_json(Path::new("/tmp/test-legacy.json"), content)
            .unwrap_or_else(|e| panic!("read_legacy_json failed: {e}"))
    }

    #[test]
    fn reader_jsonl_basic_exchange() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"test-001","cwd":"/data/proj"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Hello"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Hi back"}]}}"#,
        );
        assert_eq!(session.session_id, "test-001");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Hello");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Hi back");
        assert_eq!(
            session.workspace,
            Some(std::path::PathBuf::from("/data/proj"))
        );
    }

    #[test]
    fn reader_jsonl_assistant_output_text_is_preserved() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"out-1","cwd":"/tmp"}}
{"type":"response_item","timestamp":1700000001.0,"payload":{"role":"user","content":[{"type":"input_text","text":"Ping"}]}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"output_text","text":"Pong"}]}}"#,
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Ping");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Pong");
    }

    #[test]
    fn reader_jsonl_reasoning_events() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"r1","cwd":"/tmp"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Q"}}
{"type":"event_msg","timestamp":1700000002.0,"payload":{"type":"agent_reasoning","text":"Thinking about it..."}}
{"type":"response_item","timestamp":1700000003.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Answer"}]}}"#,
        );
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].author.as_deref(), Some("reasoning"));
        assert_eq!(session.messages[1].content, "Thinking about it...");
    }

    #[test]
    fn reader_jsonl_skips_non_conversational_events() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"skip1","cwd":"/tmp"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Q"}}
{"type":"event_msg","timestamp":1700000002.0,"payload":{"type":"token_count","info":{"input_tokens":100}}}
{"type":"event_msg","timestamp":1700000003.0,"payload":{"type":"turn_aborted"}}
{"type":"response_item","timestamp":1700000004.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"A"}]}}"#,
        );
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_jsonl_tool_calls_in_response_item() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"tc1","cwd":"/tmp"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Run it"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Running"},{"type":"tool_use","id":"call-1","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        assert_eq!(session.messages[1].content, "Running");
        assert_eq!(session.messages[1].tool_calls.len(), 1);
        assert_eq!(session.messages[1].tool_calls[0].name, "Bash");
    }

    #[test]
    fn reader_jsonl_dedupes_mirrored_user_entries() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"dup-u","cwd":"/tmp"}}
{"type":"response_item","timestamp":1700000001.0,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Same user turn"}]}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Same user turn"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}}"#,
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "Same user turn");
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Answer");
    }

    #[test]
    fn reader_jsonl_tolerates_malformed_lines() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"mf1","cwd":"/tmp"}}
not json
{"broken
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Valid"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Also valid"}]}}"#,
        );
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_jsonl_empty_content_skipped() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"ec1","cwd":"/tmp"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":""}}
{"type":"event_msg","timestamp":1700000002.0,"payload":{"type":"user_message","message":"   "}}
{"type":"event_msg","timestamp":1700000003.0,"payload":{"type":"user_message","message":"Valid"}}
{"type":"response_item","timestamp":1700000004.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Reply"}]}}"#,
        );
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_jsonl_session_id_fallback() {
        let session = read_codex_jsonl(
            r#"{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"No meta"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Reply"}]}}"#,
        );
        // No session_meta → ID falls back to filename stem.
        assert!(!session.session_id.is_empty());
    }

    #[test]
    fn reader_legacy_json_basic() {
        let session = read_codex_legacy(
            r#"{"session":{"id":"legacy-1","cwd":"/home/user/proj"},"items":[
                {"role":"user","content":"Fix the bug","timestamp":1700000000},
                {"role":"assistant","content":"Fixed it","timestamp":1700000010}
            ]}"#,
        );
        assert_eq!(session.session_id, "legacy-1");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(
            session.workspace,
            Some(std::path::PathBuf::from("/home/user/proj"))
        );
        assert!(session.started_at.is_some());
    }

    #[test]
    fn reader_legacy_json_empty_items() {
        let session = read_codex_legacy(r#"{"session":{"id":"empty-1","cwd":"/tmp"},"items":[]}"#);
        assert_eq!(session.messages.len(), 0);
    }

    #[test]
    fn reader_legacy_json_skips_empty_content() {
        let session = read_codex_legacy(
            r#"{"session":{"id":"skip-1","cwd":"/tmp"},"items":[
                {"role":"user","content":"","timestamp":1700000000},
                {"role":"user","content":"Real","timestamp":1700000001},
                {"role":"assistant","content":"Reply","timestamp":1700000002}
            ]}"#,
        );
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn reader_title_from_first_user_message() {
        let session = read_codex_jsonl(
            r#"{"type":"session_meta","timestamp":1700000000.0,"payload":{"id":"t1","cwd":"/tmp"}}
{"type":"event_msg","timestamp":1700000001.0,"payload":{"type":"user_message","message":"Optimize the database query"}}
{"type":"response_item","timestamp":1700000002.0,"payload":{"role":"assistant","content":[{"type":"input_text","text":"Done"}]}}"#,
        );
        assert_eq!(
            session.title.as_deref(),
            Some("Optimize the database query")
        );
    }

    // -----------------------------------------------------------------------
    // Writer helper unit tests
    // -----------------------------------------------------------------------

    use super::codex_role_string;

    #[test]
    fn writer_user_event_format() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "Hello from user".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        };
        let events = codex_events_for_message(&msg, "2026-02-09T06:07:08.000Z");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "event_msg");
        assert_eq!(events[0]["payload"]["type"], "user_message");
        assert_eq!(events[0]["payload"]["message"], "Hello from user");
    }

    #[test]
    fn writer_reasoning_event_format() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "Deep thought".to_string(),
            timestamp: None,
            author: Some("reasoning".to_string()),
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        };
        let events = codex_events_for_message(&msg, "2026-02-09T06:07:08.000Z");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "event_msg");
        assert_eq!(events[0]["payload"]["type"], "agent_reasoning");
        assert_eq!(events[0]["payload"]["text"], "Deep thought");
    }

    #[test]
    fn writer_codex_role_string_mapping() {
        assert_eq!(codex_role_string(&MessageRole::User), "user");
        assert_eq!(codex_role_string(&MessageRole::Assistant), "assistant");
        assert_eq!(codex_role_string(&MessageRole::Tool), "tool");
        assert_eq!(codex_role_string(&MessageRole::System), "developer");
        assert_eq!(
            codex_role_string(&MessageRole::Other("custom".to_string())),
            "custom"
        );
    }

    #[test]
    fn writer_assistant_without_token_count_produces_one_event() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "Simple reply".to_string(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!(null),
        };
        let events = codex_events_for_message(&msg, "2026-02-09T06:07:08.000Z");
        assert_eq!(
            events.len(),
            1,
            "Assistant without usage should produce one response_item"
        );
        assert_eq!(events[0]["type"], "response_item");
    }

    // -----------------------------------------------------------------------
    // Regression tests for cross-provider conversion bugs
    // -----------------------------------------------------------------------

    #[test]
    fn reader_function_call_output_classified_as_tool_role() {
        // `function_call_output` events have no `role` field. Before the fix they
        // defaulted to "assistant", placing tool results in an assistant turn which
        // the Anthropic API rejects. They must now produce a Tool-role message.
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"sx","cwd":"/tmp/p"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run something"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"done"}}"#,
        );
        let session = read_codex_jsonl(content);
        let tool_msg = session
            .messages
            .iter()
            .find(|m| !m.tool_results.is_empty())
            .expect("tool result message should exist");
        assert_eq!(
            tool_msg.role,
            MessageRole::Tool,
            "function_call_output must produce Tool role, not Assistant"
        );
    }

    #[test]
    fn reader_jsonl_compaction_resets_to_replacement_history() {
        // A `compacted` event replaces all prior history with its
        // replacement_history. Only that snapshot plus post-compaction events
        // should survive — the source agent's live context, not the full archive.
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"sx","cwd":"/tmp/p"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"PRE-COMPACTION ORIGINAL"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pre answer"}]}}"#,
            "\n",
            r#"{"type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"KEPT SUMMARY TASK"}]}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"POST answer"}]}}"#,
        );
        let session = read_codex_jsonl(content);
        let joined = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            !joined.contains("PRE-COMPACTION"),
            "pre-compaction history must be dropped; got: {joined}"
        );
        assert!(joined.contains("KEPT SUMMARY TASK"), "got: {joined}");
        assert!(joined.contains("POST answer"), "got: {joined}");
    }
}
